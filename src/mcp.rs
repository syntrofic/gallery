use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::app::AppState;
use crate::auth::Actor;
use crate::error::AppError;
use crate::ids::random_token;

pub fn allocate_socket() -> PathBuf {
    PathBuf::from(format!("/tmp/gallery-mcp-{}.sock", random_token(8)))
}

pub async fn serve(state: AppState) -> Result<()> {
    let path = state.mcp_socket.clone();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tokio::spawn(async move {
        let _cleanup = RemoveOnDrop(path);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let _ = connection(state, stream).await;
                    });
                }
                Err(_) => break,
            }
        }
    });
    Ok(())
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Bridge {
    session_id: String,
    token: String,
    epoch: i64,
}

async fn connection(state: AppState, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut bridge: Option<Bridge> = None;
    loop {
        let Some(line) = lines.next_line().await? else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                write_line(&mut write, &rpc_error(Value::Null, -32700, "PARSE")).await?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or("");
        if id.is_none() {
            continue;
        }
        let id = id.unwrap_or(Value::Null);
        let response = match method {
            "initialize" => match authorize(&state, &msg["params"]).await {
                Ok(next) => {
                    bridge = Some(next);
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": {"tools": {"listChanged": false}},
                            "serverInfo": {"name": "gallery", "version": env!("CARGO_PKG_VERSION")}
                        }
                    })
                }
                Err(code) => rpc_error(id, -32000, code),
            },
            "tools/list" => {
                if bridge.is_none() {
                    rpc_error(id, -32000, "UNAUTHENTICATED")
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"tools": tool_defs()}
                    })
                }
            }
            "tools/call" => match &bridge {
                None => rpc_error(id, -32000, "UNAUTHENTICATED"),
                Some(current) => {
                    tool_call(&state, current, id, &msg["params"]).await
                }
            },
            _ => rpc_error(id, -32601, "METHOD_NOT_FOUND"),
        };
        write_line(&mut write, &response).await?;
    }
    Ok(())
}

async fn authorize(state: &AppState, params: &Value) -> Result<Bridge, &'static str> {
    let session_id = params["gallery"]["sessionId"]
        .as_str()
        .ok_or("UNAUTHENTICATED")?;
    let token = params["gallery"]["token"].as_str().ok_or("UNAUTHENTICATED")?;
    let row = sqlx::query_as::<_, (Option<String>, String, i64)>(
        "SELECT s.bridge_token, a.status, s.run_epoch
         FROM agent_sessions s JOIN actors a ON a.id = s.actor_id
         WHERE s.id = ?",
    )
    .bind(session_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| "UNAUTHENTICATED")?;
    let Some((stored, status, epoch)) = row else {
        return Err("UNAUTHENTICATED");
    };
    let Some(stored) = stored else {
        return Err("UNAUTHENTICATED");
    };
    let presented = params["gallery"]["epoch"].as_i64().unwrap_or(epoch);
    if !token_ok(&stored, token) || status != "active" || presented != epoch {
        return Err("UNAUTHENTICATED");
    }
    Ok(Bridge {
        session_id: session_id.to_string(),
        token: token.to_string(),
        epoch,
    })
}

async fn tool_call(state: &AppState, bridge: &Bridge, id: Value, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !token_ok_still(state, bridge).await {
        return rpc_error(id, -32000, "UNAUTHENTICATED");
    }
    if write_tool(name) && !epoch_ok(state, bridge).await {
        return tool_failure(id, "EPOCH");
    }
    let actor = match load_actor(state, &bridge.session_id).await {
        Ok(actor) => actor,
        Err(code) => return tool_failure(id, code),
    };
    if actor.must_change_password {
        return tool_failure(id, "MUST_CHANGE_PASSWORD");
    }
    match crate::ai::dispatch_tool(state, &actor, &bridge.session_id, name, args).await {
        Ok(result) => {
            let text = redact(&result.to_string(), &bridge.token);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"content": [{"type": "text", "text": text}], "isError": false}
            })
        }
        Err(err) => tool_failure(id, error_code(&err)),
    }
}

fn write_tool(name: &str) -> bool {
    matches!(
        name,
        "album.update"
            | "album.reorder"
            | "album.move_images"
            | "job.repair_variants"
            | "scans.request"
            | "archive.request"
    )
}

