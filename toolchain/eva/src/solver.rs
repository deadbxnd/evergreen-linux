//! Dependency resolution and transaction planning.
//!
//! The resolver is a PubGrub-style backtracking solver: it picks the most
//! constrained outstanding requirement, tries candidate versions in preference
//! order, propagates dependencies and conflicts, and backtracks with an
//! explanation when a branch becomes unsatisfiable. Provides are normalized to
//! synthetic candidates, conflicts and replaces become incompatibilities, and
//! recommends are added in a second pass when they remain solvable.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::Result;
use evergreen_common::Version;
use serde::{Deserialize, Serialize};

use crate::config::{EvaConfig, Paths};
use crate::model::{
    normalize, Arch, Candidate, Condition, Dependency, InstallReason, InstalledPackage,
    PackageSpec, Request, UpgradeMode, VersionConstraint,
};

/// A package operation selected by the resolver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Install {
        name: String,
        version: Version,
    },
    Upgrade {
        name: String,
        from: Version,
        to: Version,
    },
    Remove {
        name: String,
    },
    Reinstall {
        name: String,
        version: Version,
    },
}

impl Action {
    pub fn name(&self) -> &str {
        match self {
            Self::Install { name, .. }
            | Self::Upgrade { name, .. }
            | Self::Remove { name }
            | Self::Reinstall { name, .. } => name,
        }
    }

    pub fn is_removal(&self) -> bool {
        matches!(self, Self::Remove { .. })
    }
}

/// An ordered set of actions to apply atomically.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionPlan {
    pub actions: Vec<Action>,
    /// Candidate metadata for the packages selected by the solve.
    #[serde(default)]
    pub assignments: BTreeMap<String, Candidate>,
    /// Packages explicitly requested by the user (rest are dependencies).
    #[serde(default)]
    pub explicit: BTreeSet<String>,
}

impl TransactionPlan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

impl fmt::Display for TransactionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for action in &self.actions {
            match action {
                Action::Install { name, version } => writeln!(f, "INSTALL {name} {version}")?,
                Action::Upgrade { name, from, to } => writeln!(f, "UPGRADE {name} {from} -> {to}")?,
                Action::Remove { name } => writeln!(f, "REMOVE  {name}")?,
                Action::Reinstall { name, version } => writeln!(f, "REINSTALL {name} {version}")?,
            }
        }
        Ok(())
    }
}

/// Human-readable explanation for an unsatisfiable request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictExplanation {
    pub package: String,
    pub detail: String,
    pub chain: Vec<String>,
    pub suggestions: Vec<String>,
}

impl fmt::Display for ConflictExplanation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Cannot resolve '{}': {}", self.package, self.detail)?;
        if !self.chain.is_empty() {
            writeln!(f, "  Dependency chain:")?;
            for item in &self.chain {
                writeln!(f, "    - {item}")?;
            }
        }
        if !self.suggestions.is_empty() {
            writeln!(f, "Suggestions:")?;
            for suggestion in &self.suggestions {
                writeln!(f, "  - {suggestion}")?;
            }
        }
        Ok(())
    }
}

