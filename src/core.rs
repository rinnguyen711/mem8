use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery};
use crate::scope::detect_scope;
use crate::store::Store;
use std::sync::Arc;
use uuid::Uuid;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;

/// Sanitize a raw agent query into a safe FTS query string.
///
/// FTS5 and `plainto_tsquery` both parse their input, and unbalanced quotes or
/// stray operators are errors rather than literal text. Splitting on FTS
/// syntax characters (as a naive sanitizer would) also breaks hyphenated
/// identifiers like `auth-token` into two independent terms, which can then
/// match a document containing "auth" and "token" nowhere near each other.
///
/// Instead, each surviving term is wrapped as a double-quoted FTS5 phrase, so
/// `auth-token login` becomes `"auth-token" "login"`. FTS5 treats a quoted
/// phrase as literal text, hyphen included, so this both avoids the parse
/// error a raw hyphen triggers and searches for the identifier as a unit.
/// Postgres's `plainto_tsquery` receives the same string; it ignores
/// punctuation and operators entirely, so the quotes are inert there.
///
/// A double quote embedded in a term is escaped by doubling it (`""`), which
/// is how FTS5 string literals escape an embedded quote, so no user input can
/// produce a malformed query.
pub fn sanitize_fts_query(raw: &str) -> Result<String> {
    let terms: Vec<String> = raw
        .split_whitespace()
        .filter(|t| !matches!(*t, "AND" | "OR" | "NOT" | "NEAR"))
        .map(|t| t.trim_matches(|c| "\"'()*:^".contains(c)))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();

    if terms.is_empty() {
        return Err(Mem8Error::InvalidInput(format!(
            "query '{raw}' contains no searchable terms"
        )));
    }

    Ok(terms.join(" "))
}

/// The memory service. Owns validation and scope resolution so that the MCP
/// server and the CLI behave identically.
pub struct Memory8 {
    store: Arc<dyn Store>,
}

