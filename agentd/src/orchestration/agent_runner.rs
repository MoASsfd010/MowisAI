//! Single-agent Vertex loop in one container (same tools as `vertex_agent`).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use super::{
    gemini_tool_declarations, gcloud_access_token, invoke_tool_via_socket, vertex_generate_url,
    HTTP_TIMEOUT_SECS, MAX_TOOL_ROUNDS,
};

#[derive(Debug, Clone)]
pub struct AgentOutput {
    pub task_id: String,
    pub success: bool,
    pub output: String,
    pub files_created: Vec<String>,
}

/// Run Gemini 2.5 with tools against one sandbox+container; `log_prefix` e.g. `[agent:backend-1]`.
pub fn run_agent(
    socket_path: &str,
    project_id: &str,
    sandbox_id: &str,
    container_id: &str,
    task_id: &str,
    role_name: &str,
    task_instruction: &str,
    context_from_deps: &str,
    log_prefix: &str,
) -> Result<AgentOutput> {
    #[cfg(not(unix))]
    {
        let _ = (
            socket_path,
            project_id,
            sandbox_id,
            container_id,
            task_id,
            role_name,
            task_instruction,
            context_from_deps,
            log_prefix,
        );
        return Err(anyhow!(
            "agent_runner requires Unix (agentd uses Unix domain sockets)"
        ));
    }

    #[cfg(unix)]
    {
        run_agent_inner(
            socket_path,
            project_id,
            sandbox_id,
            container_id,
            task_id,
            role_name,
            task_instruction,
            context_from_deps,
            log_prefix,
        )
    }
}

#[cfg(unix)]
fn run_agent_inner(
    socket_path: &str,
    project_id: &str,
    sandbox_id: &str,
    container_id: &str,
    task_id: &str,
    role_name: &str,
    task_instruction: &str,
    context_from_deps: &str,
    log_prefix: &str,
) -> Result<AgentOutput> {
    let url = vertex_generate_url(project_id);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let mut ctx_block = String::new();
    if !context_from_deps.trim().is_empty() {
        ctx_block = format!(
            "\n\nOutputs from tasks you depend on (use as ground truth; do not assume other paths exist):\n{}",
            context_from_deps
        );
    }

    let system_text = format!(
        "You are an AI agent in a Linux Alpine sandbox, role: {}. \
         You control the environment only via the provided tools (run_command, write_file, read_file, create_directory, list_files). \
         Prefer working under /workspace. List directories before assuming files exist. \
         Complete the assigned task thoroughly; when done, respond with a concise summary of what you did (no further tool calls).{}",
        role_name, ctx_block
    );

    let system_instruction = json!({
        "parts": [{ "text": system_text }]
    });

    let user_text = format!(
        "Task (id {}):\n{}",
        task_id, task_instruction
    );

    let mut contents: Vec<Value> = vec![json!({
        "role": "user",
        "parts": [{ "text": user_text }]
    })];

    let tools = json!([{
        "functionDeclarations": gemini_tool_declarations()
    }]);

    let mut files_created: Vec<String> = Vec::new();
    let mut tool_failures = false;

    for round in 0..MAX_TOOL_ROUNDS {
        let body = json!({
            "contents": contents,
            "tools": tools,
            "systemInstruction": system_instruction,
            "generationConfig": {
                "temperature": 0.5
            }
        });

        println!(
            "{} → Gemini (round {}) …",
            log_prefix,
            round + 1
        );
        let token = gcloud_access_token()?;
        let http_resp = client
            .post(&url)
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .context("agent generateContent HTTP")?;

        if !http_resp.status().is_success() {
            let status = http_resp.status();
            let text = http_resp.text().unwrap_or_default();
            return Ok(AgentOutput {
                task_id: task_id.to_string(),
                success: false,
                output: format!("Vertex error {}: {}", status, text),
                files_created,
            });
        }

        let response: Value = http_resp.json().context("parse agent Vertex JSON")?;

        if let Some(block) = response
            .pointer("/promptFeedback/blockReason")
            .and_then(|v| v.as_str())
        {
            return Ok(AgentOutput {
                task_id: task_id.to_string(),
                success: false,
                output: format!("prompt blocked: {}", block),
                files_created,
            });
        }

        let candidate = response
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("no candidates: {}", response))?;

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
        let mut round_text = String::new();

        for part in &parts {
            if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = fc.get("args").cloned().unwrap_or(json!({}));
                println!("{} tool_call: {} {}", log_prefix, name, args);
                function_calls.push((name, args));
                model_parts.push(part.clone());
            } else if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    println!("{} {}", log_prefix, t);
                    round_text.push_str(t);
                    round_text.push('\n');
                }
                model_parts.push(part.clone());
            } else {
                model_parts.push(part.clone());
            }
        }

        if function_calls.is_empty() {
            let out = round_text.trim().to_string();
            return Ok(AgentOutput {
                task_id: task_id.to_string(),
                success: !tool_failures && !out.is_empty(),
                output: if out.is_empty() {
                    "(no final text)".to_string()
                } else {
                    out
                },
                files_created,
            });
        }

        contents.push(json!({
            "role": "model",
            "parts": model_parts
        }));

        let mut response_parts = Vec::new();
        for (name, args) in &function_calls {
            let tool_result = invoke_tool_via_socket(
                socket_path,
                sandbox_id,
                container_id,
                name,
                args,
            )?;
            println!("{} tool_result: {} → {}", log_prefix, name, tool_result);
            if name == "write_file" {
                if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                    if tool_result.get("success").and_then(|v| v.as_bool()) != Some(false)
                        && tool_result.get("error").is_none()
                    {
                        files_created.push(p.to_string());
                    } else {
                        tool_failures = true;
                    }
                }
            }
            if tool_result.get("success").and_then(|v| v.as_bool()) == Some(false)
                || tool_result.get("error").is_some()
            {
                if name == "run_command" {
                    tool_failures = true;
                }
            }
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

    Ok(AgentOutput {
        task_id: task_id.to_string(),
        success: false,
        output: format!(
            "exceeded max tool rounds ({})",
            MAX_TOOL_ROUNDS
        ),
        files_created,
    })
}
