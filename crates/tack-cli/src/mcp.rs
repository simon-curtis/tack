//! MCP (Model Context Protocol) stdio server for `tack mcp`.
//!
//! Implements JSON-RPC 2.0 over newline-delimited stdin/stdout following the
//! MCP spec. All tracing/logging goes to stderr; stdout carries only protocol
//! JSON. EOF on stdin is the MCP stdio shutdown signal (exit 0).
//!
//! ## Architecture
//!
//! The testable core is [`serve_mcp`], which accepts any `BufRead`/`Write`
//! pair. Pure helpers (`tools_list`, `tool_input_schema`, `build_request`)
//! are unit-tested independently.

use std::io::{BufRead, Write};

use anyhow::Context;
use serde_json::{Value, json};
use tack_core::Repository;
use tack_core::api::{self, MethodInfo, Request};

// ── author fill ───────────────────────────────────────────────────────────────

/// Resolves the author identity for MCP tool calls, using the same priority
/// chain as `commands.rs::Author::from_env`:
/// * name  ← `TACK_AUTHOR_NAME` | `USERNAME` | `USER` | `"tack"`
/// * email ← `TACK_AUTHOR_EMAIL` | `name@COMPUTERNAME|HOSTNAME|"localhost"`
fn author_name_from_env() -> String {
    env_first(&["TACK_AUTHOR_NAME", "USERNAME", "USER"]).unwrap_or_else(|| "tack".to_owned())
}

fn author_email_from_env(name: &str) -> String {
    env_first(&["TACK_AUTHOR_EMAIL"]).unwrap_or_else(|| {
        let host =
            env_first(&["COMPUTERNAME", "HOSTNAME"]).unwrap_or_else(|| "localhost".to_owned());
        format!("{name}@{host}")
    })
}

/// Returns the first non-empty value among the named environment variables.
fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty())
}

// ── JSON Schema helpers ───────────────────────────────────────────────────────

/// The set of methods for which `author_name`/`author_email` are removed from
/// the `required` list (the MCP server fills them from the environment).
const AUTHOR_FILL_METHODS: &[&str] = &["named_cut", "scoped_cut", "backport", "backport_continue"];

/// Maps a tack param type string to its JSON Schema property object.
fn param_type_to_json_schema(ty: &str) -> Value {
    match ty {
        "bool" => json!({"type": "boolean"}),
        "[string]" => json!({"type": "array", "items": {"type": "string"}}),
        // "string" and "string?" both map to {"type":"string"}
        _ => json!({"type": "string"}),
    }
}

/// Builds the `inputSchema` JSON Schema object for a single [`MethodInfo`].
///
/// For zero-param methods: `{"type":"object","additionalProperties":false}`.
/// For methods with params: a full object schema with `properties`, `required`,
/// and `additionalProperties: false`.
///
/// For `named_cut` and `scoped_cut` the `author_name`/`author_email` fields are
/// kept as optional properties but removed from `required` (the server fills them).
#[must_use]
pub fn tool_input_schema(info: &MethodInfo) -> Value {
    if info.params.is_empty() {
        return json!({"type": "object", "additionalProperties": false});
    }

    let author_fill = AUTHOR_FILL_METHODS.contains(&info.method.as_str());

    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();

    for param in &info.params {
        let mut schema = param_type_to_json_schema(&param.ty);
        schema["description"] = Value::String(param.description.clone());
        properties.insert(param.name.clone(), schema);

        let is_author_field = param.name == "author_name" || param.name == "author_email";
        if param.required && !(author_fill && is_author_field) {
            required.push(Value::String(param.name.clone()));
        }
    }

    let mut schema = json!({
        "type": "object",
        "properties": Value::Object(properties),
        "additionalProperties": false,
    });

    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }

    schema
}

