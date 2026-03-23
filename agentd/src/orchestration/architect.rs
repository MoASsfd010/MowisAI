use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use super::types::{ImplementationBlueprint, ProjectContext};
use super::{gcloud_access_token, trace, HTTP_TIMEOUT_SECS};

pub fn create_blueprint(context: &ProjectContext, project_id: &str) -> Result<ImplementationBlueprint> {
    #[cfg(not(unix))]
    {
        let _ = (context, project_id);
        return Err(anyhow!(
            "architect requires Unix (gcloud/deployment target)"
        ));
    }

    #[cfg(unix)]
    {
        create_blueprint_inner(context, project_id)
    }
}

#[cfg(unix)]
fn create_blueprint_inner(context: &ProjectContext, project_id: &str) -> Result<ImplementationBlueprint> {
    trace("layer2/architect: creating blueprint");
    let url = format!(
        "https://us-central1-aiplatform.googleapis.com/v1/projects/{}/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent",
        project_id
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("reqwest client")?;

    let prompt = format!(
        "You are the Architect. Produce implementation blueprint JSON only.\n\
         Output schema:\n\
         {{\"sandboxes\":[{{\"name\":\"...\",\"os\":\"alpine\",\"packages\":[],\"tools\":[],\"agent_count\":1,\"deliverable\":\"...\"}}],\"execution_order\":[],\"merge_strategy\":\"...\"}}\n\n\
         Rules:\n\
         - split domains realistically (frontend/backend/deployment/testing/etc.)\n\
         - pick sandbox package sets from context\n\
         - choose tool subsets per sandbox\n\
         - choose practical agent_count per sandbox\n\
         - execution_order can include parallel groups encoded as string tags (e.g. \"frontend|deployment\")\n\
         - include a clear merge_strategy\n\n\
         Project context:\n\
         project_name: {}\n\
         description: {}\n\
         tech_stack: {:?}\n\
         existing_structure: {}\n\
         key_files: {:?}\n\
         constraints: {:?}\n\
         task_summary: {}",
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
        "generationConfig": {
            "temperature": 0.25,
            "responseMimeType": "application/json"
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
        .context("architect generateContent HTTP")?;
    trace(&format!(
        "layer2/architect: response status={} elapsed_ms={}",
        resp.status(),
        start.elapsed().as_millis()
    ));

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        return Err(anyhow!("architect error {}: {}", status, text));
    }

    let data: Value = resp.json().context("parse architect JSON")?;
    trace("layer2/architect: parsing blueprint JSON");
    let text = extract_text(&data)?;
    parse_blueprint_json(&text)
}

fn extract_text(data: &Value) -> Result<String> {
    let parts = data
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("architect: no content parts"))?;
    let mut out = String::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
            out.push_str(t);
        }
    }
    if out.trim().is_empty() {
        return Err(anyhow!("architect: empty output"));
    }
    Ok(out)
}

fn parse_blueprint_json(text: &str) -> Result<ImplementationBlueprint> {
    match serde_json::from_str::<ImplementationBlueprint>(text.trim()) {
        Ok(v) => Ok(v),
        Err(_) => {
            let trimmed = text.trim();
            let start = trimmed.find('{').ok_or_else(|| anyhow!("no JSON start"))?;
            let end = trimmed.rfind('}').ok_or_else(|| anyhow!("no JSON end"))?;
            serde_json::from_str::<ImplementationBlueprint>(&trimmed[start..=end])
                .context("parse blueprint json")
        }
    }
}
