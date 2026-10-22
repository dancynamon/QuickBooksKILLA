//! Hand-rolled JSON-RPC 2.0 over newline-delimited stdin/stdout — MCP's own
//! stdio transport — shared by every MCP server in this workspace rather than
//! copied per binary. `ROADMAP.md` §G ("Channel intake") and §D.
//!
//! One line in, one line out: `serde_json` is already a dependency of every
//! caller and there is no other reason to carry an async runtime for a
//! request/response protocol this small. [`Server::handle_line`] is the whole
//! protocol surface and touches no I/O itself, so it is unit-tested directly
//! against a stub [`ToolSet`] in this crate; [`serve_stdio`] is the loop that
//! gives it real stdin and stdout.
//!
//! What lives here: JSON-RPC 2.0 line framing and error codes, and the MCP
//! `initialize` / `notifications/initialized` / `ping` / `tools/list` /
//! `tools/call` methods. What does not: any particular tool. A server is
//! this crate's [`Server`] parameterised over a caller-supplied [`ToolSet`]
//! that knows its own tools and how to run them — `qbo-local`'s read-only
//! query API, `ledger`'s reads and gated writes, and anything built on this
//! crate later all plug in the same way.

use serde_json::{json, Value};

/// The MCP protocol revision every server built on this crate speaks.
/// [`Server::handle_initialize`] echoes the client's own `protocolVersion`
/// back when it matches this; otherwise it states this one and lets the
/// client decide whether that is workable, rather than pretending
/// compatibility it cannot promise.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// A tool a [`ToolSet`] advertises through `tools/list`, in the shape MCP
/// wants on the wire.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

impl ToolSpec {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        })
    }
}

/// A tool-level failure: the *call* reached the server and was refused or
/// could not be satisfied (a bad argument, an id that does not exist, a
/// business rule the caller tripped). This is never a JSON-RPC error — a
/// [`ToolSet::call`] that returns this becomes a successful `tools/call`
/// response with `isError: true`, per the MCP spec, so the model sees the
/// message as normal tool output rather than a transport fault.
#[derive(Debug)]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        ToolError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

/// What a server built on this crate exposes: its tools, and how to run one.
/// `call` takes `&mut self` because a write server (`ledger-mcp`) changes
/// state a call makes; a read-only server (`qbo-local-mcp`) simply never
/// needs the mutability it is handed.
pub trait ToolSet {
    fn tools(&self) -> Vec<ToolSpec>;
    fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError>;
}

/// The fixed identity a server states in `initialize`'s `serverInfo` — the
/// binary's own name and `CARGO_PKG_VERSION`, which this crate cannot know on
/// a caller's behalf.
#[derive(Clone, Copy, Debug)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// The server: a [`ToolSet`] spoken to over JSON-RPC 2.0. Owns nothing about
/// the protocol beyond its own identity — no client state survives a call,
/// so `initialize` is not tracked and every method is available from the
/// first line read.
pub struct Server<T: ToolSet> {
    tool_set: T,
    info: ServerInfo,
}

impl<T: ToolSet> Server<T> {
    pub fn new(tool_set: T, info: ServerInfo) -> Self {
        Server { tool_set, info }
    }

    /// Reach the tool set directly — for a caller that needs to inspect or
    /// drive state the protocol surface has no method for (a test flipping
    /// an internal flag, say). Ordinary protocol handling never needs this.
    pub fn tool_set(&self) -> &T {
        &self.tool_set
    }

