//! The HTTP transport, against a real bound socket.
//!
//! Auth is only worth anything if it holds on the wire. Unit tests cover the
//! token comparison and the TLS guard as pure functions; this file starts an
//! actual server, connects to it over TCP, and checks that the middleware
//! rejects what it should and lets the MCP protocol through when it should.
//!
//! Needs the `http` feature. SQLite in a temporary file backs it, so no
//! database server is required.

#![cfg(feature = "http")]

use mem8::core::{Memory8, ScopeMode};
use mem8::http::auth::Token;
use mem8::store::sqlite::SqliteStore;
use std::sync::Arc;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Start a server on an ephemeral port and return its base URL.
///
/// Mirrors `serve_http`'s wiring — the same router, the same middleware, the
/// same `Explicit` scope mode — but binds port 0 and hands back the address, so
/// tests need no fixed port and can run in parallel.
async fn spawn_server() -> String {
    let dir = std::env::temp_dir().join(format!("mem8-http-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = SqliteStore::open(&dir.join("mem8.db")).unwrap();

    let service = Arc::new(Memory8::new(Arc::new(store)).with_scope_mode(ScopeMode::Explicit));
    let token = Token::new(TOKEN).unwrap();

    let app = mem8::http::router_for_tests(service, token);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://{addr}")
}

/// A minimal MCP request. Only the transport is under test here, so this speaks
/// JSON-RPC directly rather than pulling in a client library.
fn initialize_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    })
}

async fn post(url: &str, token: Option<&str>, body: serde_json::Value) -> (u16, String) {
    let client = reqwest::Client::new();
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        // The streamable-HTTP transport requires the client to accept both.
        .header("accept", "application/json, text/event-stream")
        .json(&body);

    if let Some(t) = token {
        request = request.header("authorization", format!("Bearer {t}"));
    }

    let response = request.send().await.expect("the server must be reachable");
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    (status, text)
}

#[tokio::test]
async fn a_request_with_no_token_is_rejected() {
    let base = spawn_server().await;
    let (status, _) = post(&format!("{base}/mcp"), None, initialize_body()).await;
    assert_eq!(
        status, 401,
        "an unauthenticated request must not reach the service"
    );
}

#[tokio::test]
async fn a_request_with_the_wrong_token_is_rejected() {
    let base = spawn_server().await;
    let wrong = "ffffffffffffffffffffffffffffffff";
    let (status, _) = post(&format!("{base}/mcp"), Some(wrong), initialize_body()).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_prefix_of_the_token_is_rejected() {
    // The case a `starts_with` comparison would wrongly accept.
    let base = spawn_server().await;
    let (status, _) = post(
        &format!("{base}/mcp"),
        Some("0123456789abcdef"),
        initialize_body(),
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_rejection_does_not_reveal_why() {
    // "no header" and "wrong token" must be indistinguishable, so a probe
    // cannot learn that a guessed token was well-formed.
    let base = spawn_server().await;

    let (no_token_status, no_token_body) =
        post(&format!("{base}/mcp"), None, initialize_body()).await;
    let (bad_token_status, bad_token_body) = post(
        &format!("{base}/mcp"),
        Some("ffffffffffffffffffffffffffffffff"),
        initialize_body(),
    )
    .await;

    assert_eq!(no_token_status, bad_token_status);
    assert_eq!(no_token_body, bad_token_body);
}

#[tokio::test]
async fn the_health_endpoint_also_requires_authentication() {
    // It is inside the auth layer deliberately: an unauthenticated endpoint is
    // a way to confirm that a mem8 server is listening here.
    let base = spawn_server().await;
    let response = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(response.status().as_u16(), 401);
}

#[tokio::test]
async fn a_valid_token_completes_the_mcp_handshake() {
    let base = spawn_server().await;
    let (status, body) = post(&format!("{base}/mcp"), Some(TOKEN), initialize_body()).await;

    assert_eq!(
        status, 200,
        "an authenticated handshake must succeed; body: {body}"
    );
    assert!(
        body.contains("mem8"),
        "the server should identify itself in the handshake; body: {body}"
    );
    assert!(
        body.contains("tools"),
        "the tools capability must be advertised; body: {body}"
    );
}

/// A tool call, once the session is established.
///
/// The streamable-HTTP transport hands back a session id on `initialize`, and
/// every later request must carry it.
async fn call_tool(base: &str, session: &str, name: &str, arguments: serde_json::Value) -> String {
    let (_, body) = post_with_session(
        &format!("{base}/mcp"),
        Some(TOKEN),
        Some(session),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }),
    )
    .await;
    body
}

async fn post_with_session(
    url: &str,
    token: Option<&str>,
    session: Option<&str>,
    body: serde_json::Value,
) -> (u16, String) {
    let client = reqwest::Client::new();
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&body);

    if let Some(t) = token {
        request = request.header("authorization", format!("Bearer {t}"));
    }
    if let Some(s) = session {
        request = request.header("mcp-session-id", s);
    }

    let response = request.send().await.expect("the server must be reachable");
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    (status, text)
}

/// Establish a session and return its id.
async fn initialize(base: &str) -> String {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&initialize_body())
        .send()
        .await
        .unwrap();

    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("the server must issue a session id")
        .to_string();

    // The protocol requires this before any tool call.
    let _ = post_with_session(
        &format!("{base}/mcp"),
        Some(TOKEN),
        Some(&session),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;

    session
}

/// The scope hole this transport exists to avoid.
///
/// A server's working directory is its own, so it cannot infer which project a
/// remote caller means. Without `Explicit` mode every client's memories would
/// land in one scope named after the container's directory, silently. This
/// asserts the refusal reaches the wire, not merely the unit test.
#[tokio::test]
async fn a_tool_call_without_a_project_is_refused_over_http() {
    let base = spawn_server().await;
    let session = initialize(&base).await;

    let body = call_tool(
        &base,
        &session,
        "add_memory",
        serde_json::json!({"content": "no project named", "kind": "fact"}),
    )
    .await;

    assert!(
        body.contains("project"),
        "the refusal must name the missing field; body: {body}"
    );
}

#[tokio::test]
async fn a_tool_call_naming_its_project_succeeds_over_http() {
    let base = spawn_server().await;
    let session = initialize(&base).await;

    let body = call_tool(
        &base,
        &session,
        "add_memory",
        serde_json::json!({
            "content": "we chose rust for the binary",
            "kind": "decision",
            "project": "explicitly-named"
        }),
    )
    .await;

    assert!(
        body.contains("explicitly-named"),
        "an explicit project must be accepted and reported back; body: {body}"
    );

    // And the memory is findable again through the same transport.
    let found = call_tool(
        &base,
        &session,
        "search_memory",
        serde_json::json!({"query": "rust", "project": "explicitly-named"}),
    )
    .await;

    assert!(
        found.contains("we chose rust"),
        "a memory written over HTTP must be searchable over HTTP; body: {found}"
    );
}

#[tokio::test]
async fn the_unauthorized_response_says_how_to_authenticate() {
    let base = spawn_server().await;
    let response = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .json(&initialize_body())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer"),
        "a 401 should tell a client the scheme without revealing anything else"
    );
}