/// Errors produced while resolving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NoSolution(ConflictExplanation),
    UnknownPackage(String),
    AmbiguousVirtual {
        name: String,
        providers: Vec<String>,
    },
    PinnedConflict {
        name: String,
        pin: String,
        available: Vec<Version>,
    },
    HeldPackage(String),
    ArchMismatch {
        name: String,
        requested: Arch,
        available: Vec<Arch>,
    },
    FileConflict {
        path: String,
        packages: Vec<String>,
    },
    InsufficientSpace {
        required: u64,
        available: u64,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSolution(explanation) => write!(f, "{explanation}"),
            Self::UnknownPackage(name) => write!(f, "unknown package: {name}"),
            Self::AmbiguousVirtual { name, providers } => write!(
                f,
                "virtual package {name} has no usable provider (candidates: {})",
                providers.join(", ")
            ),
            Self::PinnedConflict {
                name,
                pin,
                available,
            } => write!(
                f,
                "package {name} is pinned to {pin}; available versions: {}",
                available
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::HeldPackage(name) => write!(f, "package {name} is held"),
            Self::ArchMismatch {
                name,
                requested,
                available,
            } => write!(
                f,
                "package {name} has no {requested} build (available: {})",
                available
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::FileConflict { path, packages } => {
                write!(f, "file conflict on {path} between {}", packages.join(", "))
            }
            Self::InsufficientSpace {
                required,
                available,
            } => write!(
                f,
                "insufficient disk space: need {required} bytes, have {available}"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<Failure> for ResolveError {
    fn from(failure: Failure) -> Self {
        Self::NoSolution(ConflictExplanation {
            package: failure.package,
            detail: failure.detail,
            chain: failure.chain,
            suggestions: Vec::new(),
        })
    }
}

/// A requirement still to be satisfied during solving.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Requirement {
    name: String,
    constraint: VersionConstraint,
    source: String,
}

impl Requirement {
    fn display(&self) -> String {
        if self.source == "__root__" {
            format!("requested {} {}", self.name, self.constraint)
        } else {
            format!("{} requires {} {}", self.source, self.name, self.constraint)
        }
    }
}

#[derive(Debug, Clone)]
struct Failure {
    package: String,
    detail: String,
    chain: Vec<String>,
}

#[derive(Debug, Default, Clone)]
struct SolveState {
    assignments: BTreeMap<String, Candidate>,
    reasons: BTreeMap<String, String>,
}

/// Resolves requests against available and installed packages.
#[derive(Debug, Clone)]
pub struct Resolver {
    pub candidates: BTreeMap<String, Vec<Candidate>>,
    pub installed: BTreeMap<String, InstalledPackage>,
    pub features: BTreeMap<String, bool>,
    pub holds: BTreeSet<String>,
    pub pins: BTreeMap<String, VersionConstraint>,
    pub config: EvaConfig,
    /// Root path used only for free-space validation.
    pub root: std::path::PathBuf,
}

impl Default for Resolver {
    fn default() -> Self {
        Self {
            candidates: BTreeMap::new(),
            installed: BTreeMap::new(),
            features: BTreeMap::new(),
            holds: BTreeSet::new(),
            pins: BTreeMap::new(),
            config: EvaConfig::default(),
            root: std::path::PathBuf::from("/"),
        }
    }
}

#[derive(Debug, Clone)]
struct Index {
    all: Vec<Candidate>,
}

impl Resolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add candidates from repository indexes.
    pub fn add_candidates(&mut self, candidates: impl IntoIterator<Item = Candidate>) {
        for candidate in candidates {
            self.candidates
                .entry(candidate.id.name.clone())
                .or_default()
                .push(candidate);
        }
    }

    /// Replace the installed set.
    pub fn set_installed(&mut self, installed: impl IntoIterator<Item = InstalledPackage>) {
        self.installed = installed
            .into_iter()
            .map(|package| (package.candidate.id.name.clone(), package))
            .collect();
    }

    pub fn set_features(&mut self, features: BTreeMap<String, bool>) {
        self.features = features;
    }

    pub fn set_holds(&mut self, holds: BTreeSet<String>) {
        self.holds = holds;
    }

    pub fn set_pins(&mut self, pins: BTreeMap<String, VersionConstraint>) {
        self.pins = pins;
    }

    /// Compatibility helper for simple install requests.
    pub fn resolve_install(&self, requests: &[String]) -> Result<TransactionPlan, ResolveError> {
        let specs = requests
            .iter()
            .map(|name| PackageSpec::parse(name))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|message| {
                ResolveError::NoSolution(ConflictExplanation {
                    package: requests.first().cloned().unwrap_or_default(),
                    detail: message.to_string(),
                    chain: Vec::new(),
                    suggestions: Vec::new(),
                })
            })?;
        self.resolve(&Request {
            install: specs,
            ..Request::default()
        })
    }

    /// Full resolve pipeline.
    pub fn resolve(&self, request: &Request) -> Result<TransactionPlan, ResolveError> {
        let index = self.build_index(request);
        let requirements = self.root_requirements(request)?;
        let removed: BTreeSet<String> = request.remove.iter().map(|name| normalize(name)).collect();

        let mut state = SolveState::default();
        self.solve(&requirements, &index, &removed, &mut state, 0)?;

        if self.config.install_recommends {
            self.add_recommends(&index, &removed, &mut state)?;
        }

        self.validate_assignment(&state, &removed)?;
        let mut plan = self.diff(&state, request);
        Self::order_actions(&mut plan.actions, &state.assignments, &self.installed);
        self.validate_plan(&plan, &state)?;
        Ok(plan)
    }

    fn build_index(&self, request: &Request) -> Index {
        let removed: BTreeSet<String> = request.remove.iter().map(|name| normalize(name)).collect();
        let mut all: Vec<Candidate> = self
            .candidates
            .values()
            .flatten()
            .cloned()
            .chain(
                self.installed
                    .values()
                    .map(|package| package.candidate.clone()),
            )
            .collect();

        all.retain(|candidate| !removed.contains(&candidate.id.name));

        if !self.pins.is_empty() {
            all.retain(|candidate| match self.pins.get(&candidate.id.name) {
                Some(pin) => pin.matches(&candidate.id.version),
                None => true,
            });
        }

        if !self.holds.is_empty() {
            all.retain(|candidate| {
                if !self.holds.contains(&candidate.id.name) {
                    return true;
                }
                self.installed
                    .get(&candidate.id.name)
                    .map(|installed| installed.candidate.id.version == candidate.id.version)
                    .unwrap_or(true)
            });
        }

        all.sort_by(|a, b| {
            b.id.version
                .cmp(&a.id.version)
                .then(a.id.repo.0.cmp(&b.id.repo.0))
                .then(a.id.name.cmp(&b.id.name))
        });
        all.dedup_by(|a, b| a.id.key() == b.id.key());

        Index { all }
    }

    fn root_requirements(&self, request: &Request) -> Result<Vec<Requirement>, ResolveError> {
        let mut requirements = Vec::new();

        for spec in &request.install {
            requirements.push(Requirement {
                name: normalize(&spec.name),
                constraint: spec.constraint.clone(),
                source: "__root__".to_string(),
            });
        }

        let upgraded: BTreeSet<String> = match &request.upgrade {
            UpgradeMode::None => BTreeSet::new(),
            UpgradeMode::All => self
                .installed
                .keys()
                .filter(|name| !self.holds.contains(*name))
                .cloned()
                .collect(),
            UpgradeMode::Specific(names) => names.iter().map(|name| normalize(name)).collect(),
        };

        for name in &upgraded {
            match self.installed.get(name) {
                Some(package) => requirements.push(Requirement {
                    name: name.clone(),
                    constraint: VersionConstraint::at_least(&package.candidate.id.version),
                    source: "__root__".to_string(),
                }),
                None => requirements.push(Requirement {
                    name: name.clone(),
                    constraint: VersionConstraint::Any,
                    source: "__root__".to_string(),
                }),
            }
        }

        for name in &request.reinstall {
            let name = normalize(name);
            let version = self
                .installed
                .get(&name)
                .map(|package| package.candidate.id.version.clone())
                .ok_or_else(|| ResolveError::UnknownPackage(name.clone()))?;
            requirements.push(Requirement {
                name,
                constraint: VersionConstraint::Eq(version),
                source: "__root__".to_string(),
            });
        }

        // Keep previously explicit packages installed at their current version.
        for (name, package) in &self.installed {
            if package.reason != InstallReason::Explicit || upgraded.contains(name) {
                continue;
            }
            if request.remove.contains(name) {
                continue;
            }
            requirements.push(Requirement {
                name: name.clone(),
                constraint: VersionConstraint::Eq(package.candidate.id.version.clone()),
                source: "__root__".to_string(),
            });
        }

        Ok(requirements)
    }

    fn options_for(
        &self,
        requirement: &Requirement,
        index: &Index,
        state: &SolveState,
        removed: &BTreeSet<String>,
    ) -> Vec<Candidate> {
        let mut options = Vec::new();
        for candidate in &index.all {
            if removed.contains(&candidate.id.name) {
                continue;
            }
            let Some(version) = candidate.provided_version(&requirement.name) else {
                continue;
            };
            if !requirement.constraint.matches(version) {
                continue;
            }
            if !self.candidate_fits(candidate, state) {
                continue;
            }
            if options
                .iter()
                .any(|other: &Candidate| other.id.key() == candidate.id.key())
            {
                continue;
            }
            options.push(candidate.clone());
        }
        options
    }

    fn candidate_fits(&self, candidate: &Candidate, state: &SolveState) -> bool {
        if let Some(existing) = state.assignments.get(&candidate.id.name) {
            return existing.id.key() == candidate.id.key();
        }

        for (name, assigned) in &state.assignments {
            if name == &candidate.id.name {
                continue;
            }
            if conflicts(candidate, assigned) || conflicts(assigned, candidate) {
                return false;
            }
            // A replacement is allowed to displace a non-root package.
            if candidate.replaces.contains(name) && !assigned.replaces.contains(&candidate.id.name)
            {
                continue;
            }
        }
        true
    }

    fn solve(
        &self,
        requirements: &[Requirement],
        index: &Index,
        removed: &BTreeSet<String>,
        state: &mut SolveState,
        depth: usize,
    ) -> Result<(), Failure> {
        if requirements.is_empty() {
            return Ok(());
        }
        if depth > 256 {
            return Err(Failure {
                package: requirements[0].name.clone(),
                detail: "dependency chain too deep".to_string(),
                chain: requirements.iter().map(Requirement::display).collect(),
            });
        }

        // Minimum-remaining-values: try the hardest requirement first.
        let mut choices: Vec<(Vec<Candidate>, Requirement)> = requirements
            .iter()
            .map(|requirement| {
                (
                    self.options_for(requirement, index, state, removed),
                    requirement.clone(),
                )
            })
            .collect();
        choices.sort_by_key(|(options, _)| options.len());
        let (options, requirement) = choices.remove(0);

        if options.is_empty() {
            return Err(self.explain_no_options(&requirement, index, state));
        }

        let mut first_failure: Option<Failure> = None;
        for candidate in options {
            let name = candidate.id.name.clone();
            let already_assigned = state.assignments.contains_key(&name);

            let mut trial = state.clone();
            if !already_assigned {
                trial.assignments.insert(name.clone(), candidate.clone());
                trial.reasons.insert(name.clone(), requirement.display());
            }

            let mut next_requirements = Vec::new();
            for dependency in &candidate.deps {
                next_requirements.push(Requirement {
                    name: dependency.name.clone(),
                    constraint: dependency.constraint.clone(),
                    source: name.clone(),
                });
            }
            for optional in &candidate.opt_deps {
                if self.condition_active(&optional.condition, &trial, removed) {
                    next_requirements.push(Requirement {
                        name: optional.dependency.name.clone(),
                        constraint: optional.dependency.constraint.clone(),
                        source: name.clone(),
                    });
                }
            }

            // Requirements already satisfied by this assignment need no work.
            next_requirements.retain(|next| {
                if let Some(assigned) = trial.assignments.get(&next.name) {
                    return assigned
                        .provided_version(&next.name)
                        .map_or(true, |version| !next.constraint.matches(version));
                }
                true
            });

            let rest: Vec<Requirement> = requirements
                .iter()
                .filter(|current| **current != requirement)
                .cloned()
                .collect();
            next_requirements.extend(rest);

            match self.solve(&next_requirements, index, removed, &mut trial, depth + 1) {
                Ok(()) => {
                    *state = trial;
                    return Ok(());
                }
                Err(failure) => {
                    if first_failure.is_none() {
                        first_failure = Some(failure);
                    }
                }
            }
        }

        Err(first_failure.unwrap_or_else(|| self.explain_no_options(&requirement, index, state)))
    }

    fn condition_active(
        &self,
        condition: &Condition,
        state: &SolveState,
        removed: &BTreeSet<String>,
    ) -> bool {
        match condition {
            Condition::Always => true,
            Condition::Feature(name) => self.features.get(name).copied().unwrap_or(false),
            Condition::PackagePresent(name) => {
                !removed.contains(name)
                    && (state.assignments.contains_key(name) || self.installed.contains_key(name))
            }
        }
    }

    fn explain_no_options(
        &self,
        requirement: &Requirement,
        index: &Index,
        state: &SolveState,
    ) -> Failure {
        let known: Vec<String> = index
            .all
            .iter()
            .filter(|candidate| candidate.satisfied_names().contains(&requirement.name))
            .map(|candidate| format!("{} {}", candidate.id.name, candidate.id.version))
            .collect();

        let detail = if known.is_empty() {
            format!("no available candidate provides {}", requirement.name)
        } else {
            format!(
                "no version of {} satisfies {}: available {}",
                requirement.name,
                requirement.constraint,
                known.join(", ")
            )
        };

        let mut chain: Vec<String> = state.reasons.values().cloned().collect();
        chain.push(requirement.display());
        chain.sort();
        chain.dedup();

        Failure {
            package: requirement.name.clone(),
            detail,
            chain,
        }
    }

    fn add_recommends(
        &self,
        index: &Index,
        removed: &BTreeSet<String>,
        state: &mut SolveState,
    ) -> Result<(), ResolveError> {
        let mut recommendations = Vec::new();
        for candidate in state.assignments.values() {
            for recommend in &candidate.recommends {
                recommendations.push((candidate.id.name.clone(), recommend.clone()));
            }
        }
        recommendations.sort_by(|a, b| a.1.name.cmp(&b.1.name));

        for (source, recommend) in recommendations {
            if state.assignments.values().any(|candidate| {
                candidate
                    .provided_version(&recommend.name)
                    .is_some_and(|version| recommend.constraint.matches(version))
            }) {
                continue;
            }
            let requirement = Requirement {
                name: recommend.name.clone(),
                constraint: recommend.constraint.clone(),
                source,
            };
            let mut trial = state.clone();
            match self.solve(&[requirement], index, removed, &mut trial, 0) {
                Ok(()) => *state = trial,
                Err(_) => continue,
            }
        }
        Ok(())
    }

    fn validate_assignment(
        &self,
        state: &SolveState,
        removed: &BTreeSet<String>,
    ) -> Result<(), ResolveError> {
        for candidate in state.assignments.values() {
            for dependency in &candidate.deps {
                let satisfied = state.assignments.values().any(|other| {
                    other
                        .provided_version(&dependency.name)
                        .is_some_and(|version| dependency.constraint.matches(version))
                });
                if !satisfied {
                    return Err(ResolveError::NoSolution(ConflictExplanation {
                        package: dependency.name.clone(),
                        detail: format!(
                            "dependency of {} is not satisfied in the final solution",
                            candidate.id.name
                        ),
                        chain: vec![format!(
                            "{} requires {} {}",
                            candidate.id.name, dependency.name, dependency.constraint
                        )],
                        suggestions: Vec::new(),
                    }));
                }
            }
            for optional in &candidate.opt_deps {
                if !self.condition_active(&optional.condition, state, removed) {
                    continue;
                }
                let satisfied = state.assignments.values().any(|other| {
                    other
                        .provided_version(&optional.dependency.name)
                        .is_some_and(|version| optional.dependency.constraint.matches(version))
                });
                if !satisfied {
                    return Err(ResolveError::NoSolution(ConflictExplanation {
                        package: optional.dependency.name.clone(),
                        detail: format!(
                            "active optional dependency of {} is not satisfied",
                            candidate.id.name
                        ),
                        chain: Vec::new(),
                        suggestions: Vec::new(),
                    }));
                }
            }
        }
        Ok(())
    }

    fn diff(&self, state: &SolveState, request: &Request) -> TransactionPlan {
        let mut actions = Vec::new();
        let reinstalls: BTreeSet<String> = request
            .reinstall
            .iter()
            .map(|name| normalize(name))
            .collect();

        for (name, candidate) in &state.assignments {
            match self.installed.get(name) {
                Some(installed) => {
                    if installed.candidate.id.version == candidate.id.version {
                        if reinstalls.contains(name) {
                            actions.push(Action::Reinstall {
                                name: name.clone(),
                                version: candidate.id.version.clone(),
                            });
                        }
                    } else {
                        actions.push(Action::Upgrade {
                            name: name.clone(),
                            from: installed.candidate.id.version.clone(),
                            to: candidate.id.version.clone(),
                        });
                    }
                }
                None => actions.push(Action::Install {
                    name: name.clone(),
                    version: candidate.id.version.clone(),
                }),
            }
        }

        let replaced: BTreeSet<String> = state
            .assignments
            .values()
            .flat_map(|candidate| candidate.replaces.iter().cloned())
            .collect();
        let removed: BTreeSet<String> = request.remove.iter().map(|name| normalize(name)).collect();

        for (name, installed) in &self.installed {
            if state.assignments.contains_key(name) && !replaced.contains(name) {
                continue;
            }
            if removed.contains(name)
                || replaced.contains(name)
                || installed.reason == InstallReason::Dependency
            {
                actions.push(Action::Remove { name: name.clone() });
            }
        }

        let mut explicit: BTreeSet<String> = request
            .install
            .iter()
            .map(|spec| normalize(&spec.name))
            .collect();
        explicit.extend(request.reinstall.iter().map(|name| normalize(name)));
        match &request.upgrade {
            UpgradeMode::All => {
                explicit.extend(
                    self.installed
                        .iter()
                        .filter(|(_, package)| package.reason == InstallReason::Explicit)
                        .map(|(name, _)| name.clone()),
                );
            }
            UpgradeMode::Specific(names) => {
                explicit.extend(names.iter().map(|name| normalize(name)));
            }
            UpgradeMode::None => {}
        }

        TransactionPlan {
            actions,
            assignments: state.assignments.clone(),
            explicit,
        }
    }

    fn order_actions(
        actions: &mut Vec<Action>,
        assignments: &BTreeMap<String, Candidate>,
        installed: &BTreeMap<String, InstalledPackage>,
    ) {
        let names: BTreeSet<String> = actions
            .iter()
            .map(|action| action.name().to_string())
            .collect();
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut indegree: BTreeMap<String, usize> =
            names.iter().map(|name| (name.clone(), 0)).collect();

        let mut add_edge = |from: &str, to: &str| {
            if from == to || !names.contains(from) || !names.contains(to) {
                return;
            }
            if edges
                .entry(from.to_string())
                .or_default()
                .insert(to.to_string())
            {
                *indegree.entry(to.to_string()).or_insert(0) += 1;
            }
        };

        // Installs and upgrades come after their dependencies.
        for action in actions.iter().filter(|action| !action.is_removal()) {
            let candidate = assignments.get(action.name());
            let deps: Vec<Dependency> = candidate
                .map(|candidate| candidate.deps.clone())
                .unwrap_or_default();
            for dependency in deps {
                let provider = assignments
                    .values()
                    .find(|other| {
                        other
                            .provided_version(&dependency.name)
                            .is_some_and(|version| dependency.constraint.matches(version))
                    })
                    .map(|other| other.id.name.clone());
                if let Some(provider) = provider {
                    add_edge(&provider, action.name());
                }
            }
        }

        // Removals run after their dependents are gone.
        for action in actions.iter().filter(|action| action.is_removal()) {
            if let Some(package) = installed.get(action.name()) {
                for dependency in &package.candidate.deps {
                    if names.contains(&dependency.name) {
                        add_edge(action.name(), &dependency.name);
                    }
                }
            }
        }

        let mut ready: BTreeSet<String> = indegree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(name, _)| name.clone())
            .collect();
        let mut ordered = Vec::new();
        while let Some(name) = ready.iter().next().cloned() {
            ready.remove(&name);
            ordered.push(name.clone());
            if let Some(children) = edges.get(&name) {
                for child in children {
                    if let Some(degree) = indegree.get_mut(child) {
                        *degree -= 1;
                        if *degree == 0 {
                            ready.insert(child.clone());
                        }
                    }
                }
            }
        }

        if ordered.len() == names.len() {
            // Kernel-related packages are applied last among installs so the
            // bootloader hook sees a consistent filesystem.
            let mut sorted: Vec<(usize, Action)> = actions
                .drain(..)
                .map(|action| {
                    let rank = if !action.is_removal()
                        && (action.name().contains("kernel") || action.name().starts_with("linux"))
                    {
                        1
                    } else {
                        0
                    };
                    (rank, action)
                })
                .collect();
            sorted.sort_by_key(|(rank, action)| {
                let position = ordered
                    .iter()
                    .position(|name| name == action.name())
                    .unwrap_or(usize::MAX);
                (*rank, position)
            });
            *actions = sorted.into_iter().map(|(_, action)| action).collect();
        }
    }

    fn validate_plan(
        &self,
        plan: &TransactionPlan,
        state: &SolveState,
    ) -> Result<(), ResolveError> {
        let removed: BTreeSet<String> = plan
            .actions
            .iter()
            .filter(|action| action.is_removal())
            .map(|action| action.name().to_string())
            .collect();
        let replaced: BTreeSet<String> = state
            .assignments
            .values()
            .flat_map(|candidate| candidate.replaces.iter().cloned())
            .collect();

        let mut owners: BTreeMap<String, String> = BTreeMap::new();
        for (name, package) in &self.installed {
            if removed.contains(name) || replaced.contains(name) {
                continue;
            }
            for file in &package.candidate.files {
                owners.insert(file.path.clone(), name.clone());
            }
            for link in &package.candidate.symlinks {
                owners.insert(link.path.clone(), name.clone());
            }
        }

        for action in &plan.actions {
            let candidate = match state.assignments.get(action.name()) {
                Some(candidate) => candidate,
                None => continue,
            };
            for file in &candidate.files {
                if let Some(owner) = owners.get(&file.path) {
                    if owner != &candidate.id.name
                        && !candidate.replaces.contains(owner)
                        && !removed.contains(owner)
                    {
                        return Err(ResolveError::FileConflict {
                            path: file.path.clone(),
                            packages: vec![owner.clone(), candidate.id.name.clone()],
                        });
                    }
                }
                owners.insert(file.path.clone(), candidate.id.name.clone());
            }
            for link in &candidate.symlinks {
                if let Some(owner) = owners.get(&link.path) {
                    if owner != &candidate.id.name
                        && !candidate.replaces.contains(owner)
                        && !removed.contains(owner)
                    {
                        return Err(ResolveError::FileConflict {
                            path: link.path.clone(),
                            packages: vec![owner.clone(), candidate.id.name.clone()],
                        });
                    }
                }
                owners.insert(link.path.clone(), candidate.id.name.clone());
            }
        }

        let mut required: u64 = 0;
        for action in &plan.actions {
            match action {
                Action::Install { name, .. } | Action::Reinstall { name, .. } => {
                    required += state
                        .assignments
                        .get(name)
                        .map(|candidate| candidate.meta.size_installed)
                        .unwrap_or(0);
                }
                Action::Upgrade { name, .. } => {
                    let candidate_size = state
                        .assignments
                        .get(name)
                        .map(|candidate| candidate.meta.size_installed)
                        .unwrap_or(0);
                    let installed_size = self
                        .installed
                        .get(name)
                        .map(|package| package.candidate.meta.size_installed)
                        .unwrap_or(0);
                    required += candidate_size.saturating_sub(installed_size);
                }
                Action::Remove { .. } => {}
            }
        }

        if required > 0 {
            if let Some(available) = available_space(&self.root) {
                if required > available {
                    return Err(ResolveError::InsufficientSpace {
                        required,
                        available,
                    });
                }
            }
        }

        Ok(())
    }
}

