//! Public entry: plan → execute → synthesize (all via Vertex Gemini 2.5 Pro).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

use super::executor::execute;
use super::planner::plan;
use super::AgentOutput;
use super::{gcloud_access_token, vertex_generate_url, HTTP_TIMEOUT_SECS};

/// End-to-end orchestration: same LLM (Gemini 2.5 Pro) for planning, every agent, and final synthesis.
pub fn run(
    prompt: &str,
    project_id: &str,
    socket_path: &str,
    max_agents: usize,
) -> Result<String> {
    #[cfg(not(unix))]
    {
        let _ = (prompt, project_id, socket_path, max_agents);
        return Err(anyhow!(
            "orchestration requires Unix (agentd uses Unix domain sockets)"
        ));
    }

    #[cfg(unix)]
    {
        run_inner(prompt, project_id, socket_path, max_agents)
    }
}

#[cfg(unix)]
fn run_inner(
    prompt: &str,
    project_id: &str,
    socket_path: &str,
    max_agents: usize,
) -> Result<String> {
    println!("[orchestrator] Analyzing task…");
    let max_tasks = max_agents.max(1);
    let p = plan(prompt, project_id, max_tasks).context("planning")?;
    println!(
        "[orchestrator] Plan: {} task(s) across team types",
        p.tasks.len()
    );

    let outputs: HashMap<String, super::AgentOutput> =
        execute(&p, socket_path, project_id, max_agents).context("execute")?;

    println!("[orchestrator] Synthesizing final result…");
    let synthesized = synthesize(prompt, &outputs, project_id)?;
    println!("[result] {}", synthesized);
    Ok(synthesized)
}

#[cfg(unix)]
fn synthesize(
    original_prompt: &str,
    outputs: &HashMap<String, AgentOutput>,
    project_id: &str,
) -> Result<String> {
    let url = vertex_generate_url(project_id);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let mut body_text = String::new();
    body_text.push_str("Original user request:\n");
    body_text.push_str(original_prompt);
    body_text.push_str("\n\n--- Agent task outputs ---\n");
    for (id, o) in outputs {
        body_text.push_str(&format!(
            "\n## Task {}\n- success: {}\n- summary:\n{}\n- files_created: {:?}\n",
            id, o.success, o.output, o.files_created
        ));
    }
    body_text.push_str(
        "\nProduce one coherent final answer for the user: what was accomplished, key artifacts, and any follow-ups. Be concise but complete.",
    );

    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": body_text }]
        }],
        "generationConfig": {
            "temperature": 0.45
        }
    });

    let token = gcloud_access_token()?;
    let http_resp = client
        .post(&url)
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("synthesis generateContent HTTP")?;

    if !http_resp.status().is_success() {
        let status = http_resp.status();
        let text = http_resp.text().unwrap_or_default();
        return Err(anyhow!("Vertex synthesis error {}: {}", status, text));
    }

    let response: Value = http_resp.json().context("parse synthesis JSON")?;
    extract_text(&response)
}

#[cfg(unix)]
fn extract_text(response: &Value) -> Result<String> {
    let candidate = response
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("synthesis: no candidates"))?;
    let parts = candidate
        .pointer("/content/parts")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow!("synthesis: missing parts"))?;
    let mut s = String::new();
    for part in parts {
        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
            s.push_str(t);
        }
    }
    let t = s.trim().to_string();
    if t.is_empty() {
        return Err(anyhow!("synthesis: empty text"));
    }
    Ok(t)
}
