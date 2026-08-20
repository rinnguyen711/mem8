use crate::core::{Memory8, SearchOptions};
#[allow(unused_imports)]
use crate::error::Mem8Error;
use crate::model::{Kind, SearchHit};
use chrono::{DateTime, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AddMemoryParams {
    /// The memory to store, in full sentences.
    pub content: String,
    /// The kind of memory: decision, preference, convention, fact, or learning.
    pub kind: Kind,
    /// Optional labels for filtering later.
    pub tags: Option<Vec<String>>,
    /// The project this memory belongs to.
    ///
    /// Optional when mem8 runs locally, where it is detected from the working
    /// directory. Required when mem8 is served over HTTP, because the server
    /// cannot infer which project a remote caller means.
    pub project: Option<String>,
    /// The id of a memory this one replaces, when the project has changed its
    /// mind.
    ///
    /// Pass this when the new memory contradicts an existing one you just found
    /// by searching — the old fact stops being returned by search but stays
    /// retrievable by id, so past decisions remain explicable. Leave it unset
    /// when the memory is simply new information.
    ///
    /// Use the `id` shown for a hit in `search_memory` results.
    ///
    /// The target must be in the same project and must not already be
    /// superseded.
    pub supersedes: Option<Uuid>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchMemoryParams {
    /// Distinctive keywords, not a sentence. Every word must appear in a
    /// memory for it to match, so "porter tokenizer" finds more than "why did
    /// we pick the porter tokenizer". Two or three specific terms work best;
    /// search again with different words if the first attempt finds nothing.
    pub query: String,
    /// Restrict results to one kind.
    pub kind: Option<Kind>,
    /// Only return memories carrying all of these tags.
    pub tags: Option<Vec<String>>,
    /// The project to search.
    ///
    /// Optional when mem8 runs locally, where it is detected from the working
    /// directory. Required when mem8 is served over HTTP, unless `global` is
    /// true.
    pub project: Option<String>,
    /// Search every project instead of the current one; overrides `project` when true.
    pub global: Option<bool>,
    /// Maximum results; defaults to 10, capped at 50.
    pub limit: Option<usize>,
    /// Include memories that have been replaced by newer ones.
    ///
    /// Defaults to false, which returns only what is currently true. Set it to
    /// see the full history of a changed decision: superseded entries are marked
    /// with the date they stopped being true, so do not quote one as current.
    /// Cannot be combined with `as_of`.
    pub include_superseded: Option<bool>,
    /// Answer as of a past instant, RFC3339 — what was believed then.
    ///
    /// Returns memories created at or before this time that had not yet been
    /// replaced as of it. Cannot be combined with `include_superseded`.
    pub as_of: Option<DateTime<Utc>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct IdParams {
    /// The memory's identifier, as returned by add_memory or search_memory.
    pub id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateMemoryParams {
    /// The memory's identifier.
    pub id: String,
    /// Replacement content.
    pub content: Option<String>,
    /// Replacement kind.
    pub kind: Option<Kind>,
    /// Replacement tags; replaces the whole list.
    pub tags: Option<Vec<String>>,
}

fn ok(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

fn fail(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(text.into())])
}

/// Test helper: flatten a result's content into a single string.
#[cfg(test)]
fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_id(raw: &str) -> Result<Uuid, CallToolResult> {
    Uuid::parse_str(raw).map_err(|_| fail(format!("'{raw}' is not a valid memory id")))
}

fn render_hits(hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return "No matching memories.".to_string();
    }
    hits.iter()
        .map(|h| {
            // A superseded memory must never read as current. `include_superseded`
            // exists so an agent can see the history of a changed decision, which
            // only works if the retracted entries are distinguishable from the
            // live one.
            //
            // Keyed on `invalid_at` rather than `superseded_by`, matching what
            // search filters on: a memory can be dead with an unknown successor,
            // and that is still not current. Appended only when the memory is
            // actually dead, so an ordinary search renders byte-identically to
            // before this existed and pays no tokens for the feature.
            let superseded = match h.memory.invalid_at {
                Some(at) => format!("  superseded: {}", at.format("%Y-%m-%d")),
                None => String::new(),
            };

            // Results already arrive best-first, but without the score every hit
            // reads as equally relevant, so a vague query looks the same as an
            // exact one. Showing it lets the caller tell a strong match from the
            // long tail below it.
            format!(
                "[{}] ({}, score {:.3}) {}\n  id: {}  project: {}  tags: {}{}",
                h.memory.created_at.format("%Y-%m-%d"),
                h.memory.kind,
                h.score,
                h.memory.content,
                h.memory.id,
                h.memory.project,
                if h.memory.tags.is_empty() {
                    "-".into()
                } else {
                    h.memory.tags.join(", ")
                },
                superseded
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Clone)]
pub struct Mem8Server {
    service: Arc<Memory8>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Mem8Server>,
}

impl Mem8Server {
    pub fn new(service: Arc<Memory8>) -> Self {
        Self {
            service,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl Mem8Server {
    #[tool(
        description = "Store a memory for later recall. Use for decisions, preferences, conventions, facts, and learnings that should outlive this session."
    )]
    async fn add_memory(
        &self,
        Parameters(p): Parameters<AddMemoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(
            match self
                .service
                .add(
                    &p.content,
                    p.kind,
                    p.tags.unwrap_or_default(),
                    p.project,
                    p.supersedes,
                )
                .await
            {
                Ok(m) => ok(format!(
                    "Stored {} memory in '{}'. id: {}",
                    m.kind, m.project, m.id
                )),
                Err(e) => fail(e.to_string()),
            },
        )
    }

    #[tool(
        description = "Search stored memories by keyword. Search here before concluding something is unknown or was never decided — memories from earlier sessions are not otherwise visible. Scoped to the current project unless global is true."
    )]
    async fn search_memory(
        &self,
        Parameters(p): Parameters<SearchMemoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        Ok(
            match self
                .service
                .search(
                    &p.query,
                    SearchOptions {
                        project: p.project,
                        global: p.global.unwrap_or(false),
                        kind: p.kind,
                        tags: p.tags.unwrap_or_default(),
                        limit: p.limit,
                        include_superseded: p.include_superseded.unwrap_or(false),
                        as_of: p.as_of,
                    },
                )
                .await
            {
                Ok(hits) => ok(render_hits(&hits)),
                Err(e) => fail(e.to_string()),
            },
        )
    }

    #[tool(description = "Retrieve one memory in full by its id.")]
    async fn get_memory(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let id = match parse_id(&p.id) {
            Ok(i) => i,
            Err(e) => return Ok(e),
        };

        Ok(match self.service.get(id).await {
            Ok(m) => ok(format!(
                "({}) {}\n  id: {}  project: {}  tags: {}\n  created: {}  updated: {}",
                m.kind,
                m.content,
                m.id,
                m.project,
                if m.tags.is_empty() {
                    "-".into()
                } else {
                    m.tags.join(", ")
                },
                m.created_at.to_rfc3339(),
                m.updated_at.to_rfc3339()
            )),
            Err(e) => fail(e.to_string()),
        })
    }

    #[tool(
        description = "Revise an existing memory. Prefer this over storing a second, contradictory memory."
    )]
    async fn update_memory(
        &self,
        Parameters(p): Parameters<UpdateMemoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let id = match parse_id(&p.id) {
            Ok(i) => i,
            Err(e) => return Ok(e),
        };
        Ok(
            match self.service.update(id, p.content, p.kind, p.tags).await {
                Ok(m) => ok(format!("Updated memory {}.", m.id)),
                Err(e) => fail(e.to_string()),
            },
        )
    }

    #[tool(description = "Permanently delete a memory by its id.")]
    async fn delete_memory(
        &self,
        Parameters(p): Parameters<IdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let id = match parse_id(&p.id) {
            Ok(i) => i,
            Err(e) => return Ok(e),
        };

        Ok(match self.service.delete(id).await {
            Ok(()) => ok(format!("Deleted memory {id}.")),
            Err(e) => fail(e.to_string()),
        })
    }
}

#[tool_handler]
impl ServerHandler for Mem8Server {
    /// Advertise the tools capability and identify this server by name.
    ///
    /// `#[tool_handler]` wires up `list_tools` and `call_tool` but leaves
    /// `get_info` at its default, which reports no capabilities at all. A
    /// client that trusts the handshake — as the specification says it should —
    /// then never registers the tools, even though `tools/list` would have
    /// returned all five. The default also names the server after the SDK
    /// rather than after mem8.
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo {
            capabilities: rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: rmcp::model::Implementation {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(
                "Persistent memory that outlives this session, scoped to the \
                 current project. Search before assuming something is unknown, \
                 and store decisions, preferences, conventions, facts, and \
                 learnings worth recalling later."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}

/// Run the MCP server over stdio until the client disconnects.
pub async fn serve_stdio() -> anyhow::Result<()> {
    use rmcp::transport::io::stdio;
    use rmcp::ServiceExt;

    let server = Mem8Server::new(Arc::new(crate::cli::build_service().await?));

    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemStore, Store};
    use std::sync::Arc;

    fn server() -> Mem8Server {
        Mem8Server::new(Arc::new(Memory8::new(Arc::new(MemStore::new()))))
    }

    #[tokio::test]
    async fn add_then_search_returns_the_memory() {
        let s = server();
        s.add_memory(Parameters(AddMemoryParams {
            content: "we chose rust".into(),
            kind: Kind::Decision,
            tags: None,
            project: Some("p1".into()),
            supersedes: None,
        }))
        .await
        .expect("protocol-level call must not fail");

        let result = s
            .search_memory(Parameters(SearchMemoryParams {
                query: "rust".into(),
                kind: None,
                tags: None,
                project: Some("p1".into()),
                global: None,
                limit: None,
                include_superseded: None,
                as_of: None,
            }))
            .await
            .expect("protocol-level call must not fail");

        let text = result_text(&result);
        assert!(text.contains("we chose rust"), "got: {text}");
        assert!(
            text.contains("score "),
            "a hit should carry its relevance score, got: {text}"
        );
    }

    /// `kind` is now the real `Kind` enum, so an unrecognized value can no
    /// longer be expressed by constructing `AddMemoryParams` directly -- it
    /// won't compile. The failure now happens one layer down, in JSON
    /// deserialization (the same `serde_json::from_value` path rmcp's own
    /// `Parameters` extractor uses; see `tests/e2e.rs` for confirmation of
    /// the exact wire-level response a real client receives). What matters
    /// here is that deserializing a bad kind fails cleanly -- with a message
    /// naming the bad value and the valid ones -- rather than panicking.
    #[test]
    fn unknown_kind_fails_deserialization_not_a_panic() {
        let raw = serde_json::json!({
            "content": "something",
            "kind": "banana",
            "project": "p1"
        });

        let err = serde_json::from_value::<AddMemoryParams>(raw)
            .expect_err("an unrecognized kind must not deserialize");

        let message = err.to_string();
        assert!(message.contains("banana"), "got: {message}");
        for k in ["decision", "preference", "convention", "fact", "learning"] {
            assert!(
                message.contains(k),
                "expected '{k}' documented in error, got: {message}"
            );
        }
    }

    #[tokio::test]
    async fn get_with_malformed_uuid_is_a_tool_error() {
        let s = server();
        let result = s
            .get_memory(Parameters(IdParams {
                id: "not-a-uuid".into(),
            }))
            .await
            .expect("protocol-level call must not fail");
        assert!(result.is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn delete_of_unknown_id_reports_not_found() {
        let s = server();
        let result = s
            .delete_memory(Parameters(IdParams {
                id: uuid::Uuid::new_v4().to_string(),
            }))
            .await
            .expect("protocol-level call must not fail");
        assert!(result.is_error.unwrap_or(false));
        assert!(result_text(&result).contains("not found"));
    }

    /// The whole point of `supersedes` on the tool surface: an agent that
    /// records a changed decision sees only the current one when it searches
    /// again.
    #[tokio::test]
    async fn add_memory_accepts_supersedes_and_hides_the_old_fact() {
        let s = server();

        let old = s
            .service
            .add(
                "storage is sqlite",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .expect("the first memory must store");

        let added = s
            .add_memory(Parameters(AddMemoryParams {
                content: "storage is postgres".into(),
                kind: Kind::Decision,
                tags: None,
                project: Some("p1".into()),
                supersedes: Some(old.id),
            }))
            .await
            .expect("protocol-level call must not fail");
        assert!(
            !added.is_error.unwrap_or(false),
            "supersede should succeed, got: {}",
            result_text(&added)
        );

        let result = s
            .search_memory(Parameters(SearchMemoryParams {
                query: "storage".into(),
                kind: None,
                tags: None,
                project: Some("p1".into()),
                global: None,
                limit: None,
                include_superseded: None,
                as_of: None,
            }))
            .await
            .expect("protocol-level call must not fail");

        let text = result_text(&result);
        assert!(text.contains("storage is postgres"), "got: {text}");
        assert!(
            !text.contains("storage is sqlite"),
            "only the live fact should be returned, got: {text}"
        );

        // Superseded, not deleted: `get` still answers, which is what keeps a
        // past decision explicable.
        let fetched = s
            .get_memory(Parameters(IdParams {
                id: old.id.to_string(),
            }))
            .await
            .expect("protocol-level call must not fail");
        assert!(
            result_text(&fetched).contains("storage is sqlite"),
            "the superseded memory must stay retrievable by id"
        );
    }

    /// `include_superseded` reaches the search options rather than being
    /// dropped on the way through.
    #[tokio::test]
    async fn include_superseded_returns_the_replaced_fact() {
        let s = server();

        let old = s
            .service
            .add(
                "storage is sqlite",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        s.service
            .add(
                "storage is postgres",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(old.id),
            )
            .await
            .unwrap();

        let result = s
            .search_memory(Parameters(SearchMemoryParams {
                query: "storage".into(),
                kind: None,
                tags: None,
                project: Some("p1".into()),
                global: None,
                limit: None,
                include_superseded: Some(true),
                as_of: None,
            }))
            .await
            .expect("protocol-level call must not fail");

        let text = result_text(&result);
        assert!(text.contains("storage is sqlite"), "got: {text}");
        assert!(text.contains("storage is postgres"), "got: {text}");
    }

    /// `as_of` reaches the search options too: at an instant inside the old
    /// memory's validity window, the old fact is what was believed.
    #[tokio::test]
    async fn as_of_answers_what_was_believed_then() {
        let store = Arc::new(MemStore::new());
        let s = Mem8Server::new(Arc::new(Memory8::new(store.clone())));

        let old = s
            .service
            .add(
                "storage is sqlite",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        let new = s
            .service
            .add(
                "storage is postgres",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(old.id),
            )
            .await
            .unwrap();

        // Invalidate again through the store with an explicit instant rather
        // than probing the window `add` produced. Two wall-clock samples
        // microseconds apart leave a window narrower than any round offset can
        // land inside -- `created_at` is stored raw while `invalid_at` is
        // truncated to microseconds -- so `core`'s own `as_of` test widens the
        // window by construction for the same reason. `supersede` is
        // write-once, so re-invalidating the already-dead `old` is refused;
        // invalidate `new` instead and probe inside *its* window.
        let at = new.created_at + chrono::Duration::seconds(10);
        store.supersede(new.id, None, at).await.unwrap();
        let invalidated = s.service.get(new.id).await.unwrap().invalid_at.unwrap();

        let result = s
            .search_memory(Parameters(SearchMemoryParams {
                query: "postgres".into(),
                kind: None,
                tags: None,
                project: Some("p1".into()),
                global: None,
                limit: None,
                include_superseded: None,
                as_of: Some(invalidated - chrono::Duration::seconds(1)),
            }))
            .await
            .expect("protocol-level call must not fail");
        assert!(
            result_text(&result).contains("storage is postgres"),
            "as_of inside the validity window must return the fact believed then, got: {}",
            result_text(&result)
        );

        // And at the invalidation instant it is gone: the predicate is
        // `invalid_at > T`, so T == invalid_at excludes it. This is what proves
        // `as_of` is actually reaching the store rather than being ignored.
        let after = s
            .search_memory(Parameters(SearchMemoryParams {
                query: "postgres".into(),
                kind: None,
                tags: None,
                project: Some("p1".into()),
                global: None,
                limit: None,
                include_superseded: None,
                as_of: Some(invalidated),
            }))
            .await
            .expect("protocol-level call must not fail");
        assert!(
            !result_text(&after).contains("storage is postgres"),
            "at the invalidation instant the fact is already dead, got: {}",
            result_text(&after)
        );
    }

    /// A retracted decision must not read as authoritative. Without a marker,
    /// `include_superseded` hands the agent a flat list in which the dead fact
    /// and the live one look identical, and the agent can quote either.
    #[tokio::test]
    async fn a_superseded_hit_is_marked_and_a_live_one_is_not() {
        let s = server();

        let old = s
            .service
            .add(
                "storage is sqlite",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        s.service
            .add(
                "storage is postgres",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(old.id),
            )
            .await
            .unwrap();

        let result = s
            .search_memory(Parameters(SearchMemoryParams {
                query: "storage".into(),
                kind: None,
                tags: None,
                project: Some("p1".into()),
                global: None,
                limit: None,
                include_superseded: Some(true),
                as_of: None,
            }))
            .await
            .expect("protocol-level call must not fail");
        let text = result_text(&result);

        // Both are present, but only the dead one carries the marker. Assert per
        // line rather than on the whole blob: a marker anywhere would otherwise
        // satisfy a naive `contains`, including one wrongly attached to the live
        // fact.
        let dead_line = text
            .lines()
            .find(|l| l.contains(&old.id.to_string()))
            .unwrap_or_else(|| panic!("the superseded memory must be listed: {text}"));
        assert!(
            dead_line.contains("superseded:"),
            "a superseded hit must be marked so it cannot be quoted as current: {dead_line}"
        );
        // The invalidation date, so the agent can tell *when* it stopped being
        // true rather than only that it did.
        let invalid_at = s.service.get(old.id).await.unwrap().invalid_at.unwrap();
        assert!(
            dead_line.contains(&invalid_at.format("%Y-%m-%d").to_string()),
            "the marker must carry the date it stopped being true: {dead_line}"
        );

        // A hit spans two lines -- content, then metadata -- and the marker
        // lives on the second, so check the live memory's whole entry.
        let live_entry = text
            .split("\n\n")
            .find(|e| e.contains("storage is postgres"))
            .unwrap_or_else(|| panic!("the live memory must be listed: {text}"));
        assert!(
            !live_entry.contains("superseded:"),
            "the current fact must not be marked: {live_entry}"
        );
    }

    /// An ordinary search renders exactly as it did before supersession
    /// existed: the marker costs nothing when nothing is dead.
    #[tokio::test]
    async fn a_live_only_search_renders_without_any_marker() {
        let s = server();
        s.service
            .add(
                "storage is postgres",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();

        let result = s
            .search_memory(Parameters(SearchMemoryParams {
                query: "storage".into(),
                kind: None,
                tags: None,
                project: Some("p1".into()),
                global: None,
                limit: None,
                include_superseded: None,
                as_of: None,
            }))
            .await
            .expect("protocol-level call must not fail");
        let text = result_text(&result);
        assert!(text.contains("storage is postgres"), "got: {text}");
        assert!(
            !text.contains("superseded"),
            "a live hit must render exactly as before: {text}"
        );
    }

    /// An agent that sets both must be told why, in the tool result rather
    /// than as an opaque protocol failure.
    #[tokio::test]
    async fn search_rejects_as_of_with_include_superseded() {
        let s = server();
        let result = s
            .search_memory(Parameters(SearchMemoryParams {
                query: "x".into(),
                kind: None,
                tags: None,
                project: Some("p1".into()),
                global: None,
                limit: None,
                include_superseded: Some(true),
                as_of: Some(chrono::Utc::now()),
            }))
            .await
            .expect("protocol-level call must not fail");

        assert!(
            result.is_error.unwrap_or(false),
            "the contradiction must surface as a tool error"
        );
        let text = result_text(&result);
        assert!(text.contains("as_of"), "got: {text}");
        assert!(text.contains("include_superseded"), "got: {text}");
    }

    /// Superseding a memory in another project is refused, and the refusal
    /// reaches the caller as readable text rather than a bare failure.
    #[tokio::test]
    async fn supersede_across_projects_is_a_readable_tool_error() {
        let s = server();
        let elsewhere = s
            .service
            .add(
                "storage is sqlite",
                Kind::Decision,
                vec![],
                Some("p2".into()),
                None,
            )
            .await
            .unwrap();

        let result = s
            .add_memory(Parameters(AddMemoryParams {
                content: "storage is postgres".into(),
                kind: Kind::Decision,
                tags: None,
                project: Some("p1".into()),
                supersedes: Some(elsewhere.id),
            }))
            .await
            .expect("protocol-level call must not fail");

        assert!(result.is_error.unwrap_or(false));
        let text = result_text(&result);
        assert!(text.contains("p2") && text.contains("p1"), "got: {text}");
    }
}
