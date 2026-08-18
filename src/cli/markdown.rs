use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, NewMemory};
use chrono::{DateTime, Utc};
use std::str::FromStr;
use uuid::Uuid;

/// A memory parsed from a markdown file, with the fields import needs that
/// `NewMemory` cannot carry.
///
/// `NewMemory` has no id, deliberately — import always creates fresh rows so it
/// cannot collide with existing identifiers. But remapping `superseded_by`
/// needs the file's own ids to build the old-to-new mapping, so the parsed form
/// keeps them alongside.
#[derive(Debug, Clone)]
pub struct ParsedMemory {
    pub new: NewMemory,
    /// The uuid from the section heading, used only to resolve `superseded_by`
    /// against other sections in the same file.
    pub original_id: Option<Uuid>,
    /// The successor's *original* id, to be remapped on import.
    pub superseded_by: Option<Uuid>,
    pub invalid_at: Option<DateTime<Utc>>,
}

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

        // Optional header lines, written only when set, so a live memory's
        // export is byte-identical to what it was before supersession existed.
        // They go after `- created:` and before the blank line that starts the
        // body: `from_markdown` uses that blank line as the header/body
        // boundary, so nothing may come between it and the content.
        let mut extra = String::new();
        if let Some(successor) = m.superseded_by {
            extra.push_str(&format!("- superseded_by: {successor}\n"));
        }
        if let Some(invalid) = m.invalid_at {
            extra.push_str(&format!("- invalid_at: {}\n", invalid.to_rfc3339()));
        }

        out.push_str(&format!(
            "## {}\n- project: {}\n- kind: {}\n- tags: {}\n- created: {}\n{}\n{}\n\n",
            m.id,
            m.project,
            m.kind,
            tags_json,
            m.created_at.to_rfc3339(),
            extra,
            m.content.trim()
        ));
    }
    out
}

