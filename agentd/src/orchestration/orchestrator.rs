//! Five-layer orchestration public entry.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::architect::create_blueprint;
use super::context_gatherer::gather_context;
use super::coordinator::live_worker_briefing;
use super::sandbox_manager::run_sandbox;
use super::sandbox_owner::create_sandbox_plan;
use super::session_store::{self, InteractiveSessionSnapshot};
use super::types::{
    ImplementationBlueprint, ProjectContext, SandboxExecutionPlan, SandboxResult, SandboxWarmState,
};
use super::{
    gcloud_access_token, trace, vertex_generate_url, vertex_generation_config, HTTP_TIMEOUT_SECS,
    VERTEX_MAX_OUTPUT_TOKENS, debug_enabled,
};

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
        run_session_first(prompt, project_id, socket_path, max_agents).map(|(_, answer)| answer)
    }
}

/// Live CLI session: reuses **agentd sandboxes** by team name, **merge git repo** per sandbox id, and
/// **worker containers** when the same `agent_id` appears again. A coordinator Gemini call fills a
/// live briefing merged into each worker’s context. Use `--session-file` to persist transcript + warm map.
#[cfg(unix)]
pub struct OrchestrationInteractiveSession {
    pub project_id: String,
    pub socket_path: String,
    pub max_agents: usize,
    pub context: ProjectContext,
    /// Full user transcript for synthesis (all turns).
    pub transcript: Vec<String>,
    /// Blueprint `SandboxConfig.name` → agentd sandbox id (reuse across REPL turns).
    pub sandbox_by_team: HashMap<String, String>,
    /// Per `sandbox_id`: reused worker containers (`agent_id` → container id) + merge container id.
    pub warm_by_sandbox: Arc<Mutex<HashMap<String, SandboxWarmState>>>,
    /// Model synthesis text per completed turn (most recent last).
    pub assistant_turns: Vec<String>,
}

#[cfg(unix)]
impl OrchestrationInteractiveSession {
    /// Restore from `session_store::write_snapshot` output.
    pub fn from_session_file(path: &Path) -> Result<Self> {
        let snap = session_store::read_snapshot(path)?;
        Ok(Self {
            project_id: snap.project_id,
            socket_path: snap.socket_path,
            max_agents: snap.max_agents,
            context: snap.context,
            transcript: snap.transcript,
            sandbox_by_team: snap.sandbox_by_team,
            warm_by_sandbox: Arc::new(Mutex::new(snap.warm_by_sandbox)),
            assistant_turns: snap.assistant_turns,
        })
    }

    pub fn save_session_file(&self, path: &Path) -> Result<()> {
        let warm = self
            .warm_by_sandbox
            .lock()
            .map_err(|e| anyhow!("warm mutex poisoned: {}", e))?
            .clone();
        let snap = InteractiveSessionSnapshot::new_v1(
            self.project_id.clone(),
            self.socket_path.clone(),
            self.max_agents,
            self.context.clone(),
            self.transcript.clone(),
            self.sandbox_by_team.clone(),
            warm,
            self.assistant_turns.clone(),
        );
        session_store::write_snapshot(path, &snap)
    }

