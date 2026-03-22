//! Dependency-aware execution: sandboxes, parallel agents per stage.

use anyhow::{anyhow, Result};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::thread;

use super::agent_runner::{run_agent, AgentOutput};
use super::planner::{Plan, PlanTask};
use super::sandbox_profiles::merge_packages;
use super::{parse_ok_field, socket_roundtrip};

/// Run all tasks in `plan`; returns map task_id → output.
pub fn execute(
    plan: &Plan,
    socket_path: &str,
    project_id: &str,
    max_agents: usize,
) -> Result<HashMap<String, AgentOutput>> {
    #[cfg(not(unix))]
    {
        let _ = (plan, socket_path, project_id, max_agents);
        return Err(anyhow!(
            "executor requires Unix (agentd uses Unix domain sockets)"
        ));
    }

    #[cfg(unix)]
    {
        execute_inner(plan, socket_path, project_id, max_agents)
    }
}

#[cfg(unix)]
fn execute_inner(
    plan: &Plan,
    socket_path: &str,
    project_id: &str,
    max_agents: usize,
) -> Result<HashMap<String, AgentOutput>> {
    if plan.tasks.is_empty() {
        println!("[orchestrator] Plan has no tasks.");
        return Ok(HashMap::new());
    }

    let max_parallel = max_agents.max(1);
    let stages = compute_stages(&plan.tasks)?;
    println!(
        "[orchestrator] Plan: {} tasks in {} stage(s)",
        plan.tasks.len(),
        stages.len()
    );

    let task_map: HashMap<String, PlanTask> = plan
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.clone()))
        .collect();

    let mut results: HashMap<String, AgentOutput> = HashMap::new();
    /// team_type -> sandbox id string
    let mut sandboxes: HashMap<String, String> = HashMap::new();

    for (si, stage_ids) in stages.iter().enumerate() {
        println!(
            "[orchestrator] Stage {}: {} task(s)",
            si + 1,
            stage_ids.len()
        );

        // Ensure sandboxes for team types appearing in this stage
        let mut teams_this_stage: HashSet<String> = HashSet::new();
        for tid in stage_ids {
            if let Some(t) = task_map.get(tid) {
                teams_this_stage.insert(normalize_team(&t.team_type));
            }
        }

        for team in &teams_this_stage {
            if sandboxes.contains_key(team) {
                continue;
            }
            let packages = merged_packages_for_plan_team(plan, team);

            println!(
                "[sandbox] Creating {} sandbox (packages: {:?})…",
                team, packages
            );
            let req = json!({
                "request_type": "create_sandbox",
                "image": "alpine",
                "packages": packages
            });
            let resp = socket_roundtrip(socket_path, &req)?;
            let sid = parse_ok_field(&resp, "sandbox")?;
            println!("[sandbox] {} ready (id {})", team, sid);
            sandboxes.insert(team.clone(), sid);
        }

        // Run stage tasks with bounded parallelism
        let mut idx = 0;
        while idx < stage_ids.len() {
            let end = (idx + max_parallel).min(stage_ids.len());
            let batch: Vec<String> = stage_ids[idx..end].to_vec();
            idx = end;

            let (tx, rx) = mpsc::channel::<Result<(String, AgentOutput)>>();

            for tid in batch {
                let task = task_map
                    .get(&tid)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing task {}", tid))?;
                let sandbox_id = sandboxes
                    .get(&normalize_team(&task.team_type))
                    .cloned()
                    .ok_or_else(|| anyhow!("no sandbox for team {}", task.team_type))?;
                let socket_path = socket_path.to_string();
                let project_id = project_id.to_string();
                let deps_context = build_dependency_context(&task, &results);
                let log_prefix = format!("[agent:{}]", sanitize_label(&task.id));
                let tx = tx.clone();

                thread::spawn(move || {
                    let r = (|| -> Result<(String, AgentOutput)> {
                        println!("{} Starting: {}", log_prefix, task.agent_instruction.chars().take(120).collect::<String>());
                        let create_ct = json!({
                            "request_type": "create_container",
                            "sandbox": &sandbox_id
                        });
                        let ct_resp = socket_roundtrip(&socket_path, &create_ct)?;
                        let container_id = parse_ok_field(&ct_resp, "container")?;
                        let role = format!("{} / {}", task.team_type, task.id);
                        let out = run_agent(
                            &socket_path,
                            &project_id,
                            &sandbox_id,
                            &container_id,
                            &task.id,
                            &role,
                            &task.agent_instruction,
                            &deps_context,
                            &log_prefix,
                        )?;
                        println!(
                            "{} Complete (success={})",
                            log_prefix, out.success
                        );
                        Ok((task.id.clone(), out))
                    })();
                    let _ = tx.send(r);
                });
            }
            drop(tx);

            for recv in rx {
                let (id, out) = recv?;
                results.insert(id, out);
            }
        }
    }

    println!("[orchestrator] All tasks complete.");
    Ok(results)
}

/// Union of profile packages and every `required_packages` for tasks of this team (whole plan).
fn merged_packages_for_plan_team(plan: &Plan, team_norm: &str) -> Vec<String> {
    let tasks: Vec<&PlanTask> = plan
        .tasks
        .iter()
        .filter(|t| normalize_team(&t.team_type) == team_norm)
        .collect();
    if tasks.is_empty() {
        return merge_packages("general", &[]);
    }
    let profile_key = tasks[0].team_type.as_str();
    let mut seen = HashSet::new();
    let mut extras: Vec<String> = Vec::new();
    for t in &tasks {
        for p in &t.required_packages {
            let p = p.trim();
            if !p.is_empty() && seen.insert(p.to_string()) {
                extras.push(p.to_string());
            }
        }
    }
    merge_packages(profile_key, &extras)
}

fn normalize_team(team_type: &str) -> String {
    team_type.to_lowercase()
}

fn sanitize_label(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn build_dependency_context(task: &PlanTask, results: &HashMap<String, AgentOutput>) -> String {
    if task.dependencies.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for dep in &task.dependencies {
        if let Some(o) = results.get(dep) {
            lines.push(format!(
                "### Task {} (success={})\n{}\nFiles: {:?}\n",
                dep, o.success, o.output, o.files_created
            ));
        } else {
            lines.push(format!("### Task {} (no output yet — error)\n", dep));
        }
    }
    lines.join("\n")
}

/// Topological stages: each stage is task ids runnable in parallel.
fn compute_stages(tasks: &[PlanTask]) -> Result<Vec<Vec<String>>> {
    let mut remaining: HashSet<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let task_map: HashMap<String, &PlanTask> = tasks.iter().map(|t| (t.id.clone(), t)).collect();

    let mut stages: Vec<Vec<String>> = Vec::new();
    while !remaining.is_empty() {
        let mut ready: Vec<String> = remaining
            .iter()
            .filter(|id| {
                let t = task_map.get(*id).expect("task");
                t.dependencies
                    .iter()
                    .all(|d| !remaining.contains(d))
            })
            .cloned()
            .collect();

        if ready.is_empty() {
            return Err(anyhow!(
                "dependency cycle or missing dependency among tasks {:?}",
                remaining
            ));
        }

        ready.sort();
        for id in &ready {
            remaining.remove(id);
        }
        stages.push(ready);
    }
    Ok(stages)
}