    /// Handle one line of input and produce at most one line of output.
    ///
    /// `None` means exactly what JSON-RPC 2.0 says it means for a
    /// notification: the caller sent a request with no `id`, and this server
    /// must not reply — not even with an error, per spec, since there is
    /// nothing to correlate a reply to.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                return Some(error_line(
                    Value::Null,
                    -32700,
                    format!("parse error: {err}"),
                ))
            }
        };

        let has_id = value.get("id").is_some();
        let id = value.get("id").cloned().unwrap_or(Value::Null);

        let Some(method) = value.get("method").and_then(Value::as_str) else {
            return has_id.then(|| error_line(id, -32600, "invalid request: missing \"method\""));
        };

        if method == "notifications/initialized" {
            return None;
        }

        let params = value.get("params").cloned().unwrap_or(Value::Null);

        let outcome = match method {
            "initialize" => Ok(self.handle_initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.handle_tools_list()),
            "tools/call" => Ok(self.handle_tools_call(&params)),
            other => Err((-32601, format!("method not found: {other}"))),
        };

        if !has_id {
            // A notification for a method this server does happen to know —
            // still no reply; JSON-RPC 2.0's "no id, no response" rule does
            // not carve out an exception for a recognised method.
            return None;
        }

        Some(match outcome {
            Ok(result) => success_line(id, result),
            Err((code, message)) => error_line(id, code, message),
        })
    }

    fn handle_initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let protocol_version = match requested {
            Some(version) if version == PROTOCOL_VERSION => version.to_string(),
            _ => PROTOCOL_VERSION.to_string(),
        };
        json!({
            "protocolVersion": protocol_version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": self.info.name,
                "version": self.info.version,
            },
        })
    }

    fn handle_tools_list(&self) -> Value {
        json!({ "tools": self.tool_set.tools().iter().map(ToolSpec::to_json).collect::<Vec<_>>() })
    }

    fn handle_tools_call(&mut self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        match self.tool_set.call(name, arguments) {
            Ok(result) => {
                let text = serde_json::to_string_pretty(&result)
                    .expect("a typed tool result always serialises");
                json!({
                    "content": [ { "type": "text", "text": text } ],
                    "structuredContent": result,
                })
            }
            Err(err) => json!({
                "content": [ { "type": "text", "text": err.message } ],
                "isError": true,
            }),
        }
    }
}

fn success_line(id: Value, result: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
    .expect("a JSON-RPC response always serialises")
}

fn error_line(id: Value, code: i64, message: impl Into<String>) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    }))
    .expect("a JSON-RPC response always serialises")
}