/// Parse an exported markdown file back into memories.
///
/// Identifiers and timestamps in the file are informational; import always
/// creates fresh rows so that importing into a populated database cannot
/// collide with existing identifiers. The section heading's own uuid is still
/// returned, in `ParsedMemory::original_id`, because remapping `superseded_by`
/// onto the freshly created rows needs it.
///
/// `superseded_by` and `invalid_at` are optional: a file written before they
/// existed still parses, with both left as `None`.
///
/// A new section starts only at a line matching `## <uuid>` (see
/// `is_section_heading`); everything else, including a content line that
/// happens to start with `## `, is treated as part of the current memory's
/// body. This keeps the round trip lossless for arbitrary content without
/// requiring any escaping on write.
///
/// A recognised section is rejected with a named error, rather than being
/// silently dropped or misfiled, if it is missing `kind:`, missing
/// `project:`, or has no body content. Trailing blank lines after the last
/// section (or whitespace between sections) are not sections and are not
/// affected by this.
pub fn from_markdown(text: &str) -> Result<Vec<ParsedMemory>> {
    let mut memories = Vec::new();

    // Split the input into chunks starting at each section-heading line.
    let lines: Vec<&str> = text.lines().collect();
    let mut section_starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_section_heading(l))
        .map(|(i, _)| i)
        .collect();
    section_starts.push(lines.len());

    for window in section_starts.windows(2) {
        let (start, end) = (window[0], window[1]);
        let heading_id = lines[start]
            .strip_prefix("## ")
            .unwrap_or(lines[start])
            .trim();
        let mut project = String::new();
        let mut kind: Option<Kind> = None;
        let mut tags: Vec<String> = Vec::new();
        let mut superseded_by: Option<Uuid> = None;
        let mut invalid_at: Option<DateTime<Utc>> = None;
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
            } else if let Some(v) = line.strip_prefix("- superseded_by:") {
                superseded_by = Some(Uuid::parse_str(v.trim()).map_err(|e| {
                    Mem8Error::InvalidInput(format!(
                        "section '{heading_id}' has an unparseable 'superseded_by': {e}"
                    ))
                })?);
            } else if let Some(v) = line.strip_prefix("- invalid_at:") {
                invalid_at = Some(
                    DateTime::parse_from_rfc3339(v.trim())
                        .map(|d| d.with_timezone(&Utc))
                        .map_err(|e| {
                            Mem8Error::InvalidInput(format!(
                                "section '{heading_id}' has an unparseable 'invalid_at': {e}"
                            ))
                        })?,
                );
            } else if line.starts_with("- created:") {
                continue;
            } else if line.trim().is_empty() && !body_lines.is_empty() {
                body_lines.push(line);
            } else if line.trim().is_empty() {
                in_body = true;
            }
        }

        let content = body_lines.join("\n").trim().to_string();

        let kind = kind.ok_or_else(|| {
            Mem8Error::InvalidInput(format!(
                "section '{heading_id}' is missing its 'kind' field"
            ))
        })?;

        if project.is_empty() {
            return Err(Mem8Error::InvalidInput(format!(
                "section '{heading_id}' is missing its 'project' field"
            )));
        }

        if content.is_empty() {
            return Err(Mem8Error::InvalidInput(format!(
                "section '{heading_id}' has no content"
            )));
        }

        // Import never carries an embedding: the markdown format does not
        // record one, and reconstructing it here would mean loading a model in
        // a code path that otherwise needs none. `mem8 reindex` backfills.
        memories.push(ParsedMemory {
            new: NewMemory {
                project,
                kind,
                content,
                tags,
                embedding: None,
            },
            original_id: Uuid::parse_str(heading_id).ok(),
            superseded_by,
            invalid_at,
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
            superseded_by: None,
            invalid_at: None,
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
        assert_eq!(parsed[0].new.content, "We chose Rust for the binary.");
        assert_eq!(parsed[0].new.kind, Kind::Decision);
        assert_eq!(parsed[0].new.project, "mem8");
        assert_eq!(parsed[0].new.tags, vec!["lang".to_string()]);
        assert!(parsed[1].new.tags.is_empty());
    }

    #[test]
    fn multiline_content_survives_the_roundtrip() {
        let original = vec![memory("First line.\n\nSecond paragraph.", vec![])];
        let parsed = from_markdown(&to_markdown(&original)).unwrap();
        assert_eq!(parsed[0].new.content, "First line.\n\nSecond paragraph.");
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
        assert_eq!(parsed[0].new.content, content);
    }

    #[test]
    fn tags_containing_commas_survive_roundtrip() {
        let original = vec![memory(
            "Some content.",
            vec!["a,b".to_string(), "c".to_string()],
        )];

        let text = to_markdown(&original);
        let parsed = from_markdown(&text).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].new.tags, vec!["a,b".to_string(), "c".to_string()]);
    }

    #[test]
    fn content_with_front_matter_lookalike_survives_roundtrip() {
        let content = "Body start.\n- project: not-really\nBody end.";
        let original = vec![memory(content, vec![])];

        let text = to_markdown(&original);
        let parsed = from_markdown(&text).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].new.content, content);
        assert_eq!(parsed[0].new.project, "mem8");
    }

    #[test]
    fn missing_project_in_a_file_is_an_error() {
        let text = "## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n- kind: decision\n- tags:\n- created: 2026-08-11T00:00:00+00:00\n\nBody.\n";
        let err = from_markdown(text).unwrap_err().to_string();
        assert!(
            err.contains("project"),
            "error should mention 'project', got: {err}"
        );
    }

    #[test]
    fn section_with_no_content_is_an_error() {
        let text = "## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n- project: p\n- kind: decision\n- tags:\n- created: 2026-08-11T00:00:00+00:00\n";
        let err = from_markdown(text).unwrap_err().to_string();
        assert!(
            err.contains("7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f"),
            "error should identify the offending section, got: {err}"
        );
    }

    #[test]
    fn trailing_whitespace_after_last_memory_is_not_an_error() {
        let text = "## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n- project: p\n- kind: decision\n- tags:\n- created: 2026-08-11T00:00:00+00:00\n\nBody.\n\n\n   \n\n";
        let parsed = from_markdown(text).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].new.content, "Body.");
    }

    #[test]
    fn live_memory_exports_byte_identically_to_before() {
        let m = memory("Plain live fact.", vec![]);
        let text = to_markdown(&[m]);
        assert!(!text.contains("superseded_by"), "got: {text}");
        assert!(!text.contains("invalid_at"), "got: {text}");
    }

    #[test]
    fn superseded_memory_roundtrips_with_remapped_successor() {
        let now = Utc::now();
        let old_id = Uuid::new_v4();
        let new_id = Uuid::new_v4();

        let mut old = memory("storage is sqlite", vec![]);
        old.id = old_id;
        old.superseded_by = Some(new_id);
        old.invalid_at = Some(now);

        let mut new = memory("storage is postgres", vec![]);
        new.id = new_id;

        let text = to_markdown(&[old, new]);
        assert!(
            text.contains(&new_id.to_string()),
            "successor uuid must be written"
        );

        let parsed = from_markdown(&text).unwrap();
        assert_eq!(parsed.len(), 2);

        // The parsed form carries the original heading id, so import can remap.
        assert_eq!(parsed[0].original_id, Some(old_id));
        assert_eq!(parsed[0].superseded_by, Some(new_id));
        assert!(parsed[0].invalid_at.is_some());
        assert_eq!(parsed[1].superseded_by, None);
    }

    #[test]
    fn a_file_without_the_new_fields_still_imports() {
        // Existing export files must keep loading: both fields are optional.
        let text = "## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n- project: p\n- kind: decision\n- tags: []\n- created: 2026-08-11T00:00:00+00:00\n\nBody.\n";
        let parsed = from_markdown(text).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].superseded_by, None);
        assert_eq!(parsed[0].invalid_at, None);
    }

    #[test]
    fn an_unparseable_invalid_at_is_an_error_naming_the_section() {
        let text = "## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n- project: p\n- kind: decision\n- tags: []\n- created: 2026-08-11T00:00:00+00:00\n- invalid_at: not-a-date\n\nBody.\n";
        let err = from_markdown(text).unwrap_err().to_string();
        assert!(
            err.contains("7a1f7a1f"),
            "error should identify the section, got: {err}"
        );
        assert!(err.contains("invalid_at"), "got: {err}");
    }

    #[test]
    fn content_containing_the_new_header_names_survives_roundtrip() {
        let successor = Uuid::new_v4();
        let content = format!(
            "Body start.\n- invalid_at: 2020-01-01T00:00:00+00:00\n- superseded_by: {successor}\nBody end."
        );
        let original = vec![memory(&content, vec![])];

        let text = to_markdown(&original);
        let parsed = from_markdown(&text).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].new.content, content);
        assert_eq!(parsed[0].superseded_by, None);
        assert_eq!(parsed[0].invalid_at, None);
    }
}
