use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::types::{AgentResult, SandboxExecutionPlan, SandboxResult};
use super::{gcloud_access_token, parse_ok_field, socket_roundtrip, trace, HTTP_TIMEOUT_SECS};
use super::worker::run_worker;

const MERGE_MAX_RETRIES: usize = 2;

pub fn run_sandbox(
    plan: &SandboxExecutionPlan,
    project_id: &str,
    socket_path: &str,
) -> Result<SandboxResult> {
    #[cfg(not(unix))]
    {
        let _ = (plan, project_id, socket_path);
        return Err(anyhow!(
            "sandbox manager requires Unix (agentd uses Unix domain sockets)"
        ));
    }

    #[cfg(unix)]
    {
        run_sandbox_inner(plan, project_id, socket_path)
    }
}

#[cfg(unix)]
fn run_sandbox_inner(
    plan: &SandboxExecutionPlan,
    project_id: &str,
    socket_path: &str,
) -> Result<SandboxResult> {
    trace(&format!(
        "layer4/manager: start sandbox={} workers={}",
        plan.sandbox_id,
        plan.agents.len()
    ));

    let mut id_to_task = HashMap::new();
    for task in &plan.agents {
        id_to_task.insert(task.agent_id.clone(), task.clone());
    }

    // Pre-create worker containers.
    let mut worker_containers: HashMap<String, String> = HashMap::new();
    for task in &plan.agents {
        let cid = create_container(socket_path, &plan.sandbox_id)?;
        trace(&format!(
            "layer4/manager: worker container ready agent={} container={}",
            task.agent_id, cid
        ));
        worker_containers.insert(task.agent_id.clone(), cid);
    }

    // Dedicated merge container.
    let merge_container = create_container(socket_path, &plan.sandbox_id)?;
    trace(&format!(
        "layer4/manager: merge container ready sandbox={} container={}",
        plan.sandbox_id, merge_container
    ));
    init_merge_repo(socket_path, &plan.sandbox_id, &merge_container)?;

    let groups = if plan.dependency_order.is_empty() {
        vec![plan.agents.iter().map(|a| a.agent_id.clone()).collect()]
    } else {
        plan.dependency_order.clone()
    };

    let mut agent_results = Vec::new();
    for (group_idx, group) in groups.iter().enumerate() {
        trace(&format!(
            "layer4/manager: running dependency group {}/{} ({} workers)",
            group_idx + 1,
            groups.len(),
            group.len()
        ));
        let (tx, rx) = mpsc::channel::<Result<AgentResult>>();

        for aid in group {
            let task = id_to_task
                .get(aid)
                .cloned()
                .ok_or_else(|| anyhow!("unknown agent id in dependency_order: {}", aid))?;
            let container_id = worker_containers
                .get(aid)
                .cloned()
                .ok_or_else(|| anyhow!("missing worker container for {}", aid))?;
            let sid = plan.sandbox_id.clone();
            let pid = project_id.to_string();
            let sock = socket_path.to_string();
            let tx = tx.clone();
            thread::spawn(move || {
                let res = run_worker(&task, &sid, &container_id, &pid, &sock);
                let _ = tx.send(res);
            });
        }
        drop(tx);

        for recv in rx {
            let result = recv?;
            trace(&format!(
                "layer4/manager: worker completed agent={} success={}",
                result.agent_id, result.success
            ));
            if !result.diff.trim().is_empty() {
                merge_worker_diff(
                    socket_path,
                    project_id,
                    &plan.sandbox_id,
                    &merge_container,
                    &result,
                )?;
            }
            agent_results.push(result);
        }
    }

    let merged_diff = read_merged_diff(socket_path, &plan.sandbox_id, &merge_container)?;
    let success = agent_results.iter().all(|r| r.success);
    trace(&format!(
        "layer4/manager: complete sandbox={} success={} merged_diff_bytes={}",
        plan.sandbox_id,
        success,
        merged_diff.len()
    ));

    Ok(SandboxResult {
        sandbox_id: plan.sandbox_id.clone(),
        success,
        agent_results,
        merged_diff,
    })
}

fn create_container(socket_path: &str, sandbox_id: &str) -> Result<String> {
    let req = json!({
        "request_type": "create_container",
        "sandbox": sandbox_id
    });
    let resp = socket_roundtrip(socket_path, &req)?;
    parse_ok_field(&resp, "container")
}

fn init_merge_repo(socket_path: &str, sandbox_id: &str, merge_container: &str) -> Result<()> {
    run_cmd(
        socket_path,
        sandbox_id,
        merge_container,
        "mkdir -p /workspace && cd /workspace && git init && git checkout -b main",
    )?;
    Ok(())
}

