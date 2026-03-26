use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

use super::types::{AgentTask, ProjectContext, SandboxConfig, SandboxExecutionPlan};
use super::{
    gcloud_access_token, parse_ok_field, socket_roundtrip, trace, vertex_generation_config_json,
    HTTP_TIMEOUT_SECS,
};

#[derive(Debug, Deserialize)]
struct PlanJson {
    #[serde(default)]
    agents: Vec<AgentTask>,
    #[serde(default)]
    dependency_order: Vec<Vec<String>>,
}

pub fn create_sandbox_plan(
    context: &ProjectContext,
    config: &SandboxConfig,
    project_id: &str,
    socket_path: &str,
    reuse_sandbox_id: Option<&str>,
) -> Result<SandboxExecutionPlan> {
    #[cfg(not(unix))]
    {
        let _ = (context, config, project_id, socket_path, reuse_sandbox_id);
        return Err(anyhow!(
            "sandbox owner requires Unix (agentd uses Unix domain sockets)"
        ));
    }

    #[cfg(unix)]
    {
        create_sandbox_plan_inner(
            context,
            config,
            project_id,
            socket_path,
            reuse_sandbox_id,
        )
    }
}

#[cfg(unix)]
fn create_sandbox_plan_inner(
    context: &ProjectContext,
    config: &SandboxConfig,
    project_id: &str,
    socket_path: &str,
    reuse_sandbox_id: Option<&str>,
) -> Result<SandboxExecutionPlan> {
    trace(&format!(
        "layer3/owner: create_sandbox_plan start sandbox={} agents={}",
        config.name, config.agent_count
    ));
    let sandbox_id = if let Some(existing) = reuse_sandbox_id {
        trace(&format!(
            "layer3/owner: reusing sandbox id={} for team={} (interactive session)",
            existing, config.name
        ));
        existing.to_string()
    } else {
        let req = json!({
            "request_type": "create_sandbox",
            "image": config.os,
        "packages": config.packages,
        "backend": "guest_vm"
        });
        let resp = socket_roundtrip(socket_path, &req)?;
        let id = parse_ok_field(&resp, "sandbox")?;
        trace(&format!(
            "layer3/owner: sandbox ready name={} id={}",
            config.name, id
        ));
        id
    };

    let url = format!(
        "https://us-central1-aiplatform.googleapis.com/v1/projects/{}/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
        project_id
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let prompt = format!(
        "Create an execution plan for sandbox '{}'.\n\
         You must output JSON only with shape:\n\
         {{\"agents\":[{{\"agent_id\":\"...\",\"task\":\"...\",\"files\":[],\"tools\":[],\"context\":\"...\"}}],\"dependency_order\":[[\"agent-id-1\"]]}}\n\n\
         Constraints:\n\
         - exactly {} agents\n\
         - use stable agent_id values when possible (e.g. `{}-worker-01`, `{}-worker-02`) so interactive sessions can reuse the same containers across turns\n\
         - each agent tools must be subset of sandbox tools: {:?}\n\
         - files should be concrete relative paths\n\
         - task scope must align with deliverable: {}\n\n\
         Project context:\n\
         project_name: {}\n\
         description: {}\n\
         tech_stack: {:?}\n\
         existing_structure: {}\n\
         key_files: {:?}\n\
         constraints: {:?}\n\
         task_summary: {}",
        config.name,
        config.agent_count,
        config.name,
        config.name,
        config.tools,
        config.deliverable,
        context.project_name,
        context.description,
        context.tech_stack,
        context.existing_structure,
        context.key_files,
        context.constraints,
        context.task_summary
    );

    let body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": prompt }] }],
        "generationConfig": vertex_generation_config_json(0.3)
    });

    let token = gcloud_access_token()?;
    let start = std::time::Instant::now();
    let http_resp = client
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("sandbox owner generateContent HTTP")?;
    trace(&format!(
        "layer3/owner: planning response sandbox={} status={} elapsed_ms={}",
        config.name,
        http_resp.status(),
        start.elapsed().as_millis()
    ));

    if !http_resp.status().is_success() {
        let status = http_resp.status();
        let text = http_resp.text().unwrap_or_default();
        return Err(anyhow!("sandbox owner error {}: {}", status, text));
    }

    let data: Value = http_resp.json().context("parse sandbox owner JSON")?;
    let text = extract_model_text(&data)?;
    let mut parsed: PlanJson = parse_plan_json(&text)?;
    trace(&format!(
        "layer3/owner: parsed plan sandbox={} agents={} groups={}",
        config.name,
        parsed.agents.len(),
        parsed.dependency_order.len()
    ));

    if parsed.agents.is_empty() {
        parsed.agents = fallback_agents(config);
    }
    if parsed.dependency_order.is_empty() {
        parsed.dependency_order = vec![parsed.agents.iter().map(|a| a.agent_id.clone()).collect()];
    }

    for agent in &mut parsed.agents {
        if agent.tools.is_empty() {
            agent.tools = config.tools.clone();
        } else {
            agent.tools.retain(|t| config.tools.iter().any(|x| x == t));
            if agent.tools.is_empty() {
                agent.tools = config.tools.clone();
            }
        }
    }

    Ok(SandboxExecutionPlan {
        sandbox_team: config.name.clone(),
        sandbox_id,
        agents: parsed.agents,
        dependency_order: parsed.dependency_order,
    })
}

fn fallback_agents(config: &SandboxConfig) -> Vec<AgentTask> {
    let mut out = Vec::new();
    for i in 0..config.agent_count.max(1) {
        out.push(AgentTask {
            agent_id: format!("{}-agent-{:02}", config.name, i + 1),
            task: format!("Contribute to {}", config.deliverable),
            files: Vec::new(),
            tools: config.tools.clone(),
            context: "Fallback plan generated by sandbox owner".to_string(),
        });
    }
    out
}

fn extract_model_text(data: &Value) -> Result<String> {
    let parts = data
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("sandbox owner: no content parts"))?;
    let mut out = String::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            out.push_str(t);
        }
    }
    if out.trim().is_empty() {
        return Err(anyhow!("sandbox owner: empty model text"));
    }
    Ok(out)
}

fn parse_plan_json(text: &str) -> Result<PlanJson> {
    match serde_json::from_str::<PlanJson>(text.trim()) {
        Ok(v) => Ok(v),
        Err(_) => {
            let trimmed = text.trim();
            let start = trimmed.find('{').ok_or_else(|| anyhow!("no JSON start"))?;
            let end = trimmed.rfind('}').ok_or_else(|| anyhow!("no JSON end"))?;
            serde_json::from_str::<PlanJson>(&trimmed[start..=end]).context("parse plan json")
        }
    }
}
