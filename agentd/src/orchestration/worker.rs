use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

use super::types::{AgentResult, AgentTask};
use super::{
    gemini_tool_declarations, gcloud_access_token, invoke_tool_via_socket, trace, HTTP_TIMEOUT_SECS,
    MAX_TOOL_ROUNDS,
};

pub fn run_worker(
    task: &AgentTask,
    sandbox_id: &str,
    container_id: &str,
    project_id: &str,
    socket_path: &str,
) -> Result<AgentResult> {
    #[cfg(not(unix))]
    {
        let _ = (task, sandbox_id, container_id, project_id, socket_path);
        return Err(anyhow!(
            "worker requires Unix (agentd uses Unix domain sockets)"
        ));
    }

    #[cfg(unix)]
    {
        run_worker_inner(task, sandbox_id, container_id, project_id, socket_path)
    }
}

#[cfg(unix)]
fn run_worker_inner(
    task: &AgentTask,
    sandbox_id: &str,
    container_id: &str,
    project_id: &str,
    socket_path: &str,
) -> Result<AgentResult> {
    trace(&format!(
        "layer5/worker: start agent={} sandbox={} container={}",
        task.agent_id, sandbox_id, container_id
    ));
    let model = "gemini-2.5-pro";
    let url = vertex_generate_url_for_model(project_id, model);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let system_prompt = format!(
        "You are Worker Agent {} inside an isolated sandbox container.\n\n\
         TASK:\n{}\n\n\
         FILES YOU OWN:\n{}\n\n\
         CONTEXT:\n{}\n\n\
         RULES:\n\
         1) Use only your allowed tools.\n\
         2) Prefer edits only to owned files unless absolutely required.\n\
         3) When done, provide concise completion summary.\n\
         4) If this is a git repo, generate diff via git_diff on path '.'.",
        task.agent_id,
        task.task,
        if task.files.is_empty() {
            "(not specified)".to_string()
        } else {
            task.files.join(", ")
        },
        task.context
    );

    let mut contents = vec![json!({
        "role": "user",
        "parts": [{ "text": task.task }]
    })];
    let tools = filtered_tool_declarations(&task.tools);
    trace(&format!(
        "layer5/worker: tool declarations agent={} requested_tools={} resolved_declarations={}",
        task.agent_id,
        task.tools.len(),
        tools.len()
    ));

    let mut completion = String::new();
    let mut touched_files: HashSet<String> = HashSet::new();

    for round in 0..MAX_TOOL_ROUNDS {
        let body = if tools.is_empty() {
            // Vertex rejects an empty tool wrapper; send no tools key instead.
            json!({
                "contents": contents,
                "systemInstruction": { "parts": [{ "text": system_prompt }] },
                "generationConfig": { "temperature": 0.4 }
            })
        } else {
            json!({
                "contents": contents,
                "tools": [{ "function_declarations": tools }],
                "systemInstruction": { "parts": [{ "text": system_prompt }] },
                "generationConfig": { "temperature": 0.4 }
            })
        };

        trace(&format!(
            "layer5/worker: round {} agent={}",
            round + 1,
            task.agent_id
        ));
        let token = gcloud_access_token()?;
        let start = std::time::Instant::now();
        let resp = client
            .post(&url)
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .context("worker generateContent HTTP")?;
        trace(&format!(
            "layer5/worker: response round={} agent={} model={} status={} elapsed_ms={}",
            round + 1,
            task.agent_id,
            model,
            resp.status(),
            start.elapsed().as_millis()
        ));

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            trace(&format!(
                "layer5/worker: vertex error round={} agent={} model={} status={} body={}",
                round + 1,
                task.agent_id,
                model,
                status,
                text
            ));
            return Ok(AgentResult {
                agent_id: task.agent_id.clone(),
                success: false,
                summary: format!("Vertex error (model {}) {}: {}", model, status, text),
                diff: String::new(),
                files_changed: Vec::new(),
            });
        }

        let data: Value = resp.json().context("parse worker vertex JSON")?;
        let candidate = data
            .get("candidates")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .ok_or_else(|| anyhow!("worker: no candidates in response"))?;
        let parts = candidate
            .pointer("/content/parts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut model_parts = Vec::new();
        let mut calls: Vec<(String, Value)> = Vec::new();
        for p in &parts {
            if let Some(fc) = p.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = fc.get("args").cloned().unwrap_or(json!({}));
                calls.push((name, args));
                model_parts.push(p.clone());
            } else if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    completion = text.to_string();
                }
                model_parts.push(p.clone());
            } else {
                model_parts.push(p.clone());
            }
        }

        if calls.is_empty() {
            let diff = collect_diff(socket_path, sandbox_id, container_id);
            let mut files_changed = extract_files_from_diff(&diff);
            for f in touched_files.drain() {
                if !files_changed.iter().any(|x| x == &f) {
                    files_changed.push(f);
                }
            }
            return Ok(AgentResult {
                agent_id: task.agent_id.clone(),
                success: !completion.trim().is_empty(),
                summary: if completion.trim().is_empty() {
                    "(no final text)".to_string()
                } else {
                    completion
                },
                diff,
                files_changed,
            });
        }

        contents.push(json!({
            "role": "model",
            "parts": model_parts
        }));

        let mut response_parts = Vec::new();
        for (name, args) in calls {
            trace(&format!(
                "layer5/worker: tool_call agent={} tool={}",
                task.agent_id, name
            ));
            if !task.tools.is_empty() && !task.tools.iter().any(|t| t == &name) {
                let blocked = json!({
                    "error": format!("tool '{}' is not allowed for this worker", name),
                    "success": false
                });
                response_parts.push(json!({
                    "functionResponse": { "name": name, "response": blocked }
                }));
                continue;
            }

            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                touched_files.insert(path.to_string());
            }
            if let Some(path) = args.get("to").and_then(|v| v.as_str()) {
                touched_files.insert(path.to_string());
            }
            if let Some(path) = args.get("dst").and_then(|v| v.as_str()) {
                touched_files.insert(path.to_string());
            }

            let tool_result =
                invoke_tool_via_socket(socket_path, sandbox_id, container_id, &name, &args)?;
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

    Ok(AgentResult {
        agent_id: task.agent_id.clone(),
        success: false,
        summary: format!("exceeded max rounds ({})", MAX_TOOL_ROUNDS),
        diff: String::new(),
        files_changed: Vec::new(),
    })
}