fn merge_worker_diff(
    socket_path: &str,
    project_id: &str,
    sandbox_id: &str,
    merge_container: &str,
    result: &AgentResult,
) -> Result<()> {
    trace(&format!(
        "layer4/manager: merging patch agent={} sandbox={}",
        result.agent_id, sandbox_id
    ));
    let patch_path = format!("/workspace/.patches/{}.diff", result.agent_id);
    invoke_tool(
        socket_path,
        sandbox_id,
        merge_container,
        "create_directory",
        json!({ "path": "/workspace/.patches" }),
    )?;
    invoke_tool(
        socket_path,
        sandbox_id,
        merge_container,
        "write_file",
        json!({ "path": patch_path, "content": result.diff }),
    )?;

    let apply_cmd = format!(
        "cd /workspace && git apply .patches/{}.diff",
        result.agent_id
    );
    let apply = run_cmd(socket_path, sandbox_id, merge_container, &apply_cmd);
    if apply.is_ok() {
        let commit_cmd = format!(
            "cd /workspace && git add . && git commit -m \"merge {}\" || true",
            result.agent_id
        );
        let _ = run_cmd(socket_path, sandbox_id, merge_container, &commit_cmd);
        return Ok(());
    }
    trace(&format!(
        "layer4/manager: merge conflict detected agent={} sandbox={}",
        result.agent_id, sandbox_id
    ));

    for attempt in 0..MERGE_MAX_RETRIES {
        let conflict_text = run_cmd(
            socket_path,
            sandbox_id,
            merge_container,
            "cd /workspace && git status --porcelain && git diff",
        )
        .unwrap_or_else(|_| String::new());
        let repaired = resolve_conflict_patch(project_id, result, &conflict_text)?;
        invoke_tool(
            socket_path,
            sandbox_id,
            merge_container,
            "write_file",
            json!({ "path": format!("/workspace/.patches/{}.repaired.diff", result.agent_id), "content": repaired }),
        )?;
        let retry_cmd = format!(
            "cd /workspace && git reset --hard HEAD && git apply .patches/{}.repaired.diff",
            result.agent_id
        );
        if run_cmd(socket_path, sandbox_id, merge_container, &retry_cmd).is_ok() {
            let commit_cmd = format!(
                "cd /workspace && git add . && git commit -m \"merge {} repaired\" || true",
                result.agent_id
            );
            let _ = run_cmd(socket_path, sandbox_id, merge_container, &commit_cmd);
            return Ok(());
        }
        trace(&format!(
            "layer4/manager: merge retry {} failed for agent={}",
            attempt + 1,
            result.agent_id
        ));
    }

    Err(anyhow!("failed to merge patch for {}", result.agent_id))
}

fn resolve_conflict_patch(
    project_id: &str,
    result: &AgentResult,
    conflict_text: &str,
) -> Result<String> {
    let url = format!(
        "https://us-central1-aiplatform.googleapis.com/v1/projects/{}/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
        project_id
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let prompt = format!(
        "Resolve this git patch conflict and output ONLY the repaired unified diff patch text.\n\n\
         Worker summary:\n{}\n\nOriginal diff:\n{}\n\nConflict context:\n{}",
        result.summary, result.diff, conflict_text
    );
    let body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": prompt }] }],
        "generationConfig": { "temperature": 0.2 }
    });

    let token = gcloud_access_token()?;
    let start = std::time::Instant::now();
    let resp = client
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .context("conflict resolution HTTP")?;
    trace(&format!(
        "layer4/manager: conflict resolver status={} elapsed_ms={}",
        resp.status(),
        start.elapsed().as_millis()
    ));
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        return Err(anyhow!("conflict resolver error {}: {}", status, text));
    }
    let data: Value = resp.json().context("parse conflict JSON")?;
    let text = data
        .pointer("/candidates/0/content/parts/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(anyhow!("conflict resolver returned empty diff"));
    }
    Ok(text)
}

fn read_merged_diff(socket_path: &str, sandbox_id: &str, merge_container: &str) -> Result<String> {
    run_cmd(socket_path, sandbox_id, merge_container, "cd /workspace && git diff")
}

fn run_cmd(
    socket_path: &str,
    sandbox_id: &str,
    container_id: &str,
    cmd: &str,
) -> Result<String> {
    let out = invoke_tool(
        socket_path,
        sandbox_id,
        container_id,
        "run_command",
        json!({ "cmd": cmd }),
    )?;
    let success = out.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
    let stdout = out
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let stderr = out.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    if success {
        Ok(stdout)
    } else {
        Err(anyhow!("run_command failed: {}", stderr))
    }
}

fn invoke_tool(
    socket_path: &str,
    sandbox_id: &str,
    container_id: &str,
    tool_name: &str,
    input: Value,
) -> Result<Value> {
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
        Err(anyhow!("invoke_tool {} failed: {}", tool_name, err))
    }
}