fn conflicts(a: &Candidate, b: &Candidate) -> bool {
    a.conflicts
        .iter()
        .any(|conflict| conflict.name == b.id.name && conflict.constraint.matches(&b.id.version))
}

fn available_space(root: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(root.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if result == 0 {
        Some(stat.f_bavail.saturating_mul(stat.f_frsize))
    } else {
        None
    }
}

/// Convenience function used by the CLI to build a resolver from repositories.
pub fn resolver_from_indexes(
    paths: &Paths,
    candidates: Vec<Candidate>,
    installed: Vec<InstalledPackage>,
    config: &EvaConfig,
) -> Resolver {
    let mut resolver = Resolver::new();
    resolver.add_candidates(candidates);
    resolver.set_installed(installed);
    resolver.config = config.clone();
    resolver.root = paths.root.clone();
    resolver
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arch, Conflict, FileEntry, PackageId, PackageMeta, Provide};
    use evergreen_common::RepoId;

    fn candidate(
        name: &str,
        version: &str,
        deps: &[(&str, &str)],
        conflicts: &[(&str, &str)],
        provides: &[(&str, &str)],
    ) -> Candidate {
        Candidate {
            id: PackageId {
                name: name.to_string(),
                version: Version::parse(version).unwrap(),
                arch: Arch::NoArch,
                repo: RepoId(0),
            },
            meta: PackageMeta {
                size_installed: 10,
                ..PackageMeta::default()
            },
            deps: deps
                .iter()
                .map(|(name, constraint)| Dependency::required(*name, constraint.parse().unwrap()))
                .collect(),
            opt_deps: Vec::new(),
            recommends: Vec::new(),
            suggests: Vec::new(),
            conflicts: conflicts
                .iter()
                .map(|(name, constraint)| Conflict {
                    name: (*name).to_string(),
                    constraint: constraint.parse().unwrap(),
                })
                .collect(),
            provides: provides
                .iter()
                .map(|(name, version)| Provide {
                    name: (*name).to_string(),
                    version: if *version == "*" {
                        None
                    } else {
                        Some(Version::parse(version).unwrap())
                    },
                })
                .collect(),
            replaces: Vec::new(),
            files: vec![FileEntry {
                path: format!("usr/share/{name}"),
                sha256: String::new(),
                mode: 0o644,
            }],
            symlinks: Vec::new(),
            hooks: Default::default(),
            archive: String::new(),
        }
    }

    fn resolver(candidates: Vec<Candidate>) -> Resolver {
        let mut resolver = Resolver::new();
        resolver.add_candidates(candidates);
        resolver.config.install_recommends = false;
        resolver
    }

    #[test]
    fn installs_dependencies_before_dependents() {
        let resolver = resolver(vec![
            candidate("app", "1.0", &[("lib", ">=1.0")], &[], &[]),
            candidate("lib", "1.0", &[], &[], &[]),
            candidate("lib", "1.1", &[], &[], &[]),
        ]);
        let plan = resolver
            .resolve_install(&["app".to_string()])
            .expect("solvable");
        let names: Vec<&str> = plan.actions.iter().map(Action::name).collect();
        assert_eq!(names, vec!["lib", "app"]);
        assert!(matches!(
            &plan.actions[0],
            Action::Install { version, .. } if version == &Version::parse("1.1").unwrap()
        ));
    }

    #[test]
    fn reports_unknown_package() {
        let resolver = resolver(vec![]);
        assert!(matches!(
            resolver.resolve_install(&["missing".to_string()]),
            Err(ResolveError::NoSolution(_))
        ));
    }

    #[test]
    fn resolves_virtual_providers() {
        let resolver = resolver(vec![
            candidate("app", "1.0", &[("browser", ">=1.0")], &[], &[]),
            candidate("firefox", "1.0", &[], &[], &[("browser", "1.0")]),
        ]);
        let plan = resolver.resolve_install(&["app".to_string()]).unwrap();
        let names: Vec<&str> = plan.actions.iter().map(Action::name).collect();
        assert_eq!(names, vec!["firefox", "app"]);
    }

    #[test]
    fn backtracks_around_conflicts() {
        let resolver = resolver(vec![
            candidate("app", "1.0", &[("lib", ">=1.0")], &[], &[]),
            candidate("lib", "2.0", &[], &[("old", "*")], &[]),
            candidate("lib", "1.0", &[], &[], &[]),
            candidate("old", "1.0", &[], &[], &[]),
        ]);
        let plan = resolver
            .resolve(&Request {
                install: vec![
                    PackageSpec::parse("app").unwrap(),
                    PackageSpec::parse("old").unwrap(),
                ],
                ..Request::default()
            })
            .unwrap();
        let lib = plan
            .actions
            .iter()
            .find(|action| action.name() == "lib")
            .unwrap();
        assert!(matches!(lib, Action::Install { version, .. } if version.to_string() == "1.0"));
    }

    #[test]
    fn upgrades_installed_package() {
        let mut resolver = resolver(vec![candidate("app", "2.0", &[], &[], &[])]);
        resolver.set_installed([InstalledPackage {
            candidate: candidate("app", "1.0", &[], &[], &[]),
            reason: InstallReason::Explicit,
            installed_at: "now".to_string(),
        }]);
        let plan = resolver
            .resolve(&Request {
                upgrade: UpgradeMode::All,
                ..Request::default()
            })
            .unwrap();
        assert!(matches!(
            &plan.actions[0],
            Action::Upgrade { from, to, .. }
                if from.to_string() == "1.0" && to.to_string() == "2.0"
        ));
    }

    #[test]
    fn removes_orphaned_dependencies() {
        let mut resolver = resolver(vec![candidate("app", "1.0", &[], &[], &[])]);
        resolver.set_installed([
            InstalledPackage {
                candidate: candidate("app", "1.0", &[("lib", "*")], &[], &[]),
                reason: InstallReason::Explicit,
                installed_at: "now".to_string(),
            },
            InstalledPackage {
                candidate: candidate("lib", "1.0", &[], &[], &[]),
                reason: InstallReason::Dependency,
                installed_at: "now".to_string(),
            },
        ]);
        let plan = resolver
            .resolve(&Request {
                remove: vec!["app".to_string()],
                ..Request::default()
            })
            .unwrap();
        let names: Vec<&str> = plan.actions.iter().map(Action::name).collect();
        assert_eq!(names, vec!["app", "lib"]);
    }

    #[test]
    fn optional_deps_follow_feature_flags() {
        let mut app = candidate("app", "1.0", &[], &[], &[]);
        app.opt_deps.push(crate::model::OptionalDep {
            dependency: crate::model::Dependency::required("extra", "*".parse().unwrap()),
            condition: Condition::Feature("media".to_string()),
        });
        let extra = candidate("extra", "1.0", &[], &[], &[]);
        let mut resolver = resolver(vec![app, extra]);
        resolver.set_features([("media".to_string(), true)].into_iter().collect());
        let plan = resolver.resolve_install(&["app".into()]).unwrap();
        let names: Vec<&str> = plan.actions.iter().map(Action::name).collect();
        assert!(names.contains(&"extra"), "optional dependency was skipped");
    }

    #[test]
    fn recommends_are_added_when_configured() {
        let mut app = candidate("app", "1.0", &[], &[], &[]);
        app.recommends.push(crate::model::Dependency {
            name: "suggested".to_string(),
            constraint: VersionConstraint::Any,
            kind: crate::model::DepKind::Recommend,
        });
        let suggested = candidate("suggested", "1.0", &[], &[], &[]);

        let mut with = resolver(vec![app.clone(), suggested.clone()]);
        with.config.install_recommends = true;
        let plan = with.resolve_install(&["app".into()]).unwrap();
        assert!(plan
            .actions
            .iter()
            .any(|action| action.name() == "suggested"));

        let mut without = resolver(vec![app, suggested]);
        without.config.install_recommends = false;
        let plan = without.resolve_install(&["app".into()]).unwrap();
        assert!(!plan
            .actions
            .iter()
            .any(|action| action.name() == "suggested"));
    }

    #[test]
    fn holds_and_pins_prevent_upgrades() {
        let installed = InstalledPackage {
            candidate: candidate("app", "1.0", &[], &[], &[]),
            reason: InstallReason::Explicit,
            installed_at: "now".to_string(),
        };
        let available = candidate("app", "2.0", &[], &[], &[]);

        let mut held = resolver(vec![available.clone()]);
        held.set_installed([installed.clone()]);
        held.set_holds(["app".to_string()].into_iter().collect());
        assert!(held
            .resolve(&Request {
                upgrade: UpgradeMode::All,
                ..Request::default()
            })
            .unwrap()
            .is_empty());

        let mut pinned = resolver(vec![available]);
        pinned.set_installed([installed]);
        pinned.set_pins(
            [(
                "app".to_string(),
                VersionConstraint::Eq(Version::parse("1.0").unwrap()),
            )]
            .into_iter()
            .collect(),
        );
        assert!(pinned
            .resolve(&Request {
                upgrade: UpgradeMode::All,
                ..Request::default()
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn detects_file_conflicts() {
        let mut first = candidate("first", "1.0", &[], &[], &[]);
        let mut second = candidate("second", "1.0", &[], &[], &[]);
        first.files[0].path = "usr/bin/shared".to_string();
        second.files[0].path = "usr/bin/shared".to_string();
        let resolver = resolver(vec![first, second]);
        let result = resolver.resolve_install(&["first".into(), "second".into()]);
        assert!(matches!(result, Err(ResolveError::FileConflict { .. })));
    }
}
