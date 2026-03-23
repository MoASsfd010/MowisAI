//! Five-layer orchestration public entry.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::architect::create_blueprint;
use super::context_gatherer::gather_context;
use super::sandbox_manager::run_sandbox;
use super::sandbox_owner::create_sandbox_plan;
use super::types::{SandboxResult, SandboxExecutionPlan};
use super::{gcloud_access_token, vertex_generate_url, HTTP_TIMEOUT_SECS};

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
    println!("[layer1] Gathering project context...");
    let context = gather_context(prompt, project_id, socket_path).context("layer1 context")?;

    println!("[layer2] Creating implementation blueprint...");
    let mut blueprint = create_blueprint(&context, project_id).context("layer2 architect")?;
    cap_agents(&mut blueprint, max_agents);
    println!("[layer2] Blueprint sandboxes: {}", blueprint.sandboxes.len());

    println!("[layer3] Building per-sandbox execution plans...");
    let plans = create_sandbox_plans_parallel(&context, &blueprint, project_id, socket_path)?;
    println!("[layer3] Created {} sandbox plan(s)", plans.len());

    println!("[layer4] Running sandbox managers...");
    let sandbox_results = run_sandbox_managers_parallel(&plans, project_id, socket_path)?;

    println!("[layer5] Synthesizing final output...");
    let final_text = synthesize(prompt, &context, &blueprint, &sandbox_results, project_id)?;
    println!("[result] {}", final_text);
    Ok(final_text)
}

#[cfg(unix)]
fn create_sandbox_plans_parallel(
    context: &super::types::ProjectContext,
    blueprint: &super::types::ImplementationBlueprint,
    project_id: &str,
    socket_path: &str,
) -> Result<Vec<SandboxExecutionPlan>> {
    let (tx, rx) = mpsc::channel::<Result<SandboxExecutionPlan>>();
    for cfg in blueprint.sandboxes.clone() {
        let tx = tx.clone();
        let context = context.clone();
        let project_id = project_id.to_string();
        let socket_path = socket_path.to_string();
        thread::spawn(move || {
            let res = create_sandbox_plan(&context, &cfg, &project_id, &socket_path);
            let _ = tx.send(res);
        });
    }
    drop(tx);

    let mut out = Vec::new();
    for recv in rx {
        out.push(recv?);
    }
    Ok(out)
}

#[cfg(unix)]
fn run_sandbox_managers_parallel(
    plans: &[SandboxExecutionPlan],
    project_id: &str,
    socket_path: &str,
) -> Result<Vec<SandboxResult>> {
    let (tx, rx) = mpsc::channel::<Result<SandboxResult>>();
    for plan in plans.iter().cloned() {
        let tx = tx.clone();
        let project_id = project_id.to_string();
        let socket_path = socket_path.to_string();
        thread::spawn(move || {
            let res = run_sandbox(&plan, &project_id, &socket_path);
            let _ = tx.send(res);
        });
    }
    drop(tx);

    let mut out = Vec::new();
    for recv in rx {
        out.push(recv?);
    }
    Ok(out)
}

#[cfg(unix)]
fn cap_agents(blueprint: &mut super::types::ImplementationBlueprint, max_agents: usize) {
    let total: usize = blueprint.sandboxes.iter().map(|s| s.agent_count).sum();
    if total <= max_agents.max(1) {
        return;
    }
    let ratio = max_agents.max(1) as f64 / total as f64;
    for sb in &mut blueprint.sandboxes {
        let scaled = ((sb.agent_count as f64) * ratio).round() as usize;
        sb.agent_count = scaled.max(1);
    }
}

#[cfg(unix)]
fn synthesize(
    prompt: &str,
    context: &super::types::ProjectContext,
    blueprint: &super::types::ImplementationBlueprint,
    results: &[SandboxResult],
    project_id: &str,
) -> Result<String> {
    let url = vertex_generate_url(project_id);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let mut summary = String::new();
    for r in results {
        summary.push_str(&format!(
            "\n## Sandbox {}\n- success: {}\n- workers: {}\n",
            r.sandbox_id,
            r.success,
            r.agent_results.len()
        ));
    }
    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "text": format!(
                    "User prompt:\n{}\n\nProject context:\n{}\n\nBlueprint:\n{}\n\nSandbox results:\n{}\n\nProvide final concise delivery summary and any unresolved issues.",
                    prompt,
                    serde_json::to_string_pretty(context).unwrap_or_default(),
                    serde_json::to_string_pretty(blueprint).unwrap_or_default(),
                    summary
                )
            }]
        }],
        "generationConfig": { "temperature": 0.35 }
    });

    let token = gcloud_access_token()?;
    let resp = client
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("synthesis HTTP")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        return Err(anyhow!("synthesis error {}: {}", status, text));
    }
    let data: Value = resp.json().context("parse synthesis JSON")?;
    let text = data
        .pointer("/candidates/0/content/parts/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(anyhow!("synthesis empty output"));
    }
    Ok(text)
}
