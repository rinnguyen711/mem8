use crate::core::Memory8;
#[allow(unused_imports)]
use crate::error::Mem8Error;
use crate::model::{Kind, SearchHit};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AddMemoryParams {
    /// The memory to store, in full sentences.
    pub content: String,
    /// One of: decision, preference, convention, fact, learning.
    pub kind: String,
    /// Optional labels for filtering later.
    pub tags: Option<Vec<String>>,
    /// Overrides the auto-detected project scope.
    pub project: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchMemoryParams {
    /// Keywords to search for.
    pub query: String,
    /// Restrict results to one kind.
    pub kind: Option<String>,
    /// Only return memories carrying all of these tags.
    pub tags: Option<Vec<String>>,
    /// Overrides the auto-detected project scope.
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
    pub kind: Option<String>,
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

fn parse_kind(raw: &str) -> Result<Kind, CallToolResult> {
    Kind::from_str(raw).map_err(|e| fail(e.to_string()))
}

fn render_hits(hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return "No matching memories.".to_string();
    }
    hits.iter()
        .map(|h| {
            format!(
                "[{}] ({}) {}\n  id: {}  project: {}  tags: {}",
                h.memory.created_at.format("%Y-%m-%d"),
                h.memory.kind,
                h.memory.content,
                h.memory.id,
                h.memory.project,
                if h.memory.tags.is_empty() { "-".into() } else { h.memory.tags.join(", ") }
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
        Self { service, tool_router: Self::tool_router() }
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
        let kind = match parse_kind(&p.kind) {
            Ok(k) => k,
            Err(e) => return Ok(e),
        };

        Ok(match self
            .service
            .add(&p.content, kind, p.tags.unwrap_or_default(), p.project)
            .await
        {
            Ok(m) => ok(format!("Stored {} memory in '{}'. id: {}", m.kind, m.project, m.id)),
            Err(e) => fail(e.to_string()),
        })
    }

    #[tool(
        description = "Search stored memories by keyword. Scoped to the current project unless global is true."
    )]
    async fn search_memory(
        &self,
        Parameters(p): Parameters<SearchMemoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let kind = match p.kind.as_deref().map(parse_kind).transpose() {
            Ok(k) => k,
            Err(e) => return Ok(e),
        };

        Ok(match self
            .service
            .search(
                &p.query,
                p.project,
                p.global.unwrap_or(false),
                kind,
                p.tags.unwrap_or_default(),
                p.limit,
            )
            .await
        {
            Ok(hits) => ok(render_hits(&hits)),
            Err(e) => fail(e.to_string()),
        })
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
                if m.tags.is_empty() { "-".into() } else { m.tags.join(", ") },
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
        let kind = match p.kind.as_deref().map(parse_kind).transpose() {
            Ok(k) => k,
            Err(e) => return Ok(e),
        };

        Ok(match self.service.update(id, p.content, kind, p.tags).await {
            Ok(m) => ok(format!("Updated memory {}.", m.id)),
            Err(e) => fail(e.to_string()),
        })
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
impl ServerHandler for Mem8Server {}

/// Run the MCP server over stdio until the client disconnects.
pub async fn serve_stdio() -> anyhow::Result<()> {
    use rmcp::transport::io::stdio;
    use rmcp::ServiceExt;

    let store = crate::store::open_from_env().await?;
    let server = Mem8Server::new(Arc::new(Memory8::new(store)));

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
            kind: "decision".into(),
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
    }

    #[tokio::test]
    async fn unknown_kind_is_a_tool_error_not_a_panic() {
        let s = server();
        let result = s
            .add_memory(Parameters(AddMemoryParams {
                content: "something".into(),
                kind: "banana".into(),
                tags: None,
                project: Some("p1".into()),
            }))
            .await
            .expect("protocol-level call must not fail");

        assert!(result.is_error.unwrap_or(false), "expected a tool error");
        assert!(result_text(&result).contains("banana"));
    }

    #[tokio::test]
    async fn get_with_malformed_uuid_is_a_tool_error() {
        let s = server();
        let result = s
            .get_memory(Parameters(IdParams { id: "not-a-uuid".into() }))
            .await
            .expect("protocol-level call must not fail");
        assert!(result.is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn delete_of_unknown_id_reports_not_found() {
        let s = server();
        let result = s
            .delete_memory(Parameters(IdParams { id: uuid::Uuid::new_v4().to_string() }))
            .await
            .expect("protocol-level call must not fail");
        assert!(result.is_error.unwrap_or(false));
        assert!(result_text(&result).contains("not found"));
    }
}
