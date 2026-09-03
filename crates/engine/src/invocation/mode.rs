//! What a headless run is *for* — and therefore what it may touch.
//!
//! The mode is the one field a caller must think about, and everything the posture rules enforce
//! follows from it. It is also the wire word an `action.toml` writes, so its string form is part of
//! the relay contract (`docs/RELAY.md`) and must stay stable.

use serde::Deserialize;

use crate::{EngineError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// A completion, nothing more: no tools, no repository, no permission mode. Judging, candidate
    /// generation, and the default for a relay action.
    #[default]
    Generate,
    /// An agentic run that may look but not touch: the read-only tool allowlist plus whatever
    /// extras the caller names, all of which must themselves be read-only.
    ReadonlyScan,
    /// An agentic run that may edit files. Requires an explicit workspace and permission mode —
    /// there is no default that is safe enough to be implicit.
    Edit,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Generate => "generate",
            Mode::ReadonlyScan => "readonly-scan",
            Mode::Edit => "edit",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "generate" => Ok(Mode::Generate),
            "readonly-scan" => Ok(Mode::ReadonlyScan),
            "edit" => Ok(Mode::Edit),
            other => Err(EngineError::Posture(format!(
                "unknown mode '{other}' (expected generate|readonly-scan|edit)"
            ))),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Mode;

    #[test]
    fn mode_round_trips_through_its_wire_name() {
        for m in [Mode::Generate, Mode::ReadonlyScan, Mode::Edit] {
            assert_eq!(m.as_str().parse::<Mode>().unwrap(), m);
            assert_eq!(
                serde_json::from_value::<Mode>(serde_json::json!(m.as_str())).unwrap(),
                m
            );
        }
        assert!("acceptEdits".parse::<Mode>().is_err());
        // An action.toml that says nothing about posture is a plain completion.
        assert_eq!(Mode::default(), Mode::Generate);
    }
}
