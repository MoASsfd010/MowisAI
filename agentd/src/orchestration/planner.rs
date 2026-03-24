//! Decompose a user prompt into a structured `Plan` via Vertex Gemini 2.5 Pro.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

use super::{
    gcloud_access_token, vertex_generate_url, vertex_generation_config_json, HTTP_TIMEOUT_SECS,
};

/// One unit of work for the executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    /// e.g. frontend-team, backend-team, devops-team, testing-team, data-team, general
    pub team_type: String,
    pub agent_instruction: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub required_packages: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub tasks: Vec<PlanTask>,
}

#[derive(Debug, Deserialize)]
struct PlanJson {
    #[serde(default)]
    tasks: Vec<PlanTask>,
}

/// Ask Gemini (Vertex) to emit a JSON plan; `max_tasks` caps how many tasks it should create.
pub fn plan(prompt: &str, project_id: &str, max_tasks: usize) -> Result<Plan> {
    #[cfg(not(unix))]
    {
        let _ = (prompt, project_id, max_tasks);
        return Err(anyhow!(
            "orchestration planner requires Unix (gcloud / deployment target)"
        ));
    }

    #[cfg(unix)]
    {
        plan_inner(prompt, project_id, max_tasks)
    }
}

#[cfg(unix)]
fn plan_inner(prompt: &str, project_id: &str, max_tasks: usize) -> Result<Plan> {
    let url = vertex_generate_url(project_id);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let instruction = format!(
        r#"You are a planning engine for a multi-agent dev system. Break the user's goal into independent subtasks.

Rules:
- Emit ONLY valid JSON (no markdown fences, no commentary).
- At most {} tasks.
- Each task has: id (string, unique), team_type (one of: frontend-team, backend-team, devops-team, testing-team, data-team, security-team, general), agent_instruction (specific prompt for that agent), dependencies (array of task ids that must finish first, can be empty), required_packages (extra Alpine apk names beyond the team defaults, can be empty).
- Order tasks logically; use dependencies for real ordering needs (e.g. API before frontend).

JSON shape:
{{"tasks":[{{"id":"...","team_type":"...","agent_instruction":"...","dependencies":[],"required_packages":[]}}]}}

User goal:
{}"#,
        max_tasks, prompt
    );

    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": instruction }]
        }],
        "generationConfig": vertex_generation_config_json(0.35)
    });

    let token = gcloud_access_token()?;
    let http_resp = client
        .post(&url)
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("planner generateContent HTTP")?;

    if !http_resp.status().is_success() {
        let status = http_resp.status();
        let text = http_resp.text().unwrap_or_default();
        return Err(anyhow!("Vertex AI planner error {}: {}", status, text));
    }

    let response: Value = http_resp.json().context("parse planner Vertex JSON")?;

    if let Some(block) = response
        .pointer("/promptFeedback/blockReason")
        .and_then(|v| v.as_str())
    {
        return Err(anyhow!("planner prompt blocked: {}", block));
    }

    let text = extract_model_text(&response)?;
    let parsed: PlanJson = serde_json::from_str(text.trim()).or_else(|_| {
        // Fallback: strip optional code fence
        let t = text.trim();
        let inner = if let Some(i) = t.find('{') {
            if let Some(j) = t.rfind('}') {
                &t[i..=j]
            } else {
                t
            }
        } else {
            t
        };
        serde_json::from_str::<PlanJson>(inner).map_err(|e| anyhow!("plan JSON parse: {} — snippet: {}", e, inner.chars().take(200).collect::<String>()))
    })?;

    if parsed.tasks.len() > max_tasks {
        return Err(anyhow!(
            "planner returned {} tasks; max is {}",
            parsed.tasks.len(),
            max_tasks
        ));
    }

    Ok(Plan {
        tasks: parsed.tasks,
    })
}

fn extract_model_text(response: &Value) -> Result<String> {
    let candidate = response
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("planner: no candidates"))?;
    let parts = candidate
        .pointer("/content/parts")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow!("planner: missing content.parts"))?;

    let mut buf = String::new();
    for part in parts {
        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
            buf.push_str(t);
        }
    }
    if buf.trim().is_empty() {
        return Err(anyhow!("planner: empty model text: {}", response));
    }
    Ok(buf)
}