/// Builds the full `tools/list` result value (the `tools` array payload).
///
/// Tool names use the `tack_` prefix followed by the method name.
/// Only `[A-Za-z0-9_]` characters are allowed in tool names; `snake_case`
/// method names already satisfy this constraint.
#[must_use]
pub fn tools_list() -> Value {
    let methods = api::schema_methods();
    let tools: Vec<Value> = methods
        .iter()
        .map(|info| {
            let name = format!("tack_{}", info.method);
            let description = format!(
                "{} Returns: {}",
                info.summary.trim_end_matches('.'),
                info.returns
            );
            json!({
                "name": name,
                "description": description,
                "inputSchema": tool_input_schema(info),
            })
        })
        .collect();
    json!({"tools": tools})
}

// ── request building ──────────────────────────────────────────────────────────

/// Builds a [`Request`] from a method name and the tool-call `arguments` object.
///
/// Inserts `"method": method` into a clone of `args`, then deserialises through
/// serde. Returns `Err(description)` on deserialise failure.
///
/// # Errors
///
/// Returns a human-readable string describing the deserialisation failure.
pub fn build_request(method: &str, args: &Value) -> Result<Request, String> {
    let mut obj = args
        .as_object()
        .map_or_else(serde_json::Map::new, Clone::clone);
    obj.insert("method".to_owned(), Value::String(method.to_owned()));
    serde_json::from_value::<Request>(Value::Object(obj))
        .map_err(|e| format!("invalid arguments: {e}"))
}

// ── JSON-RPC wire helpers ─────────────────────────────────────────────────────

/// Writes a single compact JSON-RPC 2.0 response line and flushes `writer`.
fn write_rpc_line(writer: &mut impl Write, value: &Value) -> anyhow::Result<()> {
    let line = serde_json::to_string(value).context("failed to serialise JSON-RPC line")?;
    writer
        .write_all(line.as_bytes())
        .context("failed to write JSON-RPC line")?;
    writer.write_all(b"\n").context("failed to write newline")?;
    writer.flush().context("failed to flush writer")?;
    Ok(())
}

/// Constructs a JSON-RPC success response.
fn rpc_ok(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Constructs a JSON-RPC error response.
fn rpc_err(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Constructs a MCP tool-execution error result (NOT a JSON-RPC protocol error).
fn tool_error(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

/// Constructs a MCP tool-execution success result.
fn tool_ok(response_value: &Value) -> Value {
    let text = serde_json::to_string(response_value).unwrap_or_else(|_| "{}".to_owned());
    let is_error = response_value
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| s == "error");
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": response_value,
        "isError": is_error,
    })
}

// ── routing ───────────────────────────────────────────────────────────────────

/// Routes one parsed JSON-RPC request object and optionally writes a response.
///
/// Notifications (messages without an `id`) never produce a reply.
/// Returns `Ok(())` in all cases — errors are written as JSON-RPC error lines.
fn route(msg: &Value, writer: &mut impl Write) -> anyhow::Result<()> {
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // Malformed — no method field. If there's an id, report an error.
        if let Some(id) = msg.get("id") {
            write_rpc_line(
                writer,
                &rpc_err(id, -32_600, "invalid request: missing method"),
            )?;
        }
        return Ok(());
    };

    // Notifications have no `id` — never reply.
    let id = msg.get("id");
    let is_notification = id.is_none() || method.starts_with("notifications/");

    match method {
        "initialize" => {
            let Some(id) = id else {
                return Ok(());
            };
            let client_version = msg
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("2025-11-25");
            let result = json!({
                "protocolVersion": client_version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "tack", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "This repository is versioned with tack, not git. \
                                 Use these tack_* tools (and never shell `git`) \
                                 for version control.",
            });
            write_rpc_line(writer, &rpc_ok(id, &result))?;
        }
        "ping" => {
            let Some(id) = id else {
                return Ok(());
            };
            write_rpc_line(writer, &rpc_ok(id, &json!({})))?;
        }
        "tools/list" => {
            let Some(id) = id else {
                return Ok(());
            };
            let list = tools_list();
            write_rpc_line(writer, &rpc_ok(id, &list))?;
        }
        "tools/call" => {
            let Some(id) = id else {
                return Ok(());
            };
            handle_tool_call(msg, id, writer)?;
        }
        _ if is_notification => {
            // Notifications (including `notifications/initialized`) — no reply.
        }
        _ => {
            // Unknown method with an id → method-not-found.
            let Some(id) = id else {
                return Ok(());
            };
            let message = format!("Method not found: {method}");
            write_rpc_line(writer, &rpc_err(id, -32_601, &message))?;
        }
    }

    Ok(())
}