async fn epoch_ok(state: &AppState, bridge: &Bridge) -> bool {
    let row = sqlx::query_as::<_, (i64,)>("SELECT run_epoch FROM agent_sessions WHERE id = ?")
        .bind(&bridge.session_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();
    row.is_some_and(|row| row.0 == bridge.epoch)
}

async fn token_ok_still(state: &AppState, bridge: &Bridge) -> bool {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT bridge_token FROM agent_sessions WHERE id = ?",
    )
    .bind(&bridge.session_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    row.and_then(|(token,)| token)
        .is_some_and(|stored| token_ok(&stored, &bridge.token))
}

async fn load_actor(state: &AppState, session_id: &str) -> Result<Actor, &'static str> {
    let row = sqlx::query_as::<_, (String, String, String, i64, i64)>(
        "SELECT a.id, a.username, a.role, a.must_change_password, a.revision
         FROM agent_sessions s JOIN actors a ON a.id = s.actor_id
         WHERE s.id = ? AND a.status = 'active'",
    )
    .bind(session_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| "UNAUTHENTICATED")?
    .ok_or("UNAUTHENTICATED")?;
    Ok(Actor {
        id: row.0,
        username: row.1,
        role: row.2,
        must_change_password: row.3 == 1,
        revision: row.4,
    })
}

fn tool_defs() -> Vec<Value> {
    crate::ai::allowed_tools()
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "description": format!("Gallery {name}"),
                "inputSchema": {"type": "object", "additionalProperties": true}
            })
        })
        .collect()
}

fn tool_failure(id: Value, code: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"content": [{"type": "text", "text": code}], "isError": true}
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn error_code(err: &AppError) -> &'static str {
    match err {
        AppError::Problem { code, .. } => code,
        AppError::Internal(_) => "INTERNAL",
    }
}

fn token_ok(stored: &str, given: &str) -> bool {
    bool::from(stored.as_bytes().ct_eq(given.as_bytes()))
}

fn redact(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "[redacted]")
    }
}

async fn write_line<W: AsyncWriteExt + Unpin>(write: &mut W, value: &Value) -> Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}

pub fn with_bridge_auth(line: &str, session_id: &str, token: &str, epoch: i64) -> String {
    let mut value: Value = serde_json::from_str(line).unwrap_or_else(|_| json!({}));
    if value["method"] == "initialize" {
        value["params"]["gallery"]["sessionId"] = json!(session_id);
        value["params"]["gallery"]["token"] = json!(token);
        value["params"]["gallery"]["epoch"] = json!(epoch);
    }
    value.to_string()
}

pub async fn proxy_stdio() -> Result<()> {
    let socket = std::env::var("GALLERY_MCP_SOCKET").map_err(|_| anyhow!("GALLERY_MCP_SOCKET is required"))?;
    let session = std::env::var("GALLERY_MCP_SESSION").map_err(|_| anyhow!("GALLERY_MCP_SESSION is required"))?;
    let token = std::env::var("GALLERY_MCP_TOKEN").map_err(|_| anyhow!("GALLERY_MCP_TOKEN is required"))?;
    let epoch = std::env::var("GALLERY_MCP_EPOCH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let stream = tokio::time::timeout(
        Duration::from_secs(5),
        UnixStream::connect(Path::new(&socket)),
    )
    .await
    .map_err(|_| anyhow!("mcp socket timed out"))??;
    let (read, mut write) = stream.into_split();
    let mut incoming = BufReader::new(tokio::io::stdin()).lines();
    let mut upstream = BufReader::new(read).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = incoming.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let authed = with_bridge_auth(&line, &session, &token, epoch);
        let parsed: Value = serde_json::from_str(&authed).unwrap_or(Value::Null);
        let expects_reply = parsed.get("id").is_some();
        write.write_all(authed.as_bytes()).await?;
        write.write_all(b"\n").await?;
        write.flush().await?;
        if expects_reply {
            let Some(response) = upstream.next_line().await? else {
                break;
            };
            stdout.write_all(response.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_gets_bridge_credentials() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"codex"}}}"#;
        let authed = with_bridge_auth(line, "session-1", "token-1", 3);
        let value: Value = serde_json::from_str(&authed).unwrap();
        assert_eq!(value["params"]["gallery"]["sessionId"], "session-1");
        assert_eq!(value["params"]["gallery"]["token"], "token-1");
        assert_eq!(value["params"]["gallery"]["epoch"], 3);
        let other = with_bridge_auth(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "session-1",
            "token-1",
            3,
        );
        assert!(!other.contains("token-1"));
    }
}
