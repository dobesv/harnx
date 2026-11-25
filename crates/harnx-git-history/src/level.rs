use std::fmt;
use std::str::FromStr;

/// How much of a change a tool reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLevel {
    None,
    Summary,
    Files,
    Full,
}

impl DiffLevel {
    pub const NAMES: [&'static str; 4] = ["none", "summary", "files", "full"];

    /// Whether snapshots are taken at this level.
    pub fn snapshots(self) -> bool {
        self != DiffLevel::None
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DiffLevel::None => "none",
            DiffLevel::Summary => "summary",
            DiffLevel::Files => "files",
            DiffLevel::Full => "full",
        }
    }
}

impl fmt::Display for DiffLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for DiffLevel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [
            DiffLevel::None,
            DiffLevel::Summary,
            DiffLevel::Files,
            DiffLevel::Full,
        ]
        .into_iter()
        .find(|level| level.as_str() == value)
        .ok_or_else(|| format!("diff must be one of none, summary, files, full (got \"{value}\")"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_parse_and_print() {
        for name in DiffLevel::NAMES {
            let level: DiffLevel = name.parse().expect("known name parses");
            assert_eq!(level.to_string(), name);
            assert_eq!(level.as_str(), name);
        }
        let err = "on".parse::<DiffLevel>().expect_err("unknown name");
        assert_eq!(
            err,
            "diff must be one of none, summary, files, full (got \"on\")"
        );
        assert!(!DiffLevel::None.snapshots());
        assert!(DiffLevel::Summary.snapshots());
    }
}
