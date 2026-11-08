use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GameMode {
    Survival,
    #[default]
    Creative,
}

impl From<GameMode> for i32 {
    fn from(mode: GameMode) -> Self {
        match mode {
            GameMode::Survival => 0,
            GameMode::Creative => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_modes_have_explicit_names_and_protocol_ids() {
        for (mode, name, id) in [
            (GameMode::Survival, "survival", 0),
            (GameMode::Creative, "creative", 1),
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<GameMode>(name.into()).unwrap(),
                mode
            );
            assert_eq!(i32::from(mode), id);
        }
        assert_eq!(GameMode::default(), GameMode::Creative);
        for invalid in ["spectator", "adventure", "unknown", "CREATIVE", "0"] {
            assert!(serde_json::from_value::<GameMode>(invalid.into()).is_err());
        }
        assert!(serde_json::from_value::<GameMode>(0.into()).is_err());
    }
}