/// Handles a `tools/call` request.
fn handle_tool_call(msg: &Value, id: &Value, writer: &mut impl Write) -> anyhow::Result<()> {
    let name = msg
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("");

    let Some(tack_method) = name.strip_prefix("tack_") else {
        let message = format!("Unknown tool: {name}");
        return write_rpc_line(writer, &rpc_err(id, -32_602, &message));
    };

    // Verify the method is known.
    let known_methods = api::schema_methods();
    if !known_methods.iter().any(|m| m.method == tack_method) {
        let message = format!("Unknown tool: {name}");
        return write_rpc_line(writer, &rpc_err(id, -32_602, &message));
    }

    let empty_obj = json!({});
    let args_raw = msg.pointer("/params/arguments").unwrap_or(&empty_obj);

    // Clone arguments so we can fill in author fields if needed.
    let mut args = args_raw.clone();
    maybe_fill_author(tack_method, &mut args);

    // Build the Request.
    let req = match build_request(tack_method, &args) {
        Ok(r) => r,
        Err(e) => {
            let err_val = tool_error(&e);
            return write_rpc_line(writer, &rpc_ok(id, &err_val));
        }
    };

    // Open the repo lazily from cwd.
    let Ok(cwd) = std::env::current_dir() else {
        let err_val = tool_error("failed to read current directory");
        return write_rpc_line(writer, &rpc_ok(id, &err_val));
    };
    let Ok(repo) = Repository::open(&cwd) else {
        let err_val = tool_error(&format!("no tack repository at {}", cwd.display()));
        return write_rpc_line(writer, &rpc_ok(id, &err_val));
    };

    let response = api::handle(&repo, req);
    let response_value = serde_json::to_value(&response)
        .unwrap_or_else(|_| json!({"status": "error", "message": "serialization failed"}));
    let result = tool_ok(&response_value);
    write_rpc_line(writer, &rpc_ok(id, &result))?;
    Ok(())
}

/// Fills `author_name` / `author_email` from the environment for methods that
/// require an author, if the fields are absent or empty in `args`.
fn maybe_fill_author(method: &str, args: &mut Value) {
    if !AUTHOR_FILL_METHODS.contains(&method) {
        return;
    }

    let Some(obj) = args.as_object_mut() else {
        return;
    };

    let name_missing = obj
        .get("author_name")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty);

    if name_missing {
        let name = author_name_from_env();
        let email = author_email_from_env(&name);
        obj.insert("author_name".to_owned(), Value::String(name));
        obj.insert("author_email".to_owned(), Value::String(email));
    } else {
        // Name is present; fill email if missing.
        let email_missing = obj
            .get("author_email")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if email_missing {
            let name = obj
                .get("author_name")
                .and_then(Value::as_str)
                .unwrap_or("tack")
                .to_owned();
            let email = author_email_from_env(&name);
            obj.insert("author_email".to_owned(), Value::String(email));
        }
    }
}

// ── public entry point ────────────────────────────────────────────────────────