/// The stdio loop around a [`Server`]: read newline-delimited JSON-RPC
/// requests from stdin, write at most one response line per request to
/// stdout, flushing every line so a client reading synchronously never
/// blocks waiting for a buffer to fill. Diagnostics go to stderr — stdout
/// ever carries only JSON-RPC response lines, since that is the wire a
/// client reads. Exits the process (status 0) once stdin reaches EOF, or
/// (status 1) on an I/O error reading or writing — there is nothing left for
/// a caller of this function to do with either outcome, which is why it
/// never returns.
pub fn serve_stdio<T: ToolSet>(mut server: Server<T>) -> ! {
    use std::io::{self, BufRead, Write};

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                eprintln!("stdin read error: {err}");
                std::process::exit(1);
            }
        };
        if let Some(response) = server.handle_line(&line) {
            if out.write_all(response.as_bytes()).is_err()
                || out.write_all(b"\n").is_err()
                || out.flush().is_err()
            {
                eprintln!("stdout write error");
                std::process::exit(1);
            }
        }
    }

    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal [`ToolSet`] carrying one tool, `echo`, which returns its
    /// own arguments, plus a way to force a tool error — enough to exercise
    /// every protocol path in this crate without any real domain behind it.
    struct StubToolSet;

    impl ToolSet for StubToolSet {
        fn tools(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "echo",
                description: "Echoes its arguments back.",
                input_schema: json!({ "type": "object" }),
            }]
        }

        fn call(&mut self, name: &str, args: Value) -> Result<Value, ToolError> {
            match name {
                "echo" => Ok(args),
                "fail" => Err(ToolError::new("deliberate failure")),
                other => Err(ToolError::new(format!("unknown tool: {other}"))),
            }
        }
    }

    fn server() -> Server<StubToolSet> {
        Server::new(
            StubToolSet,
            ServerInfo {
                name: "stub",
                version: "0.0.0",
            },
        )
    }

    fn call(server: &mut Server<StubToolSet>, id: i64, method: &str, params: Value) -> Value {
        let line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        let response = server
            .handle_line(&line)
            .expect("a request always gets a response");
        serde_json::from_str(&response).unwrap()
    }

    #[test]
    fn initialize_echoes_a_supported_protocol_version_and_names_the_server() {
        let mut server = server();
        let response = call(
            &mut server,
            1,
            "initialize",
            json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": { "name": "x", "version": "0" } }),
        );
        assert_eq!(response["id"], json!(1));
        assert_eq!(
            response["result"]["protocolVersion"],
            json!(PROTOCOL_VERSION)
        );
        assert_eq!(response["result"]["capabilities"], json!({ "tools": {} }));
        assert_eq!(response["result"]["serverInfo"]["name"], json!("stub"));
        assert_eq!(response["result"]["serverInfo"]["version"], json!("0.0.0"));
    }

    #[test]
    fn initialize_states_its_own_version_when_the_client_asks_for_one_it_does_not_know() {
        let mut server = server();
        let response = call(
            &mut server,
            1,
            "initialize",
            json!({ "protocolVersion": "1999-01-01" }),
        );
        assert_eq!(
            response["result"]["protocolVersion"],
            json!(PROTOCOL_VERSION)
        );
    }

    #[test]
    fn ping_returns_an_empty_result() {
        let mut server = server();
        let response = call(&mut server, 1, "ping", json!({}));
        assert_eq!(response["result"], json!({}));
    }

    #[test]
    fn a_notification_gets_no_response() {
        let mut server = server();
        let line = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        assert_eq!(server.handle_line(&line), None);

        // Even a notification naming a real method gets nothing back — the
        // "no id, no reply" rule has no carve-out for a method that exists.
        let line = json!({ "jsonrpc": "2.0", "method": "ping" }).to_string();
        assert_eq!(server.handle_line(&line), None);
    }

    #[test]
    fn malformed_json_is_a_parse_error_with_a_null_id() {
        let mut server = server();
        let response = server.handle_line("{ not json").unwrap();
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], json!(-32700));
        assert_eq!(response["id"], Value::Null);
    }

    #[test]
    fn a_request_missing_method_is_an_invalid_request() {
        let mut server = server();
        let response = call_raw(&mut server, json!({ "jsonrpc": "2.0", "id": 3 }));
        assert_eq!(response["error"]["code"], json!(-32600));
        assert_eq!(response["id"], json!(3));
    }

    fn call_raw(server: &mut Server<StubToolSet>, request: Value) -> Value {
        let response = server
            .handle_line(&request.to_string())
            .expect("a request with an id always gets a response");
        serde_json::from_str(&response).unwrap()
    }

    #[test]
    fn an_unknown_method_is_method_not_found() {
        let mut server = server();
        let response = call(&mut server, 7, "not/a/real/method", json!({}));
        assert_eq!(response["error"]["code"], json!(-32601));
        assert_eq!(response["id"], json!(7));
    }

    #[test]
    fn an_empty_line_is_silently_ignored() {
        let mut server = server();
        assert_eq!(server.handle_line(""), None);
        assert_eq!(server.handle_line("   "), None);
    }

    #[test]
    fn tools_list_carries_every_tool_with_a_schema() {
        let mut server = server();
        let response = call(&mut server, 1, "tools/list", json!({}));
        let listed = response["result"]["tools"].as_array().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["name"], json!("echo"));
        assert!(listed[0]["description"]
            .as_str()
            .is_some_and(|d| !d.is_empty()));
        assert_eq!(listed[0]["inputSchema"]["type"], json!("object"));
    }

    #[test]
    fn a_successful_tool_call_carries_content_and_structured_content() {
        let mut server = server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({ "name": "echo", "arguments": { "x": 1 } }),
        );
        assert_ne!(response["result"]["isError"], json!(true));
        assert_eq!(response["result"]["structuredContent"], json!({ "x": 1 }));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains('1'));
    }

    #[test]
    fn a_tool_error_is_a_successful_response_with_is_error_true_not_a_json_rpc_error() {
        let mut server = server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({ "name": "fail", "arguments": {} }),
        );
        assert!(
            response.get("error").is_none(),
            "must not be a JSON-RPC error"
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "deliberate failure");
    }

    #[test]
    fn an_unknown_tool_is_a_tool_error_not_a_json_rpc_error() {
        let mut server = server();
        let response = call(
            &mut server,
            1,
            "tools/call",
            json!({ "name": "not_a_tool", "arguments": {} }),
        );
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["isError"], json!(true));
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not_a_tool"));
    }
}