    pub fn follow_up(&mut self, user_line: &str) -> Result<String> {
        verify_vertex_connectivity(&self.project_id)?;
        self.transcript.push(user_line.to_string());
        self.context
            .task_summary
            .push_str("\n\n---\nUser follow-up:\n");
        self.context.task_summary.push_str(user_line);

        if debug_enabled() {
            println!("[layer2] Creating implementation blueprint (follow-up)…");
        }
        let mut blueprint = create_blueprint(&self.context, &self.project_id)?;
        cap_agents(&mut blueprint, self.max_agents);
        if debug_enabled() {
            println!("[layer2] Blueprint sandboxes: {}", blueprint.sandboxes.len());
        }

        if debug_enabled() {
            println!("[layer3] Building per-sandbox execution plans…");
        }
        let mut plans = create_sandbox_plans_parallel(
            &self.context,
            &blueprint,
            &self.project_id,
            &self.socket_path,
            &self.sandbox_by_team,
        )?;
        for p in &plans {
            self.sandbox_by_team
                .insert(p.sandbox_team.clone(), p.sandbox_id.clone());
        }
        if debug_enabled() {
            println!("[layer3] Created {} sandbox plan(s)", plans.len());
        }

        apply_coordinator_briefing(&mut plans, &self.transcript, &self.context, &self.project_id)?;

        if debug_enabled() {
            println!("[layer4] Running sandbox managers…");
        }
        let sandbox_results = run_sandbox_managers_parallel(
            &plans,
            &self.project_id,
            &self.socket_path,
            Some(self.warm_by_sandbox.clone()),
        )?;

        if debug_enabled() {
            println!("[layer5] Synthesizing final output…");
        }
        let prompt_joined = self.transcript.join("\n\n---\n");
        let final_text = synthesize(
            &prompt_joined,
            &self.context,
            &blueprint,
            &sandbox_results,
            &self.project_id,
        )?;
        println!("[result] {}", final_text);
        self.assistant_turns.push(final_text.clone());
        Ok(final_text)
    }
}

/// First full pipeline run + session handle for [`OrchestrationInteractiveSession::follow_up`].
#[cfg(unix)]
pub fn run_session_first(
    prompt: &str,
    project_id: &str,
    socket_path: &str,
    max_agents: usize,
) -> Result<(OrchestrationInteractiveSession, String)> {
    trace("preflight: starting Vertex connectivity check");
    verify_vertex_connectivity(project_id)?;
    trace("preflight: Vertex connectivity check passed");

    if debug_enabled() {
        println!("[layer1] Gathering project context…");
    }
    let context = gather_context(prompt, project_id, socket_path).context("layer1 context")?;

    if debug_enabled() {
        println!("[layer2] Creating implementation blueprint…");
    }
    let mut blueprint = create_blueprint(&context, project_id).context("layer2 architect")?;
    cap_agents(&mut blueprint, max_agents);
    if debug_enabled() {
        println!("[layer2] Blueprint sandboxes: {}", blueprint.sandboxes.len());
    }

    if debug_enabled() {
        println!("[layer3] Building per-sandbox execution plans…");
    }
    let reuse = HashMap::new();
    let mut plans = create_sandbox_plans_parallel(
        &context,
        &blueprint,
        project_id,
        socket_path,
        &reuse,
    )?;
    let mut sandbox_by_team = HashMap::new();
    for p in &plans {
        sandbox_by_team.insert(p.sandbox_team.clone(), p.sandbox_id.clone());
    }
    if debug_enabled() {
        println!("[layer3] Created {} sandbox plan(s)", plans.len());
    }

    let transcript = vec![prompt.to_string()];
    apply_coordinator_briefing(&mut plans, &transcript, &context, project_id)?;

    let warm_arc = Arc::new(Mutex::new(HashMap::<String, SandboxWarmState>::new()));

    if debug_enabled() {
        println!("[layer4] Running sandbox managers…");
    }
    let sandbox_results = run_sandbox_managers_parallel(
        &plans,
        project_id,
        socket_path,
        Some(warm_arc.clone()),
    )?;

    if debug_enabled() {
        println!("[layer5] Synthesizing final output…");
    }
    let prompt_joined = transcript.join("\n\n---\n");
    let final_text = synthesize(
        &prompt_joined,
        &context,
        &blueprint,
        &sandbox_results,
        project_id,
    )?;
    println!("[result] {}", final_text);

    let session = OrchestrationInteractiveSession {
        project_id: project_id.to_string(),
        socket_path: socket_path.to_string(),
        max_agents,
        context,
        transcript,
        sandbox_by_team,
        warm_by_sandbox: warm_arc,
        assistant_turns: vec![final_text.clone()],
    };
    Ok((session, final_text))
}