impl Memory8 {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    fn resolve_scope(&self, explicit: Option<String>) -> String {
        match explicit {
            Some(p) if !p.trim().is_empty() => p,
            _ => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                detect_scope(&cwd)
            }
        }
    }

    pub async fn add(
        &self,
        content: &str,
        kind: Kind,
        tags: Vec<String>,
        project: Option<String>,
    ) -> Result<Memory> {
        if content.trim().is_empty() {
            return Err(Mem8Error::InvalidInput("content must not be empty".into()));
        }

        self.store
            .add(NewMemory {
                project: self.resolve_scope(project),
                kind,
                content: content.trim().to_string(),
                tags,
            })
            .await
    }

    pub async fn get(&self, id: Uuid) -> Result<Memory> {
        self.store.get(id).await
    }

    pub async fn update(
        &self,
        id: Uuid,
        content: Option<String>,
        kind: Option<Kind>,
        tags: Option<Vec<String>>,
    ) -> Result<Memory> {
        // Trim here as `add` does, so a memory's stored content does not depend
        // on which call wrote it.
        let content = match content {
            Some(c) if c.trim().is_empty() => {
                return Err(Mem8Error::InvalidInput("content must not be empty".into()));
            }
            Some(c) => Some(c.trim().to_string()),
            None => None,
        };
        self.store.update(id, MemoryUpdate { content, kind, tags }).await
    }

    pub async fn delete(&self, id: Uuid) -> Result<()> {
        self.store.delete(id).await
    }

    pub async fn search(
        &self,
        query: &str,
        project: Option<String>,
        global: bool,
        kind: Option<Kind>,
        tags: Vec<String>,
        limit: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        if query.trim().is_empty() {
            return Err(Mem8Error::InvalidInput("query must not be empty".into()));
        }

        let text = sanitize_fts_query(query)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let project = if global { None } else { Some(self.resolve_scope(project)) };

        self.store
            .search(SearchQuery { text, project, global, kind, tags, limit })
            .await
    }

    /// Every memory, for `mem8 export`.
    pub async fn all(&self) -> Result<Vec<Memory>> {
        self.store.all().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use crate::store::MemStore;
    use std::sync::Arc;

    fn service() -> Memory8 {
        Memory8::new(Arc::new(MemStore::new()))
    }

    #[tokio::test]
    async fn add_rejects_empty_content() {
        let svc = service();
        let err = svc.add("   ", Kind::Fact, vec![], Some("p1".into())).await.unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        assert!(err.to_string().contains("content"));
    }

    #[tokio::test]
    async fn add_uses_explicit_project_over_detection() {
        let svc = service();
        let m = svc.add("a fact", Kind::Fact, vec![], Some("explicit".into())).await.unwrap();
        assert_eq!(m.project, "explicit");
    }

    #[tokio::test]
    async fn add_falls_back_to_detected_scope() {
        let svc = service();
        let m = svc.add("a fact", Kind::Fact, vec![], None).await.unwrap();
        assert!(!m.project.is_empty(), "detected scope must never be empty");
    }

    #[tokio::test]
    async fn search_rejects_empty_query() {
        let svc = service();
        let err = svc.search("", None, false, None, vec![], None).await.unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
    }

    #[tokio::test]
    async fn search_clamps_limit_to_maximum() {
        let svc = service();
        for i in 0..60 {
            svc.add(&format!("fact number {i}"), Kind::Fact, vec![], Some("p1".into()))
                .await
                .unwrap();
        }
        let hits = svc
            .search("fact", Some("p1".into()), false, None, vec![], Some(999))
            .await
            .unwrap();
        assert!(hits.len() <= MAX_LIMIT, "limit must be clamped to {MAX_LIMIT}");
    }

    #[tokio::test]
    async fn search_defaults_limit_to_ten() {
        let svc = service();
        for i in 0..20 {
            svc.add(&format!("fact number {i}"), Kind::Fact, vec![], Some("p1".into()))
                .await
                .unwrap();
        }
        let hits = svc
            .search("fact", Some("p1".into()), false, None, vec![], None)
            .await
            .unwrap();
        assert_eq!(hits.len(), DEFAULT_LIMIT);
    }

    #[test]
    fn sanitize_strips_unbalanced_quotes() {
        // An unbalanced quote in the raw query must not carry through into an
        // unbalanced quote in the emitted FTS string -- that would make the
        // query malformed. The term's own stray `"` is trimmed, then the term
        // is re-wrapped in a fresh, properly balanced pair of quotes.
        let cleaned = sanitize_fts_query("auth \"broken").unwrap();
        assert_eq!(cleaned, "\"auth\" \"broken\"");
        assert_eq!(cleaned.matches('"').count() % 2, 0, "quotes must balance");
    }

    #[test]
    fn sanitize_strips_fts_operators() {
        // Bare FTS operator keywords are dropped; stray operator punctuation
        // clinging to a term is trimmed before the term is quoted, so no
        // operator syntax survives into the emitted query.
        let cleaned = sanitize_fts_query("auth AND (login OR session)*").unwrap();
        assert_eq!(cleaned, "\"auth\" \"login\" \"session\"");
        assert!(!cleaned.contains('('));
        assert!(!cleaned.contains(')'));
        assert!(!cleaned.contains('*'));
    }

    #[test]
    fn sanitize_rejects_a_query_with_no_usable_terms() {
        assert!(sanitize_fts_query("\"\"()*").is_err());
    }

    #[test]
    fn hyphenated_term_is_quoted_not_split() {
        // The old sanitizer stripped `-` and produced two bare terms
        // (`auth token`), which could match a document containing "auth" and
        // "token" nowhere near each other. It must now survive as one quoted
        // phrase so FTS5 treats the hyphenated identifier as literal text.
        let cleaned = sanitize_fts_query("auth-token").unwrap();
        assert_eq!(cleaned, "\"auth-token\"");
    }

    #[test]
    fn embedded_quote_cannot_break_the_query() {
        // A term containing a literal `"` must not produce a malformed FTS
        // string. The embedded quote is escaped by doubling it, so the
        // overall query stays a well-formed, balanced sequence of phrases.
        let cleaned = sanitize_fts_query("say\"hi").unwrap();
        assert_eq!(cleaned, "\"say\"\"hi\"");
        assert_eq!(cleaned.matches('"').count() % 2, 0, "quotes must balance");
    }

    #[tokio::test]
    async fn update_trims_content_like_add_does() {
        let svc = service();
        let added = svc
            .add("  spaced out  ", Kind::Fact, vec![], Some("p1".into()))
            .await
            .unwrap();
        assert_eq!(added.content, "spaced out");

        let updated = svc
            .update(added.id, Some("  revised  ".into()), None, None)
            .await
            .unwrap();
        assert_eq!(updated.content, "revised");
    }

    #[tokio::test]
    async fn update_on_missing_id_is_not_found() {
        let svc = service();
        let err = svc
            .update(uuid::Uuid::new_v4(), None, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::NotFound(_)));
    }
}