fn vertex_generate_url_for_model(project_id: &str, model: &str) -> String {
    format!(
        "https://us-central1-aiplatform.googleapis.com/v1/projects/{}/locations/us-central1/publishers/google/models/{}:generateContent",
        project_id, model
    )
}

fn filtered_tool_declarations(allowed_tools: &[String]) -> Vec<Value> {
    let all = gemini_tool_declarations();
    let all_arr = all.as_array().cloned().unwrap_or_default();
    if allowed_tools.is_empty() {
        return all_arr;
    }
    let filtered: Vec<Value> = all_arr
        .into_iter()
        .filter(|d| {
            d.get("name")
                .and_then(|n| n.as_str())
                .map(|name| allowed_tools.iter().any(|t| t == name))
                .unwrap_or(false)
        })
        .collect();
    // If planner/tool names are mismatched, do not send an empty tools wrapper.
    // Fall back to full declarations so workers remain functional.
    if filtered.is_empty() {
        gemini_tool_declarations()
            .as_array()
            .cloned()
            .unwrap_or_default()
    } else {
        filtered
    }
}

fn collect_diff(socket_path: &str, sandbox_id: &str, container_id: &str) -> String {
    let args = json!({ "path": "." });
    match invoke_tool_via_socket(socket_path, sandbox_id, container_id, "git_diff", &args) {
        Ok(v) => v
            .get("diff")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string(),
        Err(_) => String::new(),
    }
}

fn extract_files_from_diff(diff: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            let p = path.trim();
            if !p.is_empty() && p != "/dev/null" && !out.iter().any(|x| x == p) {
                out.push(p.to_string());
            }
        }
    }
    out
}