#[cfg(unix)]
fn apply_coordinator_briefing(
    plans: &mut [SandboxExecutionPlan],
    transcript: &[String],
    context: &ProjectContext,
    project_id: &str,
) -> Result<()> {
    let briefing = match live_worker_briefing(transcript, context, project_id) {
        Ok(b) => b,
        Err(e) => {
            trace(&format!(
                "coordinator: could not refresh briefing (workers use owner context only): {}",
                e
            ));
            return Ok(());
        }
    };
    if briefing.trim().is_empty() {
        return Ok(());
    }
    trace("coordinator: merged live session briefing into worker task contexts");
    for p in plans.iter_mut() {
        for a in p.agents.iter_mut() {
            a.context = format!(
                "Coordinator briefing (live, session-wide):\n{}\n\n---\nOwner context:\n{}",
                briefing, a.context
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn verify_vertex_connectivity(project_id: &str) -> Result<()> {
    let url = vertex_generate_url(project_id);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("reqwest client preflight")?;
    let body = json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": "Reply with only: ok" }]
        }],
        "generationConfig": {
            "temperature": 0.0,
            "maxOutputTokens": VERTEX_MAX_OUTPUT_TOKENS.min(1024),
            "responseMimeType": "text/plain"
        }
    });
    let token = gcloud_access_token()?;
    let start = std::time::Instant::now();
    let resp = client
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("vertex preflight HTTP")?;
    trace(&format!(
        "preflight response status={} elapsed_ms={}",
        resp.status(),
        start.elapsed().as_millis()
    ));
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        return Err(anyhow!("vertex preflight failed {}: {}", status, text));
    }
    let data: Value = resp.json().context("parse vertex preflight JSON")?;
    let finish_reason = data
        .pointer("/candidates/0/finishReason")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN");
    let text = data
        .pointer("/candidates/0/content/parts/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    trace(&format!(
        "preflight parsed finish_reason={} text_len={}",
        finish_reason,
        text.len()
    ));
    if text.is_empty() || !text.contains("ok") {
        return Err(anyhow!(
            "vertex preflight returned unexpected output (finish_reason={}); raw={}",
            finish_reason,
            data
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn create_sandbox_plans_parallel(
    context: &ProjectContext,
    blueprint: &ImplementationBlueprint,
    project_id: &str,
    socket_path: &str,
    reuse_sandboxes: &HashMap<String, String>,
) -> Result<Vec<SandboxExecutionPlan>> {
    let (tx, rx) = mpsc::channel::<Result<SandboxExecutionPlan>>();
    for cfg in blueprint.sandboxes.clone() {
        let tx = tx.clone();
        let context = context.clone();
        let project_id = project_id.to_string();
        let socket_path = socket_path.to_string();
        let reuse_id = reuse_sandboxes.get(&cfg.name).map(|s| s.clone());
        thread::spawn(move || {
            let res = create_sandbox_plan(
                &context,
                &cfg,
                &project_id,
                &socket_path,
                reuse_id.as_deref(),
            );
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
    warm_by_sandbox: Option<Arc<Mutex<HashMap<String, SandboxWarmState>>>>,
) -> Result<Vec<SandboxResult>> {
    let (tx, rx) = mpsc::channel::<Result<SandboxResult>>();
    for plan in plans.iter().cloned() {
        let tx = tx.clone();
        let project_id = project_id.to_string();
        let socket_path = socket_path.to_string();
        let warm_t = warm_by_sandbox.clone();
        thread::spawn(move || {
            let res = (|| -> Result<SandboxResult> {
                if let Some(ref arc) = warm_t {
                    let mut guard = arc
                        .lock()
                        .map_err(|e| anyhow!("warm mutex poisoned: {}", e))?;
                    let ent = guard.entry(plan.sandbox_id.clone()).or_default();
                    run_sandbox(&plan, &project_id, &socket_path, Some(ent))
                } else {
                    run_sandbox(&plan, &project_id, &socket_path, None)
                }
            })();
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
fn cap_agents(blueprint: &mut ImplementationBlueprint, max_agents: usize) {
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
    context: &ProjectContext,
    blueprint: &ImplementationBlueprint,
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
        "generationConfig": vertex_generation_config(0.35)
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
