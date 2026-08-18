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
    /// Set when semantic search is enabled and the embedder was available.
    ///
    /// `None` means the memory is findable by keyword only. Storing it that way
    /// is deliberate: an embedding failure must not cost the user the write.
    pub embedding: Option<Vec<f32>>,
}

/// Hand-written rather than derived, because deriving it would require
/// `Kind: Default`, and a `#[default]` variant changes the JSON schema
/// generated for the MCP tool surface: schemars emits the defaulted variant as
/// a separate `const` branch inside a `oneOf` instead of one flat `enum`.
/// Agents read that schema, so the wire contract must not shift to make a test
/// constructor shorter.
impl Default for NewMemory {
    fn default() -> Self {
        Self {
            project: String::new(),
            kind: Kind::Decision,
            content: String::new(),
            tags: Vec::new(),
            embedding: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MemoryUpdate {
    pub content: Option<String>,
    pub kind: Option<Kind>,
    pub tags: Option<Vec<String>>,
    /// A replacement embedding, when the content changed and could be
    /// re-embedded. `None` leaves the stored vector untouched.
    pub embedding: Option<Vec<f32>>,
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

/// A search by embedding similarity rather than by words.
///
/// Mirrors `SearchQuery` field for field apart from carrying a vector instead
/// of text. The filters are not optional extras: a semantic search that ignored
/// `project` would surface one project's memories in another, which is exactly
/// what scoping exists to prevent.
#[derive(Debug, Clone)]
pub struct VectorQuery {
    pub embedding: Vec<f32>,
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
