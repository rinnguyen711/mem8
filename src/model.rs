use crate::error::Mem8Error;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Decision,
    Preference,
    Convention,
    Fact,
    Learning,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Decision,
        Kind::Preference,
        Kind::Convention,
        Kind::Fact,
        Kind::Learning,
    ];
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Kind::Decision => "decision",
            Kind::Preference => "preference",
            Kind::Convention => "convention",
            Kind::Fact => "fact",
            Kind::Learning => "learning",
        };
        f.write_str(s)
    }
}

impl FromStr for Kind {
    type Err = Mem8Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "decision" => Ok(Kind::Decision),
            "preference" => Ok(Kind::Preference),
            "convention" => Ok(Kind::Convention),
            "fact" => Ok(Kind::Fact),
            "learning" => Ok(Kind::Learning),
            other => Err(Mem8Error::InvalidInput(format!(
                "unknown kind '{other}'; expected one of: decision, preference, convention, fact, learning"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: Uuid,
    pub project: String,
    pub kind: Kind,
    pub content: String,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone)]
pub struct NewMemory {
    pub project: String,
    pub kind: Kind,
    pub content: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryUpdate {
    pub content: Option<String>,
    pub kind: Option<Kind>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub text: String,
    pub project: Option<String>,
    pub global: bool,
    pub kind: Option<Kind>,
    pub tags: Vec<String>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub memory: Memory,
    pub score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parses_from_lowercase() {
        assert_eq!("decision".parse::<Kind>().unwrap(), Kind::Decision);
        assert_eq!("learning".parse::<Kind>().unwrap(), Kind::Learning);
    }

    #[test]
    fn kind_rejects_unknown_value() {
        let err = "banana".parse::<Kind>().unwrap_err();
        assert!(err.to_string().contains("banana"));
        assert!(err.to_string().contains("decision"));
    }

    #[test]
    fn kind_displays_as_lowercase() {
        assert_eq!(Kind::Convention.to_string(), "convention");
    }

    #[test]
    fn kind_roundtrips_through_string() {
        for k in Kind::ALL {
            assert_eq!(k.to_string().parse::<Kind>().unwrap(), k);
        }
    }
}
