use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use super::types::ProjectContext;
use super::{
    gcloud_access_token, invoke_tool_via_socket, parse_ok_field, socket_roundtrip, trace,
    vertex_generation_config, HTTP_TIMEOUT_SECS, MAX_CONTEXT_GATHER_ROUNDS,
};

pub fn gather_context(prompt: &str, project_id: &str, socket_path: &str) -> Result<ProjectContext> {
    #[cfg(not(unix))]
    {
        let _ = (prompt, project_id, socket_path);
        return Err(anyhow!(
            "context gatherer requires Unix (agentd uses Unix domain sockets)"
        ));
    }

    #[cfg(unix)]
    {
        gather_context_inner(prompt, project_id, socket_path)
    }
}

#[cfg(unix)]
fn gather_context_inner(prompt: &str, project_id: &str, socket_path: &str) -> Result<ProjectContext> {
    trace("layer1/context: creating analysis sandbox");
    let sandbox_id = create_sandbox(socket_path)?;
    trace(&format!("layer1/context: sandbox ready {}", sandbox_id));
    let container_id = create_container(socket_path, &sandbox_id)?;
    trace(&format!(
        "layer1/context: container ready {} in sandbox {}",
        container_id, sandbox_id
    ));

    let url = format!(
        "https://us-central1-aiplatform.googleapis.com/v1/projects/{}/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
        project_id
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let system_prompt = "You are the Context Gatherer.\n\
        Goal: understand the project deeply before planning or coding.\n\
        Use tools to inspect files and structure.\n\
        If context is missing, include explicit questions inside constraints.\n\
        Output final response strictly as JSON:\n\
        {\n\
          \"project_name\":\"...\",\n\
          \"description\":\"...\",\n\
          \"tech_stack\":[\"...\"],\n\
          \"existing_structure\":\"...\",\n\
          \"key_files\":[\"...\"],\n\
          \"constraints\":[\"...\"],\n\
          \"task_summary\":\"...\"\n\
        }\n\
        Do not output markdown.";

    let mut contents = vec![json!({
        "role": "user",
        "parts": [{ "text": prompt }]
    })];
    let tools = vec![
        tool_decl("list_files", "List files in a directory", json!({"path": {"type":"string","description":"Directory path"}}), vec!["path"]),
        tool_decl("read_file", "Read a file", json!({"path": {"type":"string","description":"File path"}}), vec!["path"]),
    ];

    for round in 0..MAX_CONTEXT_GATHER_ROUNDS {
        trace(&format!(
            "layer1/context: gemini round {}/{}",
            round + 1,
            MAX_CONTEXT_GATHER_ROUNDS
        ));
        let body = json!({
            "contents": contents,
            "tools": [{ "function_declarations": tools }],
            "systemInstruction": { "parts": [{ "text": system_prompt }] },
            "generationConfig": vertex_generation_config(0.2)
        });
        let start = std::time::Instant::now();
        let token = gcloud_access_token()?;
        let resp = client
            .post(&url)
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .context("context gatherer HTTP")?;
        trace(&format!(
            "layer1/context: gemini round {} status={} elapsed_ms={}",
            round + 1,
            resp.status(),
            start.elapsed().as_millis()
        ));

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(anyhow!("context gatherer error {}: {}", status, text));
        }
        let data: Value = resp.json().context("parse context gatherer JSON")?;
        let parts = data
            .pointer("/candidates/0/content/parts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut model_parts = Vec::new();
        let mut calls: Vec<(String, Value)> = Vec::new();
        let mut final_text = String::new();
        for p in &parts {
            if let Some(fc) = p.get("functionCall") {
                let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args = fc.get("args").cloned().unwrap_or(json!({}));
                calls.push((name, args));
                model_parts.push(p.clone());
            } else if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    final_text = t.to_string();
                }
                model_parts.push(p.clone());
            }
        }

        if calls.is_empty() {
            trace("layer1/context: no function calls, parsing final JSON context");
            return parse_context_json(&final_text);
        }

        contents.push(json!({ "role": "model", "parts": model_parts }));
        let mut response_parts = Vec::new();
        for (name, args) in calls {
            trace(&format!("layer1/context: tool_call name={}", name));
            let tool_result = invoke_tool_via_socket(socket_path, &sandbox_id, &container_id, &name, &args)?;
            response_parts.push(json!({
                "functionResponse": {
                    "name": name,
                    "response": tool_result
                }
            }));
        }
        contents.push(json!({ "role": "user", "parts": response_parts }));
    }

    Err(anyhow!("context gatherer exceeded tool-loop rounds ({})", MAX_CONTEXT_GATHER_ROUNDS))
}

fn create_sandbox(socket_path: &str) -> Result<String> {
    let resp = socket_roundtrip(socket_path, &json!({"request_type":"create_sandbox","image":"alpine"}))?;
    parse_ok_field(&resp, "sandbox")
}

fn create_container(socket_path: &str, sandbox_id: &str) -> Result<String> {
    let resp = socket_roundtrip(
        socket_path,
        &json!({"request_type":"create_container","sandbox":sandbox_id}),
    )?;
    parse_ok_field(&resp, "container")
}

fn tool_decl(name: &str, description: &str, properties: Value, required: Vec<&str>) -> Value {
    json!({
        "name": name,
        "description": description,
        "parameters": {
            "type": "object",
            "properties": properties,
            "required": required
        }
    })
}

fn parse_context_json(text: &str) -> Result<ProjectContext> {
    match serde_json::from_str::<ProjectContext>(text.trim()) {
        Ok(v) => Ok(v),
        Err(_) => {
            let trimmed = text.trim();
            let start = trimmed.find('{').ok_or_else(|| anyhow!("no JSON start"))?;
            let end = trimmed.rfind('}').ok_or_else(|| anyhow!("no JSON end"))?;
            serde_json::from_str::<ProjectContext>(&trimmed[start..=end]).context("parse project context")
        }
    }
}
