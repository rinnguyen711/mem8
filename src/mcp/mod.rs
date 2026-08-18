use crate::core::Memory8;
#[allow(unused_imports)]
use crate::error::Mem8Error;
use crate::model::{Kind, SearchHit};
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
            // Results already arrive best-first, but without the score every hit
            // reads as equally relevant, so a vague query looks the same as an
            // exact one. Showing it lets the caller tell a strong match from the
            // long tail below it.
            format!(
                "[{}] ({}, score {:.3}) {}\n  id: {}  project: {}  tags: {}",
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
                }
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
                // Task 9 wires this to a real `supersedes` parameter.
                .add(
                    &p.content,
                    p.kind,
                    p.tags.unwrap_or_default(),
                    p.project,
                    None,
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
                    p.project,
                    p.global.unwrap_or(false),
                    p.kind,
                    p.tags.unwrap_or_default(),
                    p.limit,
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
    use crate::store::MemStore;
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
}
