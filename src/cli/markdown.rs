use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, NewMemory};
use std::str::FromStr;

/// Serialise memories to markdown, one section per memory.
pub fn to_markdown(memories: &[Memory]) -> String {
    let mut out = String::from("# mem8 export\n\n");
    for m in memories {
        out.push_str(&format!(
            "## {}\n- project: {}\n- kind: {}\n- tags: {}\n- created: {}\n\n{}\n\n",
            m.id,
            m.project,
            m.kind,
            m.tags.join(", "),
            m.created_at.to_rfc3339(),
            m.content.trim()
        ));
    }
    out
}

/// Parse an exported markdown file back into memories.
///
/// Identifiers and timestamps in the file are informational; import always
/// creates fresh rows so that importing into a populated database cannot
/// collide with existing identifiers.
pub fn from_markdown(text: &str) -> Result<Vec<NewMemory>> {
    let mut memories = Vec::new();

    // Prepend a sentinel newline so a `## ` heading at the very start of
    // `text` (as in a hand-written fixture with no preamble) is matched by
    // the `"\n## "` delimiter the same way a heading preceded by the
    // `# mem8 export\n\n` preamble (as in real `to_markdown` output) is.
    // Without this, `text.split("\n## ")` returns the whole input as a
    // single unsplit chunk when it starts with `## `, and `.skip(1)` then
    // discards it entirely instead of yielding it as a section.
    let sentinel = format!("\n{text}");

    for section in sentinel.split("\n## ").skip(1) {
        let mut project = String::new();
        let mut kind: Option<Kind> = None;
        let mut tags: Vec<String> = Vec::new();
        let mut body_lines: Vec<&str> = Vec::new();
        let mut in_body = false;

        for line in section.lines().skip(1) {
            if in_body {
                body_lines.push(line);
            } else if let Some(v) = line.strip_prefix("- project:") {
                project = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("- kind:") {
                kind = Some(Kind::from_str(v.trim())?);
            } else if let Some(v) = line.strip_prefix("- tags:") {
                tags = v
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
            } else if line.starts_with("- created:") {
                continue;
            } else if line.trim().is_empty() && !body_lines.is_empty() {
                body_lines.push(line);
            } else if line.trim().is_empty() {
                in_body = true;
            }
        }

        let content = body_lines.join("\n").trim().to_string();
        if content.is_empty() {
            continue;
        }

        let kind = kind.ok_or_else(|| {
            Mem8Error::InvalidInput("a memory section is missing its 'kind' field".into())
        })?;

        memories.push(NewMemory {
            project: if project.is_empty() { "default".into() } else { project },
            kind,
            content,
            tags,
        });
    }

    Ok(memories)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Memory};
    use chrono::Utc;
    use uuid::Uuid;

    fn memory(content: &str, tags: Vec<String>) -> Memory {
        let now = Utc::now();
        Memory {
            id: Uuid::new_v4(),
            project: "mem8".into(),
            kind: Kind::Decision,
            content: content.into(),
            tags,
            created_at: now,
            updated_at: now,
            embedding: None,
        }
    }

    #[test]
    fn roundtrip_preserves_content_kind_project_and_tags() {
        let original = vec![
            memory("We chose Rust for the binary.", vec!["lang".into()]),
            memory("Tests run with cargo test.", vec![]),
        ];

        let text = to_markdown(&original);
        let parsed = from_markdown(&text).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].content, "We chose Rust for the binary.");
        assert_eq!(parsed[0].kind, Kind::Decision);
        assert_eq!(parsed[0].project, "mem8");
        assert_eq!(parsed[0].tags, vec!["lang".to_string()]);
        assert!(parsed[1].tags.is_empty());
    }

    #[test]
    fn multiline_content_survives_the_roundtrip() {
        let original = vec![memory("First line.\n\nSecond paragraph.", vec![])];
        let parsed = from_markdown(&to_markdown(&original)).unwrap();
        assert_eq!(parsed[0].content, "First line.\n\nSecond paragraph.");
    }

    #[test]
    fn empty_input_parses_to_no_memories() {
        assert!(from_markdown("").unwrap().is_empty());
    }

    #[test]
    fn unknown_kind_in_a_file_is_an_error() {
        let text = "## 7a1f\n- project: p\n- kind: banana\n- tags:\n- created: 2026-08-11T00:00:00+00:00\n\nBody.\n";
        assert!(from_markdown(text).is_err());
    }
}
