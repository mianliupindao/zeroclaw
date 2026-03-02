use crate::agent;
use crate::config::Config;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use uuid::Uuid;

const JSONRPC_VERSION: &str = "2.0";
const JSONRPC_PARSE_ERROR: i64 = -32700;
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_PARAMS: i64 = -32602;
const JSONRPC_INTERNAL_ERROR: i64 = -32603;
const JSONRPC_SESSION_NOT_FOUND: i64 = -32004;

#[derive(Debug, Clone)]
struct JsonRpcRequest {
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRpcErrorObject {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl JsonRpcErrorObject {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }
}

#[derive(Debug, Clone)]
struct AcpSession {
    cwd: PathBuf,
}

#[derive(Debug, Clone)]
struct PromptRequest {
    session_id: String,
    cwd: PathBuf,
    prompt: String,
}

#[derive(Default)]
struct AcpServerState {
    sessions: HashMap<String, AcpSession>,
}

impl AcpServerState {
    fn initialize_result(&self) -> Value {
        json!({
            "protocol_version": 1,
            "server_info": {
                "name": "zeroclaw",
                "title": "ZeroClaw ACP Server",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "session": true,
                "streaming_notifications": true,
                "methods": [
                    "initialize",
                    "session/new",
                    "session/prompt",
                    "session/stop",
                ],
            }
        })
    }

    fn create_session(
        &mut self,
        params: Option<&Value>,
    ) -> std::result::Result<Value, JsonRpcErrorObject> {
        let cwd = resolve_cwd(params)?;
        let session_id = format!("zc-{}", Uuid::new_v4());
        self.sessions
            .insert(session_id.clone(), AcpSession { cwd: cwd.clone() });
        Ok(json!({
            "session_id": session_id,
            "cwd": cwd.to_string_lossy(),
        }))
    }

    fn stop_session(
        &mut self,
        params: Option<&Value>,
    ) -> std::result::Result<Value, JsonRpcErrorObject> {
        let obj = params_as_object(params)?;
        let session_id = required_non_empty_string(obj, "session_id")?;
        let stopped = self.sessions.remove(&session_id).is_some();
        Ok(json!({
            "session_id": session_id,
            "stopped": stopped,
        }))
    }

    fn resolve_prompt_request(
        &self,
        params: Option<&Value>,
    ) -> std::result::Result<PromptRequest, JsonRpcErrorObject> {
        let obj = params_as_object(params)?;
        let session_id = required_non_empty_string(obj, "session_id")?;
        let prompt = extract_prompt_text(obj)?;
        let Some(session) = self.sessions.get(&session_id) else {
            return Err(JsonRpcErrorObject::new(
                JSONRPC_SESSION_NOT_FOUND,
                format!("Session not found: {session_id}"),
            ));
        };
        Ok(PromptRequest {
            session_id,
            cwd: session.cwd.clone(),
            prompt,
        })
    }
}

pub async fn run_stdio(config: Config) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut state = AcpServerState::default();

    while let Some(line) = reader
        .next_line()
        .await
        .context("Failed reading ACP stdin")?
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let inbound = match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => value,
            Err(error) => {
                let err = JsonRpcErrorObject::with_data(
                    JSONRPC_PARSE_ERROR,
                    "Parse error",
                    json!({ "detail": error.to_string() }),
                );
                write_json_line(&mut stdout, &json_rpc_error(Value::Null, err)).await?;
                continue;
            }
        };

        let request = match parse_json_rpc_request(inbound) {
            Ok(request) => request,
            Err(error) => {
                write_json_line(&mut stdout, &json_rpc_error(Value::Null, error)).await?;
                continue;
            }
        };

        let response_id = request.id.clone().unwrap_or(Value::Null);
        let outcome = match request.method.as_str() {
            "initialize" => Ok(state.initialize_result()),
            "session/new" => state.create_session(request.params.as_ref()),
            "session/stop" => state.stop_session(request.params.as_ref()),
            "session/prompt" => {
                let prompt_request = match state.resolve_prompt_request(request.params.as_ref()) {
                    Ok(prompt_request) => prompt_request,
                    Err(error) => {
                        if request.id.is_some() {
                            write_json_line(
                                &mut stdout,
                                &json_rpc_error(response_id.clone(), error),
                            )
                            .await?;
                        }
                        continue;
                    }
                };

                send_notification(
                    &mut stdout,
                    "session/progress",
                    json!({
                        "session_id": prompt_request.session_id,
                        "event": "started",
                    }),
                )
                .await?;

                let mut session_config = config.clone();
                session_config.workspace_dir = prompt_request.cwd;

                match agent::process_message_with_session(
                    session_config,
                    &prompt_request.prompt,
                    Some(prompt_request.session_id.as_str()),
                )
                .await
                {
                    Ok(response_text) => {
                        send_notification(
                            &mut stdout,
                            "session/content",
                            json!({
                                "session_id": prompt_request.session_id,
                                "delta": response_text,
                            }),
                        )
                        .await?;
                        send_notification(
                            &mut stdout,
                            "session/progress",
                            json!({
                                "session_id": prompt_request.session_id,
                                "event": "completed",
                            }),
                        )
                        .await?;
                        Ok(json!({
                            "session_id": prompt_request.session_id,
                            "response": response_text,
                            "content": [
                                {
                                    "type": "text",
                                    "text": response_text,
                                }
                            ],
                        }))
                    }
                    Err(error) => {
                        let msg = error.to_string();
                        send_notification(
                            &mut stdout,
                            "session/progress",
                            json!({
                                "session_id": prompt_request.session_id,
                                "event": "failed",
                                "error": msg,
                            }),
                        )
                        .await?;
                        Err(JsonRpcErrorObject::with_data(
                            JSONRPC_INTERNAL_ERROR,
                            "Agent processing failed",
                            json!({ "detail": msg }),
                        ))
                    }
                }
            }
            _ => Err(JsonRpcErrorObject::new(
                JSONRPC_METHOD_NOT_FOUND,
                format!("Method not found: {}", request.method),
            )),
        };

        if request.id.is_none() {
            continue;
        }

        let payload = match outcome {
            Ok(result) => json_rpc_result(response_id.clone(), result),
            Err(error) => json_rpc_error(response_id.clone(), error),
        };
        write_json_line(&mut stdout, &payload).await?;
    }

    Ok(())
}

