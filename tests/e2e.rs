//! End-to-end test: spawns the real compiled `mem8` binary and speaks the
//! actual MCP wire protocol (newline-delimited JSON-RPC) over its stdio.
//! This is the only test that proves the contract a real MCP client sees.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for a single reply before treating the server as hung.
/// Generous, but finite: a real hang must not stall the whole suite forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    fn start(db: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mem8"))
            .arg("serve")
            .env("MEM8_DB", format!("sqlite://{}", db.display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the mem8 binary");

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, next_id: 1 }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;

        let message = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();

        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            // Fail fast instead of blocking forever in `read_line` if the
            // server has died or is never going to answer.
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "server process exited (status: {status}) while awaiting a reply to {method}"
                );
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "timed out after {REPLY_TIMEOUT:?} waiting for a reply to {method}; \
                     server did not answer in time (possible stdout corruption or protocol mismatch)"
                );
            }

            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).unwrap();
            assert!(read > 0, "server closed stdout while awaiting a reply to {method}");

            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return value;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method });
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_then_add_then_search_over_real_stdio() {
    let dir = std::env::temp_dir().join(format!("mem8-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("e2e.db");

    let mut server = Server::start(&db);

    // Initialize. rmcp 0.9.1's `ProtocolVersion` recognizes "2025-06-18" as
    // `V_2025_06_18` (see model.rs) even though its own default/LATEST is
    // pinned to "2025-03-26" pending full 2025-06-18 compliance; the server
    // does not reject a client offering a version it recognizes.
    let init = server.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "mem8-e2e", "version": "0.1.0" }
        }),
    );
    assert!(init.get("result").is_some(), "initialize failed: {init}");

    // The handshake must advertise the tools capability. A client that trusts
    // it — as the specification says it should — will not register any tools
    // without this, even though `tools/list` below would answer correctly.
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "initialize must advertise the tools capability, got: {}",
        init["result"]["capabilities"]
    );

    // And identify this server, not the SDK it happens to be built on.
    assert_eq!(
        init["result"]["serverInfo"]["name"].as_str(),
        Some("mem8"),
        "initialize should name mem8, got: {}",
        init["result"]["serverInfo"]
    );

    server.notify("notifications/initialized");

    // All five tools are advertised.
    let listed = server.request("tools/list", serde_json::json!({}));
    let tools = listed["result"]["tools"].as_array().expect("tools/list returned no array");
    let names: Vec<String> =
        tools.iter().map(|t| t["name"].as_str().unwrap_or_default().to_string()).collect();

    for expected in [
        "add_memory",
        "search_memory",
        "get_memory",
        "update_memory",
        "delete_memory",
    ] {
        assert!(names.contains(&expected.to_string()), "missing tool {expected} in {names:?}");
    }

    // `AddMemoryParams::kind` is now the real `Kind` enum, so schemars must
    // derive a closed schema listing the five lowercase variants -- a real
    // MCP client can introspect and validate against this without reading
    // prose. Print the raw schema so `cargo test -- --nocapture` shows
    // exactly what schemars 1.x emits for a fieldless enum with
    // `#[serde(rename_all = "lowercase")]`.
    let add_memory_tool =
        tools.iter().find(|t| t["name"] == "add_memory").expect("add_memory tool must be listed");
    let input_schema = &add_memory_tool["inputSchema"];
    let kind_schema = &input_schema["properties"]["kind"];
    println!("add_memory inputSchema:\n{}", serde_json::to_string_pretty(input_schema).unwrap());
    println!("kind schema:\n{}", serde_json::to_string_pretty(kind_schema).unwrap());

    // schemars 1.x hoists the fieldless enum into `$defs` and leaves a
    // `$ref` on the field itself, so resolve the ref to reach the `enum`
    // array.
    let kind_ref = kind_schema["$ref"]
        .as_str()
        .unwrap_or_else(|| panic!("kind schema has no '$ref': {kind_schema}"));
    let def_name = kind_ref.rsplit('/').next().unwrap();
    let kind_def = &input_schema["$defs"][def_name];
    let enum_values: Vec<String> = kind_def["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("Kind $def has no 'enum' array: {kind_def}"))
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();
    let expected = ["decision", "preference", "convention", "fact", "learning"];
    assert_eq!(
        enum_values, expected,
        "add_memory's kind enum no longer lists exactly the five expected values, in order: {kind_schema}"
    );

    // An invalid kind now fails during rmcp's own JSON deserialization,
    // before the tool handler runs -- that's a protocol-level error
    // (JSON-RPC `error`, not a successful `result` carrying a tool error).
    // Confirm the message a real client receives is still intelligible and
    // still names the valid values.
    let bad_kind = server.request(
        "tools/call",
        serde_json::json!({
            "name": "add_memory",
            "arguments": { "content": "x", "kind": "banana", "project": "p1" }
        }),
    );
    println!("invalid kind response:\n{}", serde_json::to_string_pretty(&bad_kind).unwrap());
    assert!(bad_kind.get("error").is_some(), "expected a protocol-level error: {bad_kind}");
    let message = bad_kind["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("banana"), "error message should name the bad value: {message}");
    for k in expected {
        assert!(message.contains(k), "error message should still document '{k}': {message}");
    }

    // Store a memory.
    let added = server.request(
        "tools/call",
        serde_json::json!({
            "name": "add_memory",
            "arguments": {
                "content": "The e2e test spawns the real binary.",
                "kind": "fact",
                "project": "e2e"
            }
        }),
    );
    assert!(added.get("result").is_some(), "add_memory failed: {added}");

    // Find it again.
    let found = server.request(
        "tools/call",
        serde_json::json!({
            "name": "search_memory",
            "arguments": { "query": "binary", "project": "e2e" }
        }),
    );
    let text = serde_json::to_string(&found["result"]).unwrap();
    assert!(text.contains("spawns the real binary"), "search returned: {text}");

    // Clean up: drop the server first so the child process (and its open
    // SQLite handle) is gone before we try to remove the temp directory --
    // on Windows the db file cannot be deleted while the child holds it.
    drop(server);
    std::fs::remove_dir_all(&dir).ok();
}
