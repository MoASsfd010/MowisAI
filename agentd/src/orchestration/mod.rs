//! Multi-sandbox orchestration: Vertex Gemini planner, executor, and synthesis.

mod agent_runner;
mod executor;
mod planner;
pub mod orchestrator;
mod sandbox_profiles;

pub use agent_runner::AgentOutput;
pub use planner::{Plan, PlanTask};

pub(crate) const HTTP_TIMEOUT_SECS: u64 = 180;
pub(crate) const MAX_TOOL_ROUNDS: usize = 64;

// ── Vertex / gcloud (shared by planner, agent_runner, orchestrator) ────────

#[cfg(unix)]
pub(crate) fn gcloud_access_token() -> anyhow::Result<String> {
    use anyhow::{anyhow, Context};
    use std::process::Command;
    let out = Command::new("gcloud")
        .args(["auth", "print-access-token"])
        .output()
        .context("spawn gcloud — is it installed and on PATH?")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gcloud auth print-access-token failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let s = String::from_utf8(out.stdout).context("token utf-8")?;
    let t = s.trim().to_string();
    if t.is_empty() {
        return Err(anyhow!("empty access token from gcloud"));
    }
    Ok(t)
}

#[cfg(not(unix))]
pub(crate) fn gcloud_access_token() -> anyhow::Result<String> {
    Err(anyhow::anyhow!(
        "orchestration requires Unix (agentd uses Unix domain sockets)"
    ))
}

pub(crate) fn vertex_generate_url(project_id: &str) -> String {
    format!(
        "https://us-central1-aiplatform.googleapis.com/v1/projects/{}/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
        project_id
    )
}

/// Same five tools as `vertex_agent.rs` / agentd socket.
pub(crate) fn gemini_tool_declarations() -> serde_json::Value {
    use serde_json::json;
    json!([
        {
            "name": "run_command",
            "description": "Run a shell command inside the sandbox (chroot).",
            "parameters": {
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Shell command" }
                },
                "required": ["cmd"]
            }
        },
        {
            "name": "write_file",
            "description": "Write text content to a file path in the sandbox.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "read_file",
            "description": "Read a file from the sandbox.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "create_directory",
            "description": "Create a directory (and parents) in the sandbox.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "list_files",
            "description": "List files and subdirectories in a directory.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }
        }
    ])
}

// ── Socket protocol (matches vertex_agent / socket_server) ───────────────────

#[cfg(not(unix))]
pub(crate) fn socket_roundtrip(
    _socket_path: &str,
    _req: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    Err(anyhow::anyhow!(
        "orchestration requires Unix (agentd uses Unix domain sockets)"
    ))
}

#[cfg(unix)]
pub(crate) fn socket_roundtrip(
    socket_path: &str,
    req: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    use anyhow::{anyhow, Context};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket_path).context("UnixStream::connect")?;
    let mut line = serde_json::to_string(req).context("serialize request")?;
    line.push('\n');
    stream.write_all(line.as_bytes()).context("write socket")?;
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .context("read socket")?;
    let trimmed = response_line.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty socket response"));
    }
    serde_json::from_str(trimmed).context("parse socket JSON")
}

#[cfg(not(unix))]
pub(crate) fn parse_ok_field(
    _resp: &serde_json::Value,
    _key: &str,
) -> anyhow::Result<String> {
    Err(anyhow::anyhow!(
        "orchestration requires Unix (agentd uses Unix domain sockets)"
    ))
}

#[cfg(unix)]
pub(crate) fn parse_ok_field(resp: &serde_json::Value, key: &str) -> anyhow::Result<String> {
    use anyhow::anyhow;
    if resp.get("status").and_then(|s| s.as_str()) != Some("ok") {
        let err = resp
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown socket error");
        return Err(anyhow!("socket: {}", err));
    }
    let result = resp
        .get("result")
        .ok_or_else(|| anyhow!("socket response missing result"))?;
    if let Some(s) = result.get(key).and_then(|v| v.as_str()) {
        return Ok(s.to_string());
    }
    if let Some(n) = result.get(key).and_then(|v| v.as_u64()) {
        return Ok(n.to_string());
    }
    Err(anyhow!("result missing string/number field '{}'", key))
}

#[cfg(not(unix))]
pub(crate) fn invoke_tool_via_socket(
    _socket_path: &str,
    _sandbox_id: &str,
    _container_id: &str,
    _tool_name: &str,
    _input: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    Err(anyhow::anyhow!(
        "orchestration requires Unix (agentd uses Unix domain sockets)"
    ))
}

#[cfg(unix)]
pub(crate) fn invoke_tool_via_socket(
    socket_path: &str,
    sandbox_id: &str,
    container_id: &str,
    tool_name: &str,
    input: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context;
    use serde_json::json;
    let allowed = [
        "run_command",
        "write_file",
        "read_file",
        "create_directory",
        "list_files",
    ];
    if !allowed.contains(&tool_name) {
        return Ok(json!({
            "error": format!("unknown tool '{}'", tool_name),
            "success": false
        }));
    }
    let req = json!({
        "request_type": "invoke_tool",
        "sandbox": sandbox_id,
        "container": container_id,
        "name": tool_name,
        "input": input
    });
    let resp = socket_roundtrip(socket_path, &req)?;
    if resp.get("status").and_then(|s| s.as_str()) == Some("ok") {
        Ok(resp.get("result").cloned().unwrap_or(json!({})))
    } else {
        let err = resp
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("tool error");
        Ok(json!({ "error": err, "success": false }))
    }
}