fn parse_json_rpc_request(value: Value) -> std::result::Result<JsonRpcRequest, JsonRpcErrorObject> {
    let obj = value.as_object().ok_or_else(|| {
        JsonRpcErrorObject::new(JSONRPC_INVALID_REQUEST, "Request must be a JSON object")
    })?;

    let Some(jsonrpc) = obj.get("jsonrpc").and_then(Value::as_str) else {
        return Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_REQUEST,
            "Missing jsonrpc version",
        ));
    };
    if jsonrpc != JSONRPC_VERSION {
        return Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_REQUEST,
            format!("Unsupported jsonrpc version: {jsonrpc}"),
        ));
    }

    if let Some(id) = obj.get("id") {
        if !is_valid_jsonrpc_id(id) {
            return Err(JsonRpcErrorObject::new(
                JSONRPC_INVALID_REQUEST,
                "id must be a string, number, or null",
            ));
        }
    }

    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            JsonRpcErrorObject::new(JSONRPC_INVALID_REQUEST, "method must be a non-empty string")
        })?
        .to_string();

    Ok(JsonRpcRequest {
        id: obj.get("id").cloned(),
        method,
        params: obj.get("params").cloned(),
    })
}

fn params_as_object(
    params: Option<&Value>,
) -> std::result::Result<&Map<String, Value>, JsonRpcErrorObject> {
    match params {
        Some(Value::Object(obj)) => Ok(obj),
        Some(_) => Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_PARAMS,
            "params must be an object",
        )),
        None => Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_PARAMS,
            "Missing params object",
        )),
    }
}

fn resolve_cwd(params: Option<&Value>) -> std::result::Result<PathBuf, JsonRpcErrorObject> {
    let cwd_value = match params {
        Some(Value::Object(obj)) => obj.get("cwd"),
        Some(_) => {
            return Err(JsonRpcErrorObject::new(
                JSONRPC_INVALID_PARAMS,
                "params must be an object",
            ));
        }
        None => None,
    };

    let cwd = if let Some(raw_cwd) = cwd_value {
        let Some(cwd_str) = raw_cwd.as_str() else {
            return Err(JsonRpcErrorObject::new(
                JSONRPC_INVALID_PARAMS,
                "params.cwd must be a string",
            ));
        };
        if cwd_str.trim().is_empty() {
            return Err(JsonRpcErrorObject::new(
                JSONRPC_INVALID_PARAMS,
                "params.cwd must not be empty",
            ));
        }
        PathBuf::from(cwd_str)
    } else {
        std::env::current_dir().map_err(|error| {
            JsonRpcErrorObject::with_data(
                JSONRPC_INTERNAL_ERROR,
                "Failed to resolve current working directory",
                json!({ "detail": error.to_string() }),
            )
        })?
    };

    let absolute_cwd = if cwd.is_absolute() {
        cwd
    } else {
        let current = std::env::current_dir().map_err(|error| {
            JsonRpcErrorObject::with_data(
                JSONRPC_INTERNAL_ERROR,
                "Failed to resolve current working directory",
                json!({ "detail": error.to_string() }),
            )
        })?;
        current.join(cwd)
    };

    if !absolute_cwd.is_dir() {
        return Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_PARAMS,
            format!(
                "params.cwd does not exist or is not a directory: {}",
                absolute_cwd.display()
            ),
        ));
    }

    Ok(absolute_cwd)
}

fn required_non_empty_string(
    params: &Map<String, Value>,
    key: &str,
) -> std::result::Result<String, JsonRpcErrorObject> {
    let Some(raw) = params.get(key).and_then(Value::as_str) else {
        return Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_PARAMS,
            format!("params.{key} must be a non-empty string"),
        ));
    };
    if raw.trim().is_empty() {
        return Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_PARAMS,
            format!("params.{key} must be a non-empty string"),
        ));
    }
    Ok(raw.to_string())
}

