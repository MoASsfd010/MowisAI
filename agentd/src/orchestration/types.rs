use std::collections::HashMap;

use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    pub project_name: String,
    pub description: String,
    pub tech_stack: Vec<String>,
    pub existing_structure: String,
    pub key_files: Vec<String>,
    pub constraints: Vec<String>,
    pub task_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub name: String,
    pub os: String,
    pub packages: Vec<String>,
    pub tools: Vec<String>,
    pub agent_count: usize,
    pub deliverable: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationBlueprint {
    pub sandboxes: Vec<SandboxConfig>,
    pub execution_order: Vec<String>,
    pub merge_strategy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    pub agent_id: String,
    pub task: String,
    pub files: Vec<String>,
    pub tools: Vec<String>,
    pub context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxExecutionPlan {
    /// Blueprint sandbox / team name (used to reuse the same agentd sandbox across REPL turns).
    #[serde(default)]
    pub sandbox_team: String,
    pub sandbox_id: String,
    pub agents: Vec<AgentTask>,
    pub dependency_order: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub agent_id: String,
    pub success: bool,
    pub summary: String,
    pub diff: String,
    pub files_changed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxResult {
    pub sandbox_id: String,
    pub success: bool,
    pub agent_results: Vec<AgentResult>,
    pub merged_diff: String,
}

/// Persistent handles for interactive / long-lived runs: same `sandbox_id`, reuse containers when
/// `agent_id` matches a previous turn (merge container is always reused once created).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxWarmState {
    pub worker_containers: HashMap<String, String>,
    pub merge_container_id: Option<String>,
}