/// Runs the MCP server loop, reading newline-delimited JSON-RPC from `reader`
/// and writing responses to `writer`. Returns when the input stream closes.
///
/// Logs and tracing go to stderr; `writer` carries only protocol JSON.
///
/// # Errors
///
/// Returns an error only on fatal I/O failures (not on bad JSON or unknown
/// methods — those are reported as JSON-RPC error lines and the loop continues).
pub fn serve_mcp<R: BufRead, W: Write>(mut reader: R, writer: &mut W) -> anyhow::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .context("failed to read from stdin")?;
        if read == 0 {
            return Ok(()); // EOF — MCP stdio shutdown signal.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue; // Skip blank lines.
        }

        match serde_json::from_str::<Value>(trimmed) {
            Ok(msg) => {
                if let Err(e) = route(&msg, writer) {
                    tracing::warn!("route error: {e}");
                }
            }
            Err(e) => {
                // Malformed JSON — report parse error with id:null.
                tracing::debug!("JSON parse error: {e}");
                let err_resp = rpc_err(&Value::Null, -32_700, &format!("parse error: {e}"));
                let _ = write_rpc_line(writer, &err_resp);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use tack_core::Repository;
    use tempfile::TempDir;

    use super::*;

    // ── RAII guards ───────────────────────────────────────────────────────────

    /// Restores the process working directory on drop.
    struct CwdGuard(PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    /// Removes `TACK_AUTHOR_NAME` / `TACK_AUTHOR_EMAIL` from the environment on drop.
    struct AuthorEnvGuard;
    impl Drop for AuthorEnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("TACK_AUTHOR_NAME");
                std::env::remove_var("TACK_AUTHOR_EMAIL");
            }
        }
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn make_tack_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(dir.path()).expect("init repo");
        dir
    }

    /// Changes to `dir`, returning a guard that restores the original cwd.
    fn chdir(dir: &std::path::Path) -> CwdGuard {
        let original = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(dir).expect("chdir");
        CwdGuard(original)
    }

    /// Runs `serve_mcp` with `input` as the reader and returns the output lines
    /// as a `Vec<Value>`.
    fn run_session(input: &str) -> Vec<Value> {
        let reader = Cursor::new(input.as_bytes());
        let mut output: Vec<u8> = Vec::new();
        serve_mcp(reader, &mut output).expect("serve_mcp");
        let text = String::from_utf8(output).expect("utf8 output");
        text.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect("parse output line"))
            .collect()
    }

    // ── tool_input_schema ─────────────────────────────────────────────────────

    #[test]
    fn tool_input_schema_no_arg_method_returns_minimal_schema() {
        let methods = api::schema_methods();
        let status = methods
            .iter()
            .find(|m| m.method == "status")
            .expect("status method");
        let schema = tool_input_schema(status);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            schema.get("properties").is_none(),
            "no-arg method must not have properties"
        );
        assert!(
            schema.get("required").is_none(),
            "no-arg method must not have required"
        );
    }

    #[test]
    fn tool_input_schema_diff_method_has_correct_types() {
        let methods = api::schema_methods();
        let diff = methods
            .iter()
            .find(|m| m.method == "diff")
            .expect("diff method");
        let schema = tool_input_schema(diff);

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);

        // `from` and `to` are optional strings.
        assert_eq!(schema["properties"]["from"]["type"], "string");
        assert_eq!(schema["properties"]["to"]["type"], "string");
        // `stat` and `patch` are booleans.
        assert_eq!(schema["properties"]["stat"]["type"], "boolean");
        assert_eq!(schema["properties"]["patch"]["type"], "boolean");

        // None of these are required (all optional in diff).
        let required = schema.get("required");
        assert!(
            required.is_none() || required.unwrap().as_array().is_some_and(Vec::is_empty),
            "diff has no required params: {schema}"
        );
    }

    #[test]
    fn tool_input_schema_scoped_cut_author_fields_not_required() {
        let methods = api::schema_methods();
        let sc = methods
            .iter()
            .find(|m| m.method == "scoped_cut")
            .expect("scoped_cut method");
        let schema = tool_input_schema(sc);

        // `paths` and `message` are required.
        let required = schema["required"].as_array().expect("required array");
        let required_names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert!(
            required_names.contains(&"paths"),
            "paths must be required: {required_names:?}"
        );
        assert!(
            required_names.contains(&"message"),
            "message must be required: {required_names:?}"
        );

        // `author_name` and `author_email` must NOT be required (server fills them).
        assert!(
            !required_names.contains(&"author_name"),
            "author_name must not be required for scoped_cut"
        );
        assert!(
            !required_names.contains(&"author_email"),
            "author_email must not be required for scoped_cut"
        );

        // But the properties must still be present (optional override).
        assert!(
            schema["properties"].get("author_name").is_some(),
            "author_name must be a declared property"
        );
        assert!(
            schema["properties"].get("author_email").is_some(),
            "author_email must be a declared property"
        );

        // `paths` is `[string]`.
        assert_eq!(schema["properties"]["paths"]["type"], "array");
        assert_eq!(schema["properties"]["paths"]["items"]["type"], "string");
    }

    // ── tools_list ────────────────────────────────────────────────────────────

    #[test]
    fn tools_list_contains_all_methods_with_tack_prefix() {
        let list = tools_list();
        let tools = list["tools"].as_array().expect("tools array");

        let schema_methods = api::schema_methods();
        assert_eq!(
            tools.len(),
            schema_methods.len(),
            "tools/list must expose every schema method"
        );

        for method in &schema_methods {
            let expected_name = format!("tack_{}", method.method);
            let found = tools.iter().any(|t| t["name"] == expected_name);
            assert!(found, "missing tool {expected_name} in tools/list");
        }
    }

    #[test]
    fn tools_list_all_input_schemas_are_valid_objects() {
        let list = tools_list();
        let tools = list["tools"].as_array().expect("tools array");

        for tool in tools {
            let name = tool["name"].as_str().unwrap_or("<unknown>");
            let schema = &tool["inputSchema"];
            assert_eq!(
                schema["type"], "object",
                "{name}: inputSchema must be type:object"
            );
            assert_eq!(
                schema["additionalProperties"], false,
                "{name}: inputSchema must have additionalProperties:false"
            );
        }
    }

    #[test]
    fn tools_list_all_names_use_only_allowed_chars() {
        let list = tools_list();
        let tools = list["tools"].as_array().expect("tools array");
        for tool in tools {
            let name = tool["name"].as_str().expect("name string");
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "tool name {name:?} contains disallowed characters"
            );
        }
    }

    // ── build_request ─────────────────────────────────────────────────────────

    #[test]
    fn build_request_status_from_empty_args() {
        let req = build_request("status", &json!({})).expect("build_request status");
        assert!(matches!(req, Request::Status));
    }

    #[test]
    fn build_request_diff_with_partial_args() {
        let args = json!({"to": "ab12", "stat": true});
        let req = build_request("diff", &args).expect("build_request diff");
        assert!(matches!(
            req,
            Request::Diff { to: Some(ref t), stat: true, .. } if t == "ab12"
        ));
    }

    #[test]
    fn build_request_returns_err_on_bad_args() {
        // `paths` is required for scoped_cut but not provided.
        let result = build_request("scoped_cut", &json!({"message": "x"}));
        assert!(result.is_err(), "missing required field must produce Err");
    }

    // ── full session via Cursor ───────────────────────────────────────────────

    #[test]
    fn session_initialize_echoes_client_protocol_version() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        let resp = &lines[0];
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(resp["result"]["serverInfo"]["name"], "tack");
    }

    #[test]
    fn session_initialize_uses_default_version_when_absent() {
        let input =
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["result"]["protocolVersion"], "2025-11-25");
    }

    #[test]
    fn session_notifications_initialized_produces_no_output() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n",
        );
        let lines = run_session(input);
        // Must have exactly 2 lines: the initialize reply and the ping reply.
        // The notification must NOT produce a line.
        assert_eq!(
            lines.len(),
            2,
            "notification must not produce output: {lines:?}"
        );
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[1]["id"], 2);
        assert_eq!(lines[1]["result"], json!({}));
    }

    #[test]
    fn session_tools_list_returns_all_tack_tools() {
        let input = r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        let tools = lines[0]["result"]["tools"].as_array().expect("tools array");
        let schema_methods = api::schema_methods();
        assert_eq!(tools.len(), schema_methods.len());
        for method in &schema_methods {
            let expected = format!("tack_{}", method.method);
            assert!(
                tools.iter().any(|t| t["name"] == expected),
                "missing tool {expected}"
            );
        }
    }

    #[test]
    fn session_ping_returns_empty_object() {
        let input = r#"{"jsonrpc":"2.0","id":99,"method":"ping"}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["result"], json!({}));
    }

    #[test]
    fn session_unknown_method_returns_method_not_found() {
        let input = r#"{"jsonrpc":"2.0","id":5,"method":"unknown/thing"}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["error"]["code"], -32_601);
    }

    #[test]
    fn session_malformed_json_returns_parse_error() {
        let input = "not json at all\n";
        let lines = run_session(input);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["id"], Value::Null);
        assert_eq!(lines[0]["error"]["code"], -32_700);
    }

    #[test]
    fn session_blank_lines_are_ignored() {
        let input = "\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n";
        let lines = run_session(input);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["result"], json!({}));
    }

    #[test]
    fn session_unknown_tool_returns_protocol_error() {
        let input = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"git_status","arguments":{}}}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0]["error"]["code"], -32_602,
            "unknown tool must return -32602"
        );
    }

    // ── tools/call tack_status with a real repo ───────────────────────────────

    #[test]
    fn session_tools_call_status_succeeds_with_real_repo() {
        let dir = make_tack_repo();
        // Change to the repo directory so `serve_mcp` opens it.
        let _cwd = chdir(dir.path());

        let input = r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"tack_status","arguments":{}}}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        let resp = &lines[0];
        // Must be a result (not an error at the JSON-RPC level).
        assert!(resp.get("result").is_some(), "expected result: {resp}");
        assert!(resp.get("error").is_none(), "unexpected error: {resp}");
        // Content must be present and type must be "text".
        let content = resp["result"]["content"].as_array().expect("content array");
        assert!(!content.is_empty());
        assert_eq!(content[0]["type"], "text");
        // isError must be false for a clean status.
        assert_eq!(resp["result"]["isError"], false);
    }

    #[test]
    fn session_tools_call_no_repo_returns_tool_error() {
        // Point cwd at a directory with no .tack/ so the repo open fails.
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = chdir(dir.path());

        let input = r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"tack_status","arguments":{}}}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        // JSON-RPC level: result (not error), but isError:true inside result.
        let resp = &lines[0];
        assert!(
            resp.get("result").is_some(),
            "must be a result (tool execution error)"
        );
        assert_eq!(resp["result"]["isError"], true);
    }

    // ── author fill ───────────────────────────────────────────────────────────

    #[test]
    fn author_fill_named_cut_with_env_var() {
        let dir = make_tack_repo();
        // Write a file so a named_cut has something to snapshot.
        std::fs::write(dir.path().join("file.txt"), b"hello").expect("write");

        let _cwd = chdir(dir.path());

        // SAFETY: tests in this module must not run in parallel (env is process-global).
        // The guard restores the variables on drop.
        unsafe {
            std::env::set_var("TACK_AUTHOR_NAME", "TestAuthor");
            std::env::set_var("TACK_AUTHOR_EMAIL", "test@example.com");
        }
        let _env_guard = AuthorEnvGuard;

        // Call tack_named_cut WITHOUT providing author_name/author_email.
        let input = r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"tack_named_cut","arguments":{"message":"test cut"}}}"#.to_owned() + "\n";
        let lines = run_session(&input);
        assert_eq!(lines.len(), 1);
        let resp = &lines[0];
        assert!(resp.get("result").is_some(), "expected result: {resp}");
        // isError must be false — the cut was created successfully.
        assert_eq!(
            resp["result"]["isError"], false,
            "named_cut with env author must succeed: {resp}"
        );
        // The structured content must have status:named_cut.
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["status"], "named_cut", "expected named_cut status: {sc}");
    }
}