fn extract_prompt_text(
    params: &Map<String, Value>,
) -> std::result::Result<String, JsonRpcErrorObject> {
    let prompt_value = params.get("prompt").or_else(|| params.get("input"));
    let Some(prompt_value) = prompt_value else {
        return Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_PARAMS,
            "params.prompt must be present",
        ));
    };

    let prompt = match prompt_value {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut chunks = Vec::new();
            for item in items {
                match item {
                    Value::String(text) => {
                        if !text.trim().is_empty() {
                            chunks.push(text.as_str());
                        }
                    }
                    Value::Object(obj) => {
                        if let Some(text) = obj.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                chunks.push(text);
                            }
                        }
                    }
                    _ => {}
                }
            }
            if chunks.is_empty() {
                return Err(JsonRpcErrorObject::new(
                    JSONRPC_INVALID_PARAMS,
                    "params.prompt must contain text items",
                ));
            }
            chunks.join("\n")
        }
        _ => {
            return Err(JsonRpcErrorObject::new(
                JSONRPC_INVALID_PARAMS,
                "params.prompt must be a string or array of text items",
            ));
        }
    };

    if prompt.trim().is_empty() {
        return Err(JsonRpcErrorObject::new(
            JSONRPC_INVALID_PARAMS,
            "params.prompt must not be empty",
        ));
    }

    Ok(prompt)
}

fn is_valid_jsonrpc_id(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Null)
}

fn json_rpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    })
}

fn json_rpc_error(id: Value, error: JsonRpcErrorObject) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": error,
    })
}

async fn send_notification<W>(writer: &mut W, method: &str, params: Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": method,
        "params": params,
    });
    write_json_line(writer, &payload).await
}

async fn write_json_line<W>(writer: &mut W, payload: &Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let encoded = serde_json::to_string(payload).context("Failed to encode ACP JSON payload")?;
    writer
        .write_all(encoded.as_bytes())
        .await
        .context("Failed writing ACP response payload")?;
    writer
        .write_all(b"\n")
        .await
        .context("Failed writing ACP response delimiter")?;
    writer
        .flush()
        .await
        .context("Failed flushing ACP response")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_rpc_request_accepts_notification_without_id() {
        let request = parse_json_rpc_request(json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {},
        }))
        .expect("valid JSON-RPC notification should parse");
        assert!(request.id.is_none());
        assert_eq!(request.method, "initialize");
    }

    #[test]
    fn parse_json_rpc_request_rejects_invalid_jsonrpc_version() {
        let error = parse_json_rpc_request(json!({
            "jsonrpc": "1.0",
            "id": 1,
            "method": "initialize",
        }))
        .expect_err("invalid JSON-RPC version should fail");
        assert_eq!(error.code, JSONRPC_INVALID_REQUEST);
    }

    #[test]
    fn parse_json_rpc_request_rejects_non_scalar_id() {
        let error = parse_json_rpc_request(json!({
            "jsonrpc": "2.0",
            "id": {"nested": true},
            "method": "initialize",
        }))
        .expect_err("non scalar id should fail");
        assert_eq!(error.code, JSONRPC_INVALID_REQUEST);
    }

    #[test]
    fn extract_prompt_text_supports_string_and_array_payloads() {
        let string_prompt = extract_prompt_text(
            json!({
                "prompt": "hello",
            })
            .as_object()
            .expect("object"),
        )
        .expect("string prompt should parse");
        assert_eq!(string_prompt, "hello");

        let array_prompt = extract_prompt_text(
            json!({
                "prompt": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "world"},
                ],
            })
            .as_object()
            .expect("object"),
        )
        .expect("array prompt should parse");
        assert_eq!(array_prompt, "hello\nworld");
    }

    #[test]
    fn create_and_stop_session_roundtrip() {
        let mut state = AcpServerState::default();
        let cwd = std::env::current_dir().expect("cwd");
        let created = state
            .create_session(Some(&json!({ "cwd": cwd.to_string_lossy() })))
            .expect("session/new should succeed");
        let session_id = created["session_id"]
            .as_str()
            .expect("session id must be string")
            .to_string();
        assert!(state.sessions.contains_key(&session_id));

        let stopped_once = state
            .stop_session(Some(&json!({ "session_id": session_id })))
            .expect("first stop should succeed");
        assert_eq!(stopped_once["stopped"], json!(true));

        let stopped_twice = state
            .stop_session(Some(&json!({ "session_id": session_id })))
            .expect("second stop should still succeed");
        assert_eq!(stopped_twice["stopped"], json!(false));
    }

    #[test]
    fn resolve_prompt_request_requires_existing_session() {
        let state = AcpServerState::default();
        let error = state
            .resolve_prompt_request(Some(&json!({
                "session_id": "missing",
                "prompt": "hello",
            })))
            .expect_err("missing session should fail");
        assert_eq!(error.code, JSONRPC_SESSION_NOT_FOUND);
    }
}
