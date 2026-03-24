//! Session coordinator: one Gemini call that turns full transcript + `ProjectContext` into a live
//! briefing prepended to each worker’s `AgentTask.context` so the next tool calls are project-aware.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use super::types::ProjectContext;
use super::{gcloud_access_token, vertex_generate_url, vertex_generation_config, HTTP_TIMEOUT_SECS};

pub fn live_worker_briefing(
    transcript: &[String],
    context: &ProjectContext,
    project_id: &str,
) -> Result<String> {
    #[cfg(not(unix))]
    {
        let _ = (transcript, context, project_id);
        return Err(anyhow::anyhow!(
            "coordinator requires Unix for this orchestration build"
        ));
    }
    #[cfg(unix)]
    {
        live_worker_briefing_inner(transcript, context, project_id)
    }
}

#[cfg(unix)]
fn live_worker_briefing_inner(
    transcript: &[String],
    context: &ProjectContext,
    project_id: &str,
) -> Result<String> {
    let url = vertex_generate_url(project_id);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("coordinator reqwest client")?;

    let chat = transcript.join("\n\n---\n");
    let instruction = format!(
        "You are the Session Coordinator. The user works in an interactive multi-agent dev loop.\n\
         Produce a concise briefing (plain text, no JSON) for specialist workers who will edit code next.\n\
         Include: current goal, constraints, tech stack, what changed across turns, open questions.\n\
         Do not repeat the entire chat — synthesize.\n\n\
         ProjectContext (structured):\n{}\n\n\
         Full user transcript (chronological):\n{}\n",
        serde_json::to_string_pretty(context).unwrap_or_default(),
        chat
    );

    let body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": instruction }] }],
        "generationConfig": vertex_generation_config(0.2)
    });

    let token = gcloud_access_token()?;
    let resp = client
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("coordinator generateContent HTTP")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        return Err(anyhow::anyhow!(
            "coordinator error {}: {}",
            status,
            text
        ));
    }

    let data: Value = resp.json().context("parse coordinator JSON")?;
    let mut out = String::new();
    if let Some(parts) = data
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
    {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                out.push_str(t);
            }
        }
    }
    let outx = out.trim().to_string();
    if outx.is_empty() {
        return Err(anyhow::anyhow!("coordinator empty output"));
    }
    Ok(outx)
}
