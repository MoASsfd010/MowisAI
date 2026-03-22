//! Agent loop: Vertex AI Gemini 2.5 ↔ agentd Unix socket (sandbox tools).
//!
//! On non-Unix targets the entrypoint returns an error (Unix sockets required).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::Command;
use std::time::Duration;

const MAX_TOOL_ROUNDS: usize = 64;
const HTTP_TIMEOUT_SECS: u64 = 180;

/// Run the Gemini ↔ agentd tool loop until the model returns a final text answer.
pub fn run(prompt: &str, project_id: &str, socket_path: &str) -> Result<()> {
    #[cfg(unix)]
    {
        run_inner(prompt, project_id, socket_path)
    }
    #[cfg(not(unix))]
    {
        let _ = (prompt, project_id, socket_path);
        Err(anyhow!(
            "vertex_agent requires Unix (agentd uses Unix domain sockets)"
        ))
    }
}

#[cfg(unix)]
fn run_inner(prompt: &str, project_id: &str, socket_path: &str) -> Result<()> {
    println!("[vertex] creating sandbox via {} …", socket_path);
    let create_sb = json!({
        "request_type": "create_sandbox",
        "image": "alpine"
    });
    let sb_resp = socket_roundtrip(socket_path, &create_sb)?;
    let sandbox_id = parse_ok_field(&sb_resp, "sandbox").context("create_sandbox")?;
    println!("[vertex] sandbox id {}", sandbox_id);

    println!("[vertex] creating container…");
    let create_ct = json!({
        "request_type": "create_container",
        "sandbox": &sandbox_id
    });
    let ct_resp = socket_roundtrip(socket_path, &create_ct)?;
    let container_id = parse_ok_field(&ct_resp, "container").context("create_container")?;
    println!("[vertex] container id {}", container_id);

    let url = format!(
        "https://us-central1-aiplatform.googleapis.com/v1/projects/{}/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
        project_id
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let system_instruction = json!({
        "parts": [{ "text": "You control a Linux sandbox (Alpine) via tools. Use paths under /workspace when writing files unless the user specifies otherwise. Prefer listing directories before assuming files exist." }]
    });

    let mut contents: Vec<Value> = vec![json!({
        "role": "user",
        "parts": [{ "text": prompt }]
    })];

    let tools = json!([{
        "functionDeclarations": gemini_tool_declarations()
    }]);

    for round in 0..MAX_TOOL_ROUNDS {
        let body = json!({
            "contents": contents,
            "tools": tools,
            "systemInstruction": system_instruction,
            "generationConfig": {
                "temperature": 0.5
            }
        });

        println!("[vertex] → Gemini (round {}) …", round + 1);
        let token = gcloud_access_token()?;
        let http_resp = client
            .post(&url)
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .context("generateContent HTTP")?;

        if !http_resp.status().is_success() {
            let status = http_resp.status();
            let text = http_resp.text().unwrap_or_default();
            return Err(anyhow!("Vertex AI error {}: {}", status, text));
        }

        let response: Value = http_resp.json().context("parse Vertex JSON")?;

        if let Some(block) = response
            .pointer("/promptFeedback/blockReason")
            .and_then(|v| v.as_str())
        {
            return Err(anyhow!("prompt blocked: {}", block));
        }

        let candidate = response
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("no candidates in response: {}", response))?;

        let content = candidate
            .get("content")
            .ok_or_else(|| anyhow!("candidate missing content"))?;
        let parts = content
            .get("parts")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        let mut model_parts: Vec<Value> = Vec::new();
        let mut function_calls: Vec<(String, Value)> = Vec::new();

        for part in &parts {
            if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = fc.get("args").cloned().unwrap_or(json!({}));
                println!("[tool-call] {} {}", name, args);
                function_calls.push((name, args));
                model_parts.push(part.clone());
            } else if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    println!("[model]\n{}", t);
                }
                model_parts.push(part.clone());
            } else {
                model_parts.push(part.clone());
            }
        }

        if function_calls.is_empty() {
            println!("[vertex] done (no further tool calls).");
            return Ok(());
        }

        contents.push(json!({
            "role": "model",
            "parts": model_parts
        }));

        let mut response_parts = Vec::new();
        for (name, args) in function_calls {
            let tool_result = invoke_tool_via_socket(
                socket_path,
                &sandbox_id,
                &container_id,
                &name,
                &args,
            )?;
            println!("[tool-result] {} → {}", name, tool_result);
            response_parts.push(json!({
                "functionResponse": {
                    "name": name,
                    "response": tool_result
                }
            }));
        }

        contents.push(json!({
            "role": "user",
            "parts": response_parts
        }));
    }

    Err(anyhow!(
        "exceeded max tool rounds ({}) without final answer",
        MAX_TOOL_ROUNDS
    ))
}

#[cfg(unix)]
fn gcloud_access_token() -> Result<String> {
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

#[cfg(unix)]
fn gemini_tool_declarations() -> Value {
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

#[cfg(unix)]
fn parse_ok_field(resp: &Value, key: &str) -> Result<String> {
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

#[cfg(unix)]
fn socket_roundtrip(socket_path: &str, req: &Value) -> Result<Value> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).context("UnixStream::connect")?;
    let mut line = serde_json::to_string(req).context("serialize request")?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .context("write socket")?;
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

#[cfg(unix)]
fn invoke_tool_via_socket(
    socket_path: &str,
    sandbox_id: &str,
    container_id: &str,
    tool_name: &str,
    input: &Value,
) -> Result<Value> {
    use std::os::unix::net::UnixStream;

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
