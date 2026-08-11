use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, NewMemory};
use std::str::FromStr;
use uuid::Uuid;

/// True when `line` is a section heading, i.e. `## ` followed by a
/// UUID-shaped token. Content lines that merely start with `## ` (a heading
/// the user typed as part of a memory's body) do not match this shape, so
/// they cannot be mistaken for a new section by `from_markdown`. This is
/// what makes the section boundary unambiguous without needing to escape
/// user content on write.
fn is_section_heading(line: &str) -> bool {
    line.strip_prefix("## ")
        .map(|rest| Uuid::parse_str(rest.trim()).is_ok())
        .unwrap_or(false)
}

/// Serialise memories to markdown, one section per memory.
pub fn to_markdown(memories: &[Memory]) -> String {
    let mut out = String::from("# mem8 export\n\n");
    for m in memories {
        let tags_json = serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".to_string());
        out.push_str(&format!(
            "## {}\n- project: {}\n- kind: {}\n- tags: {}\n- created: {}\n\n{}\n\n",
            m.id,
            m.project,
            m.kind,
            tags_json,
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
///
/// A new section starts only at a line matching `## <uuid>` (see
/// `is_section_heading`); everything else, including a content line that
/// happens to start with `## `, is treated as part of the current memory's
/// body. This keeps the round trip lossless for arbitrary content without
/// requiring any escaping on write.
pub fn from_markdown(text: &str) -> Result<Vec<NewMemory>> {
    let mut memories = Vec::new();

    // Split the input into chunks starting at each section-heading line.
    let lines: Vec<&str> = text.lines().collect();
    let mut section_starts: Vec<usize> =
        lines.iter().enumerate().filter(|(_, l)| is_section_heading(l)).map(|(i, _)| i).collect();
    section_starts.push(lines.len());

    for window in section_starts.windows(2) {
        let (start, end) = (window[0], window[1]);
        let mut project = String::new();
        let mut kind: Option<Kind> = None;
        let mut tags: Vec<String> = Vec::new();
        let mut body_lines: Vec<&str> = Vec::new();
        let mut in_body = false;

        // Skip the heading line itself (start).
        for &line in &lines[start + 1..end] {
            if in_body {
                body_lines.push(line);
            } else if let Some(v) = line.strip_prefix("- project:") {
                project = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("- kind:") {
                kind = Some(Kind::from_str(v.trim())?);
            } else if let Some(v) = line.strip_prefix("- tags:") {
                let v = v.trim();
                tags = if v.is_empty() {
                    Vec::new()
                } else if let Ok(parsed) = serde_json::from_str::<Vec<String>>(v) {
                    parsed
                } else {
                    // Fall back to the legacy comma-separated form for
                    // hand-written or pre-existing files that predate the
                    // JSON tag format. Tags containing commas cannot be
                    // recovered unambiguously in this form.
                    v.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                };
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
        let text = "## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n- project: p\n- kind: banana\n- tags:\n- created: 2026-08-11T00:00:00+00:00\n\nBody.\n";
        assert!(from_markdown(text).is_err());
    }

    #[test]
    fn content_with_markdown_heading_survives_roundtrip() {
        let content = "Intro line.\n## Not actually a heading\nMore body after it.";
        let original = vec![memory(content, vec![])];

        let text = to_markdown(&original);
        let parsed = from_markdown(&text).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].content, content);
    }

    #[test]
    fn tags_containing_commas_survive_roundtrip() {
        let original = vec![memory("Some content.", vec!["a,b".to_string(), "c".to_string()])];

        let text = to_markdown(&original);
        let parsed = from_markdown(&text).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].tags, vec!["a,b".to_string(), "c".to_string()]);
    }

    #[test]
    fn content_with_front_matter_lookalike_survives_roundtrip() {
        let content = "Body start.\n- project: not-really\nBody end.";
        let original = vec![memory(content, vec![])];

        let text = to_markdown(&original);
        let parsed = from_markdown(&text).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].content, content);
        assert_eq!(parsed[0].project, "mem8");
    }
}
