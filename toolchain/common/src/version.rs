//! Package version parsing and comparison.
//!
//! Versions follow the form `[epoch:]core[-release]`, where `core` is split into
//! numeric and alphabetic chunks on `.`, `-`, `_`, and `+`. Numeric chunks compare
//! numerically and sort above alphabetic chunks.

use std::cmp::Ordering;
use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// A parsed package version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    pub epoch: u32,
    pub core: VersionCore,
    pub release: u32,
}

/// The `core` portion of a version, stored as comparable chunks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VersionCore {
    chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Chunk {
    Num(u64),
    Str(String),
}

/// Error returned when a version string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionParseError {
    pub input: String,
    pub reason: &'static str,
}

impl fmt::Display for VersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid version {:?}: {}", self.input, self.reason)
    }
}

impl std::error::Error for VersionParseError {}

impl std::str::FromStr for Version {
    type Err = VersionParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Version::parse(input)
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Version::parse(&text).map_err(de::Error::custom)
    }
}

impl Version {
    /// Parse a version string such as `1:2.39-3`.
    pub fn parse(input: &str) -> Result<Self, VersionParseError> {
        let (epoch, rest) = match input.split_once(':') {
            Some((epoch, rest)) => {
                let epoch = epoch.parse::<u32>().map_err(|_| VersionParseError {
                    input: input.to_string(),
                    reason: "epoch is not a non-negative integer",
                })?;
                (epoch, rest)
            }
            None => (0, input),
        };

        let (core_str, release) = match rest.rsplit_once('-') {
            Some((core, release))
                if !release.is_empty() && release.chars().all(|c| c.is_ascii_digit()) =>
            {
                let release = release.parse::<u32>().map_err(|_| VersionParseError {
                    input: input.to_string(),
                    reason: "release is not a non-negative integer",
                })?;
                (core, release)
            }
            _ => (rest, 0),
        };

        if core_str.is_empty() {
            return Err(VersionParseError {
                input: input.to_string(),
                reason: "empty version core",
            });
        }

        Ok(Self {
            epoch,
            core: VersionCore::parse(core_str),
            release,
        })
    }
}

impl VersionCore {
    fn parse(input: &str) -> Self {
        let chunks = input
            .split(['.', '-', '_', '+'])
            .filter(|part| !part.is_empty())
            .map(|part| {
                if part.chars().all(|c| c.is_ascii_digit()) {
                    part.parse::<u64>()
                        .map(Chunk::Num)
                        .unwrap_or_else(|_| Chunk::Str(part.to_string()))
                } else {
                    Chunk::Str(part.to_string())
                }
            })
            .collect();

        Self { chunks }
    }

    fn trailing_is_zero(&self, from: usize) -> bool {
        self.chunks[from..].iter().all(|chunk| match chunk {
            Chunk::Num(n) => *n == 0,
            Chunk::Str(s) => s.is_empty(),
        })
    }
}

impl Ord for VersionCore {
    fn cmp(&self, other: &Self) -> Ordering {
        let shared = self.chunks.len().min(other.chunks.len());
        for i in 0..shared {
            let ordering = match (&self.chunks[i], &other.chunks[i]) {
                (Chunk::Num(a), Chunk::Num(b)) => a.cmp(b),
                (Chunk::Str(a), Chunk::Str(b)) => a.cmp(b),
                // Numeric chunks sort above alphabetic chunks.
                (Chunk::Num(_), Chunk::Str(_)) => Ordering::Greater,
                (Chunk::Str(_), Chunk::Num(_)) => Ordering::Less,
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }

        match self.chunks.len().cmp(&other.chunks.len()) {
            Ordering::Equal => Ordering::Equal,
            Ordering::Greater => {
                if self.trailing_is_zero(shared) {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            }
            Ordering::Less => {
                if other.trailing_is_zero(shared) {
                    Ordering::Equal
                } else {
                    Ordering::Less
                }
            }
        }
    }
}

impl PartialOrd for VersionCore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| self.core.cmp(&other.core))
            .then_with(|| self.release.cmp(&other.release))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.epoch != 0 {
            write!(f, "{}:", self.epoch)?;
        }

        let core = self
            .core
            .chunks
            .iter()
            .map(|chunk| match chunk {
                Chunk::Num(n) => n.to_string(),
                Chunk::Str(s) => s.clone(),
            })
            .collect::<Vec<_>>()
            .join(".");
        write!(f, "{}", core)?;

        if self.release != 0 {
            write!(f, "-{}", self.release)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Version;
    use std::cmp::Ordering;

    fn v(s: &str) -> Version {
        Version::parse(s).expect("valid version")
    }

    #[test]
    fn parses_epoch_and_release() {
        let version = v("2:1.4.5-3");
        assert_eq!(version.epoch, 2);
        assert_eq!(version.release, 3);
    }

    #[test]
    fn compares_numeric_chunks() {
        assert_eq!(v("1.10").cmp(&v("1.9")), Ordering::Greater);
    }

    #[test]
    fn numeric_sorts_above_alpha() {
        assert_eq!(v("1.1").cmp(&v("1.a")), Ordering::Greater);
    }

    #[test]
    fn trailing_zeros_are_equal() {
        assert_eq!(v("1.0").cmp(&v("1")), Ordering::Equal);
    }

    #[test]
    fn epoch_wins() {
        assert_eq!(v("1:0.1").cmp(&v("0:9.9")), Ordering::Greater);
    }

    #[test]
    fn release_breaks_ties() {
        assert_eq!(v("1.0-2").cmp(&v("1.0-1")), Ordering::Greater);
    }
}
