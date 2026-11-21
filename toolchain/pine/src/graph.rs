//! Dependency graph construction and transaction ordering.

use std::collections::{BTreeMap, BTreeSet};

use crate::unit::{Unit, UnitKind};

/// Errors from graph construction and resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    UnknownUnit(String),
    MissingRequirement { unit: String, requires: String },
    Cycle(Vec<String>),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownUnit(name) => write!(f, "unknown unit: {name}"),
            Self::MissingRequirement { unit, requires } => {
                write!(f, "{unit} requires missing unit {requires}")
            }
            Self::Cycle(units) => write!(f, "dependency cycle: {}", units.join(" -> ")),
        }
    }
}

impl std::error::Error for GraphError {}

/// Resolve the units needed to reach `target`, returned in startup order.
///
/// Requirement edges (`Wants`, `Requires`, `BindsTo`) pull units into the
/// transaction. Ordering edges (`After`, `Before`) constrain the order.
pub fn transaction(
    units: &BTreeMap<String, Unit>,
    target: &str,
) -> Result<Vec<String>, GraphError> {
    if !units.contains_key(target) {
        return Err(GraphError::UnknownUnit(target.to_string()));
    }

    // Collect the dependency closure.
    let mut closure: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = vec![target.to_string()];

    while let Some(name) = queue.pop() {
        if !closure.insert(name.clone()) {
            continue;
        }
        let Some(unit) = units.get(&name) else {
            continue;
        };

        for dependency in unit
            .config
            .wants
            .iter()
            .chain(unit.config.requires.iter())
            .chain(unit.config.binds_to.iter())
        {
            if units.contains_key(dependency) {
                if !closure.contains(dependency) {
                    queue.push(dependency.clone());
                }
            } else if unit.config.requires.contains(dependency)
                || unit.config.binds_to.contains(dependency)
            {
                return Err(GraphError::MissingRequirement {
                    unit: name,
                    requires: dependency.clone(),
                });
            }
            // Missing `Wants` are ignored (they are soft dependencies).
        }
    }

    // Build ordering edges within the closure. Edge `a -> b` means a starts before b.
    let mut adjacency: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut indegree: BTreeMap<String, usize> =
        closure.iter().map(|name| (name.clone(), 0)).collect();

    let add_edge = |from: &str,
                    to: &str,
                    adjacency: &mut BTreeMap<String, BTreeSet<String>>,
                    indegree: &mut BTreeMap<String, usize>| {
        if from == to {
            return;
        }
        if adjacency
            .entry(from.to_string())
            .or_default()
            .insert(to.to_string())
        {
            *indegree.entry(to.to_string()).or_insert(0) += 1;
        }
    };

    for name in &closure {
        let Some(unit) = units.get(name) else {
            continue;
        };
        for after in &unit.config.after {
            if closure.contains(after) {
                add_edge(after, name, &mut adjacency, &mut indegree);
            }
        }
        for before in &unit.config.before {
            if closure.contains(before) {
                add_edge(name, before, &mut adjacency, &mut indegree);
            }
        }
        // Requirements are hard, so start them first even without explicit
        // `After=` (pine executes transactions sequentially).
        for dependency in unit
            .config
            .requires
            .iter()
            .chain(unit.config.binds_to.iter())
        {
            if closure.contains(dependency) {
                add_edge(dependency, name, &mut adjacency, &mut indegree);
            }
        }
        // Targets implicitly order themselves after the units they pull in, so
        // "reached target" is only reported once its services are up.
        if unit.config.is_target() {
            for dependency in unit
                .config
                .wants
                .iter()
                .chain(unit.config.requires.iter())
                .chain(unit.config.binds_to.iter())
            {
                if closure.contains(dependency) {
                    add_edge(dependency, name, &mut adjacency, &mut indegree);
                }
            }
        }
    }

    // Kahn's algorithm with deterministic tie-breaking.
    let mut ready: BTreeSet<String> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(name, _)| name.clone())
        .collect();

    let mut order = Vec::with_capacity(closure.len());
    while let Some(name) = ready.iter().next().cloned() {
        ready.remove(&name);
        order.push(name.clone());
        if let Some(children) = adjacency.get(&name) {
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

    if order.len() != closure.len() {
        let remaining: Vec<String> = closure
            .iter()
            .filter(|name| !order.contains(name))
            .cloned()
            .collect();
        return Err(GraphError::Cycle(remaining));
    }

    Ok(order)
}

/// Reverse order for stopping units (dependents before dependencies).
pub fn stop_order(order: &[String]) -> Vec<String> {
    order.iter().rev().cloned().collect()
}

/// Whether a unit is a target.
pub fn is_target(unit: &Unit) -> bool {
    unit.config.kind == UnitKind::Target
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::unit::Unit;

    fn unit(name: &str, body: &str) -> Unit {
        let value: toml::Value = toml::from_str(body).unwrap();
        let config = crate::unit::UnitConfig::from_value(name, value, &Config::default()).unwrap();
        Unit::new(config)
    }

    fn units(list: Vec<Unit>) -> BTreeMap<String, Unit> {
        list.into_iter()
            .map(|unit| (unit.config.name.clone(), unit))
            .collect()
    }

    #[test]
    fn orders_after_dependencies() {
        let map = units(vec![
            unit(
                "app.target",
                r#"
[Unit]
Wants = ["b.service", "a.service"]
"#,
            ),
            unit(
                "a.service",
                r#"
[Unit]
After = ["b.service"]
[Service]
ExecStart = "/bin/true"
"#,
            ),
            unit(
                "b.service",
                r#"
[Service]
ExecStart = "/bin/true"
"#,
            ),
        ]);

        let order = transaction(&map, "app.target").unwrap();
        let b = order.iter().position(|name| name == "b.service").unwrap();
        let a = order.iter().position(|name| name == "a.service").unwrap();
        assert!(b < a, "b.service should start before a.service: {order:?}");
    }

    #[test]
    fn pulls_wants_and_requires() {
        let map = units(vec![
            unit(
                "app.target",
                r#"
[Unit]
Wants = ["a.service"]
Requires = ["b.service"]
"#,
            ),
            unit(
                "a.service",
                r#"
[Service]
ExecStart = "/bin/true"
"#,
            ),
            unit(
                "b.service",
                r#"
[Service]
ExecStart = "/bin/true"
"#,
            ),
        ]);

        let order = transaction(&map, "app.target").unwrap();
        assert_eq!(order.len(), 3);
        assert!(order.contains(&"a.service".to_string()));
        assert!(order.contains(&"b.service".to_string()));
    }

    #[test]
    fn missing_requires_is_an_error() {
        let map = units(vec![unit(
            "app.target",
            r#"
[Unit]
Requires = ["missing.service"]
"#,
        )]);
        assert!(matches!(
            transaction(&map, "app.target"),
            Err(GraphError::MissingRequirement { .. })
        ));
    }

    #[test]
    fn targets_start_after_their_services() {
        let map = units(vec![
            unit(
                "app.target",
                r#"
[Unit]
Wants = ["a.service", "b.service"]
"#,
            ),
            unit(
                "a.service",
                r#"
[Service]
ExecStart = "/bin/true"
"#,
            ),
            unit(
                "b.service",
                r#"
[Service]
ExecStart = "/bin/true"
"#,
            ),
        ]);

        let order = transaction(&map, "app.target").unwrap();
        assert_eq!(order.last().map(String::as_str), Some("app.target"));
    }

    #[test]
    fn detects_cycles() {
        let map = units(vec![
            unit(
                "a.service",
                r#"
[Unit]
After = ["b.service"]
Requires = ["b.service"]
[Service]
ExecStart = "/bin/true"
"#,
            ),
            unit(
                "b.service",
                r#"
[Unit]
After = ["a.service"]
Requires = ["a.service"]
[Service]
ExecStart = "/bin/true"
"#,
            ),
        ]);
        assert!(matches!(
            transaction(&map, "a.service"),
            Err(GraphError::Cycle(_))
        ));
    }

    #[test]
    fn unknown_target_is_an_error() {
        let map = BTreeMap::new();
        assert!(matches!(
            transaction(&map, "nope.target"),
            Err(GraphError::UnknownUnit(_))
        ));
    }

    #[test]
    fn stop_order_is_reversed() {
        let order = vec![
            "a.service".to_string(),
            "b.service".to_string(),
            "c.target".to_string(),
        ];
        assert_eq!(
            stop_order(&order),
            vec![
                "c.target".to_string(),
                "b.service".to_string(),
                "a.service".to_string()
            ]
        );
    }
}
