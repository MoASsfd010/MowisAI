use anyhow::{Context, Result};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::audit::{AuditEvent, EventType};
use crate::buckets::BucketStore;
use crate::memory::AgentMemory;
use crate::security::SecurityPolicy;
use crate::tool_registry;
use crate::{ResourceLimits, Sandbox};

lazy_static! {
    // shared state across threads; wrap map in Arc for cheap cloning
    static ref SANDBOXES: Arc<Mutex<HashMap<u64, Sandbox>>> = Arc::new(Mutex::new(HashMap::new()));
    static ref AUDITOR: crate::audit::SecurityAuditor = {
        let audit_path = if cfg!(test) {
            "/tmp/agentd-test/log/audit.log"
        } else {
            "/var/log/agentd/audit.log"
        };
        let _ = std::fs::create_dir_all(std::path::Path::new(audit_path).parent().unwrap());
        crate::audit::SecurityAuditor::new(std::path::Path::new(audit_path))
            .expect("failed to initialize audit logger")
    };
    static ref PERSISTENCE: crate::persistence::PersistenceManager = {
        let persist_path = if cfg!(test) {
            "/tmp/agentd-test/lib"
        } else {
            "/var/lib/agentd"
        };
        let _ = std::fs::create_dir_all(persist_path);
        crate::persistence::PersistenceManager::new(std::path::Path::new(persist_path))
    };
    static ref MEMORY_STORE: Mutex<HashMap<u64, AgentMemory>> = Mutex::new(HashMap::new());
    static ref COORDINATOR: Mutex<crate::agent_loop::AgentCoordinator> =
        Mutex::new(crate::agent_loop::AgentCoordinator::new());
}

// ── Wire types ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct SocketRequest {
    pub request_type: String,
    /// Sandbox ID – accepted as either a string ("123") or bare number (123)
    pub sandbox: Option<Value>,
    pub ram: Option<u64>,
    pub cpu: Option<u64>,
    /// OS image to use, e.g. "alpine" (required for create_sandbox)
    pub image: Option<String>,
    /// Container ID – same flexible parsing as sandbox
    pub container: Option<Value>,
    /// Extra packages to install on top of core packages
    pub packages: Option<Vec<String>>,
    /// Optional Git repository URL to seed into sandbox baseline (/workspace)
    pub seed_repo_url: Option<String>,
    /// Optional branch or ref for repo seeding
    pub seed_repo_branch: Option<String>,
    /// Optional subdirectory inside /workspace where repo should be cloned
    pub seed_repo_subdir: Option<String>,
    pub name: Option<String>,
    pub input: Option<Value>,
    pub command: Option<String>,
    pub to: Option<u64>,
    pub channel: Option<u64>,
    pub agent: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct SocketResponse {
    pub status: String,
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl SocketResponse {
    fn ok(result: Option<Value>) -> Self {
        SocketResponse { status: "ok".into(), result, error: None }
    }
    fn err<E: ToString>(e: E) -> Self {
        SocketResponse { status: "error".into(), result: None, error: Some(e.to_string()) }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Accept sandbox/container IDs as either a JSON string or a JSON number.
fn parse_id(val: &Value) -> Option<u64> {
    match val {
        Value::String(s) => s.parse::<u64>().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Run a single command inside the OS image via chroot and stream its output
/// to stdout in real time so the caller can see progress (e.g. apk install).
/// Returns an error if the command exits non-zero.
fn chroot_run_streaming(root: &std::path::Path, cmd: &str) -> Result<()> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    // Copy DNS config into the image so network calls work
    let etc = root.join("etc");
    std::fs::create_dir_all(&etc).ok();
    let _ = std::fs::copy("/etc/resolv.conf", etc.join("resolv.conf"));
    let _ = std::fs::copy("/etc/hosts", etc.join("hosts"));

    let mut child = Command::new("chroot")
        .arg(root)
        .arg("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("chroot spawn failed")?;

    // Stream stdout
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().flatten() {
            println!("  [sandbox] {}", line);
        }
    }
    // Stream stderr
    if let Some(stderr) = child.stderr.take() {
        for line in BufReader::new(stderr).lines().flatten() {
            eprintln!("  [sandbox] {}", line);
        }
    }

    let status = child.wait().context("chroot wait failed")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("command exited with status: {}", status))
    }
}

/// Install packages inside the OS image. Detects the package manager from the
/// image name. Always installs the core set first, then any extras requested.
/// Live output is streamed to stdout so the user can see progress.
fn install_packages_in_image(
    root: &std::path::Path,
    image_hint: &str,
    extra_packages: &[String],
) -> Result<()> {
    // Core packages every container needs – git, shell utilities, runtimes
    let core = [
        "git", "curl", "wget", "bash", "python3", "py3-pip",
        "nodejs", "npm", "ca-certificates", "openssh-client",
    ];

    let is_alpine = image_hint.contains("alpine") || image_hint.is_empty();
    let is_debian = image_hint.contains("ubuntu") || image_hint.contains("debian");

    let all_packages: Vec<&str> = {
        let mut v: Vec<&str> = core.iter().copied().collect();
        v.extend(extra_packages.iter().map(|s| s.as_str()));
        v
    };

    println!("  [sandbox] Installing packages: {}", all_packages.join(" "));

    let install_cmd = if is_alpine {
        // For Alpine, set up repositories to use HTTP instead of HTTPS to avoid TLS bootstrap issues
        // Then run apk add
        format!(
            "echo 'http://dl-cdn.alpinelinux.org/alpine/v3.23/main' > /etc/apk/repositories && \
             echo 'http://dl-cdn.alpinelinux.org/alpine/v3.23/community' >> /etc/apk/repositories && \
             apk add --no-cache {}", 
            all_packages.join(" ")
        )
    } else if is_debian {
        format!(
            "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends {}",
            all_packages.join(" ")
        )
    } else {
        // Unknown image – try apk first (with HTTP repos to avoid TLS bootstrap), fall back to apt
        format!(
            "echo 'http://dl-cdn.alpinelinux.org/alpine/v3.23/main' > /etc/apk/repositories && \
             echo 'http://dl-cdn.alpinelinux.org/alpine/v3.23/community' >> /etc/apk/repositories && \
             apk add --no-cache {pkgs} 2>/dev/null || (apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y {pkgs})",
            pkgs = all_packages.join(" ")
        )
    };

    // Copy CA certificates into chroot so apk can verify TLS
    // Try multiple common locations to find system CA certificates
    let ca_src_paths = vec![
        "/etc/ssl/certs/ca-certificates.crt",      // Debian/Ubuntu bundle
        "/etc/pki/tls/certs/ca-bundle.crt",        // RedHat/CentOS bundle
        "/etc/ssl/certs/ca-bundle.crt",            // Alpine alternative
    ];
    
    let ca_dest = root.join("etc/ssl/certs");
    std::fs::create_dir_all(&ca_dest).ok();
    
    // Try to copy the first available CA bundle
    for src in &ca_src_paths {
        if std::path::Path::new(src).exists() {
            let dest = ca_dest.join("ca-certificates.crt");
            let _ = std::fs::copy(src, &dest);
            if dest.exists() {
                break;  // Successfully copied, stop trying other paths
            }
        }
    }
    
    // Also copy individual cert files if ca-certificates.crt exists
    if std::path::Path::new("/etc/ssl/certs").exists() {
        if let Ok(entries) = std::fs::read_dir("/etc/ssl/certs") {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_file() {
                        let filename = entry.file_name();
                        if let Some(name) = filename.to_str() {
                            // Copy .pem and .crt files
                            if name.ends_with(".pem") || name.ends_with(".crt") {
                                let _ = std::fs::copy(entry.path(), ca_dest.join(name));
                            }
                        }
                    }
                }
            }
        }
    }

    chroot_run_streaming(root, &install_cmd)
        .context("package installation failed")?;

    println!("  [sandbox] Packages installed.");
    Ok(())
}

// ── Request handler ───────────────────────────────────────────────────────────

fn handle_request(req: SocketRequest) -> SocketResponse {
    // Validate request type and required fields per request type
    match req.request_type.as_str() {
        "create_sandbox" => {
            // Optional: image, ram, cpu, packages
        }
        "create_container" => {
            if req.sandbox.is_none() {
                return SocketResponse::err("create_container: missing sandbox id");
            }
        }
        "invoke_tool" => {
            if req.sandbox.is_none() {
                return SocketResponse::err("invoke_tool: missing sandbox id");
            }
            if req.container.is_none() {
                return SocketResponse::err("invoke_tool: missing container id");
            }
            if req.name.is_none() || req.name.as_ref().map(|n| n.is_empty()).unwrap_or(true) {
                return SocketResponse::err("invoke_tool: missing tool name");
            }
        }
        "destroy_sandbox" => {
            if req.sandbox.is_none() {
                return SocketResponse::err("destroy_sandbox: missing sandbox id");
            }
        }
        "list" => {
            // No required fields
        }
        "set_policy" => {
            if req.sandbox.is_none() {
                return SocketResponse::err("set_policy: missing sandbox id");
            }
            if req.input.is_none() {
                return SocketResponse::err("set_policy: missing policy input");
            }
        }
        "list_containers" => {
            if req.sandbox.is_none() {
                return SocketResponse::err("list_containers: missing sandbox id");
            }
        }
        "create_channel" | "send_message" | "read_messages" => {
            if req.sandbox.is_none() {
                return SocketResponse::err(&format!("{}: missing sandbox id", req.request_type));
            }
        }
        _ => {
            return SocketResponse::err(&format!("unknown request type: {}", req.request_type));
        }
    }

    match req.request_type.as_str() {

        // ── create_sandbox ──────────────────────────────────────────────────
        // 1. Create sandbox with the given OS image (required).
        // 2. Install core packages + any extras via chroot into that image.
        //    Live output streams to the server's stdout so callers see progress.
        // 3. Every container created from this sandbox will inherit those packages
        //    via overlayfs (lower layer = sandbox upper, which has the packages).
        "create_sandbox" => {
            let image = req.image.clone().unwrap_or_else(|| "alpine".to_string());
            let limits = ResourceLimits { ram_bytes: req.ram, cpu_millis: req.cpu };
            let seed_repo_url = req.seed_repo_url.clone();
            let seed_repo_branch = req.seed_repo_branch.clone();
            let seed_repo_subdir = req.seed_repo_subdir.clone();

            let mut sb = match Sandbox::new_with_image(limits, Some(&image)) {
                Ok(s) => s,
                Err(e) => {
                    let _ = AUDITOR.record_event(
                        AuditEvent::new(EventType::SandboxCreated, 0, "sandbox creation failed")
                            .with_result(format!("failed: {}", e)),
                    );
                    return SocketResponse::err(format!("create_sandbox failed: {}", e));
                }
            };

            let id = sb.id();
            let root = sb.root_path().to_owned();
            let extra = req.packages.as_deref().unwrap_or(&[]);

            println!("[agentd] Setting up sandbox {} with image '{}'", id, image);

            if let Err(e) = install_packages_in_image(&root, &image, extra) {
                log::warn!("sandbox {} package install warning: {}", id, e);
                // Non-fatal: continue even if some optional packages failed.
                // Core failures will be caught when the first tool runs.
            }

            // Optional: seed a repository into sandbox baseline so all containers share it.
            if let Some(repo_url) = seed_repo_url.as_ref() {
                println!(
                    "[agentd] Seeding repo {} into sandbox {} ...",
                    repo_url, id
                );
                if let Err(e) = sb.seed_git_repo(
                    repo_url,
                    seed_repo_branch.as_deref(),
                    seed_repo_subdir.as_deref(),
                ) {
                    log::warn!("sandbox {} repo seed warning: {}", id, e);
                }
            }

            // Register all tools into the sandbox
            for tool in tool_registry::create_all_tools() {
                sb.register_tool(tool);
            }

            let mut store = SANDBOXES.lock().unwrap();
            store.insert(id, sb);

            let _ = AUDITOR.record_event(
                AuditEvent::new(EventType::SandboxCreated, 0, "sandbox created")
                    .with_target(id)
                    .with_result("success"),
            );

            println!("[agentd] Sandbox {} ready.", id);
            SocketResponse::ok(Some(json!({ "sandbox": id.to_string() })))
        }

        // ── create_container ────────────────────────────────────────────────
        // Creates an overlayfs container on top of the sandbox image layer.
        // The sandbox upper (with installed packages) is the lower layer, so
        // every container automatically has all core packages available.
        "create_container" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };

            let mut store = SANDBOXES.lock().unwrap();
            let sb = match store.get_mut(&sandbox_id) {
                Some(s) => s,
                None => return SocketResponse::err(format!("sandbox {} not found", sandbox_id)),
            };

            match sb.create_container() {
                Ok(container_id) => {
                    SocketResponse::ok(Some(json!({ "container": container_id.to_string() })))
                }
                Err(e) => SocketResponse::err(format!("create_container failed: {}", e)),
            }
        }

        // ── invoke_tool ─────────────────────────────────────────────────────
        // All tool execution happens inside the container via chroot.
        // Nothing runs on the host OS.
        // IMPORTANT: Lock is released before tool execution to allow other operations.
        "invoke_tool" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let container_id = match req.container.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing container id — create one first with create_container"),
            };
            let name = match req.name {
                Some(ref n) if !n.is_empty() => n.clone(),
                _ => return SocketResponse::err("missing tool name"),
            };
            let input = req.input.clone().unwrap_or(json!({}));

            // CRITICAL FIX: Prepare tool while holding lock, then drop lock before execution.
            // This prevents long-running tools from blocking all other operations.
            let prep_result = {
                let store = SANDBOXES.lock().unwrap();
                let sb = match store.get(&sandbox_id) {
                    Some(s) => s,
                    None => return SocketResponse::err(format!("sandbox {} not found", sandbox_id)),
                };
                sb.prepare_tool_invocation(container_id, &name, &input)
            }; // lock is released here

            match prep_result {
                Ok(prep) => {
                    // Execute tool WITHOUT holding SANDBOXES lock.
                    // Other requests can proceed in parallel.
                    let result = crate::sandbox::execute_tool_unlocked(prep, input);

                    // Re-acquire lock only for audit logging (should be fast).
                    if result.is_ok() {
                        let _ = AUDITOR.record_event(
                            AuditEvent::new(EventType::ToolInvoked, sandbox_id, "tool invoked")
                                .with_details(json!({ "tool": name })),
                        );
                    }

                    match result {
                        Ok(val) => SocketResponse::ok(Some(val)),
                        Err(e) => {
                            let err_str = e.to_string();
                            let event_type = if err_str.contains("security policy denied") {
                                EventType::SecurityViolation
                            } else {
                                EventType::ToolFailed
                            };
                            let _ = AUDITOR.record_event(
                                AuditEvent::new(event_type, sandbox_id, "tool error")
                                    .with_details(json!({ "tool": name, "error": err_str })),
                            );
                            SocketResponse::err(e)
                        }
                    }
                }
                Err(e) => {
                    let err_str = e.to_string();
                    let event_type = if err_str.contains("security policy denied") {
                        EventType::SecurityViolation
                    } else {
                        EventType::ToolFailed
                    };
                    let _ = AUDITOR.record_event(
                        AuditEvent::new(event_type, sandbox_id, "tool prep failed")
                            .with_details(json!({ "tool": name, "error": err_str })),
                    );
                    SocketResponse::err(e)
                }
            }
        }

        // ── list ────────────────────────────────────────────────────────────
        "list" => {
            let store = SANDBOXES.lock().unwrap();
            let ids: Vec<String> = store.keys().map(|id| id.to_string()).collect();
            SocketResponse::ok(Some(json!(ids)))
        }

        // ── list_containers ────────────────────────────────────────────────
        "list_containers" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let store = SANDBOXES.lock().unwrap();
            match store.get(&sandbox_id) {
                Some(sb) => {
                    let cids: Vec<String> = sb.list_containers()
                        .iter()
                        .map(|id| id.to_string())
                        .collect();
                    SocketResponse::ok(Some(json!(cids)))
                }
                None => SocketResponse::err(format!("sandbox {} not found", sandbox_id)),
            }
        }

        // ── destroy_sandbox ─────────────────────────────────────────────────
        "destroy_sandbox" => {
            let id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let mut store = SANDBOXES.lock().unwrap();
            if store.remove(&id).is_some() {
                let _ = AUDITOR.record_event(
                    AuditEvent::new(EventType::SandboxDestroyed, 0, "sandbox destroyed")
                        .with_target(id),
                );
                SocketResponse::ok(None)
            } else {
                SocketResponse::err(format!("sandbox {} not found", id))
            }
        }

        // ── register_tool ───────────────────────────────────────────────────
        "register_tool" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let name = req.name.clone().unwrap_or_default();
            let mut store = SANDBOXES.lock().unwrap();
            let sb = match store.get_mut(&sandbox_id) {
                Some(s) => s,
                None => return SocketResponse::err(format!("sandbox {} not found", sandbox_id)),
            };
            match tool_registry::get_tool(&name) {
                Some(tool) => {
                    sb.register_tool(tool);
                    SocketResponse::ok(None)
                }
                None => SocketResponse::err(format!("unknown tool: {}", name)),
            }
        }

        // ── set_policy / get_policy ─────────────────────────────────────────
        "set_policy" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let name = req.name.clone().unwrap_or_default();
            let policy = match name.as_str() {
                "restrictive" => SecurityPolicy::default_restrictive(),
                "permissive"  => SecurityPolicy::default_permissive(),
                other => return SocketResponse::err(format!("unknown policy '{}' — use 'restrictive' or 'permissive'", other)),
            };
            let mut store = SANDBOXES.lock().unwrap();
            match store.get_mut(&sandbox_id) {
                Some(sb) => {
                    sb.set_policy(policy);
                    let _ = AUDITOR.record_event(
                        AuditEvent::new(EventType::Custom("PolicySet".into()), 0, "policy set")
                            .with_target(sandbox_id)
                            .with_details(json!({ "policy": name })),
                    );
                    SocketResponse::ok(None)
                }
                None => SocketResponse::err(format!("sandbox {} not found", sandbox_id)),
            }
        }

        "get_policy" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let store = SANDBOXES.lock().unwrap();
            match store.get(&sandbox_id) {
                Some(sb) => match sb.policy() {
                    Some(policy) => match serde_json::to_value(policy) {
                        Ok(val) => SocketResponse::ok(Some(val)),
                        Err(e) => SocketResponse::err(format!("serialize policy: {}", e)),
                    },
                    None => SocketResponse::err("no policy set for sandbox"),
                },
                None => SocketResponse::err(format!("sandbox {} not found", sandbox_id)),
            }
        }

        // ── audit ────────────────────────────────────────────────────────────
        "get_audit_stats" => SocketResponse::ok(Some(AUDITOR.get_stats())),
        "get_anomalies"   => SocketResponse::ok(Some(AUDITOR.detect_anomalies())),

        // ── channels ─────────────────────────────────────────────────────────
        "create_channel" => {
            let from = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let to = match req.to {
                Some(id) => id,
                None => return SocketResponse::err("missing 'to' sandbox id"),
            };
            let id = crate::channels::create_channel(from, to);
            SocketResponse::ok(Some(json!({ "channel": id })))
        }

        "send_message" => {
            let from = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let channel_id = match req.channel {
                Some(id) => id,
                None => return SocketResponse::err("missing channel id"),
            };
            let payload = req.command.clone().unwrap_or_default();
            match crate::channels::send_message(
                channel_id,
                crate::channels::Message { from, to: 0, payload },
            ) {
                Ok(_)  => SocketResponse::ok(None),
                Err(e) => SocketResponse::err(e),
            }
        }

        "read_messages" => {
            let channel_id = match req.channel {
                Some(id) => id,
                None => return SocketResponse::err("missing channel id"),
            };
            match crate::channels::read_messages(channel_id) {
                Ok(msgs) => {
                    let out: Vec<_> = msgs
                        .iter()
                        .map(|m| json!({ "from": m.from, "to": m.to, "payload": m.payload }))
                        .collect();
                    SocketResponse::ok(Some(json!(out)))
                }
                Err(e) => SocketResponse::err(e),
            }
        }

        // ── bucket store ─────────────────────────────────────────────────────
        "bucket_put" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let key   = req.name.clone().unwrap_or_default();
            let value = req.command.clone().unwrap_or_default();

            let bucket_path = {
                let store = SANDBOXES.lock().unwrap();
                match store.get(&sandbox_id) {
                    Some(sb) => sb.root_path().join("buckets"),
                    None => return SocketResponse::err(format!("sandbox {} not found", sandbox_id)),
                }
            };
            match BucketStore::new(bucket_path) {
                Ok(mut bs) => match bs.put(&key, &value) {
                    Ok(())  => SocketResponse::ok(None),
                    Err(e)  => SocketResponse::err(e),
                },
                Err(e) => SocketResponse::err(e),
            }
        }

        "bucket_get" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let key = req.name.clone().unwrap_or_default();

            let bucket_path = {
                let store = SANDBOXES.lock().unwrap();
                match store.get(&sandbox_id) {
                    Some(sb) => sb.root_path().join("buckets"),
                    None => return SocketResponse::err(format!("sandbox {} not found", sandbox_id)),
                }
            };
            match BucketStore::new(bucket_path) {
                Ok(bs) => match bs.get(&key) {
                    Ok(Some(v)) => SocketResponse::ok(Some(json!({ "value": v }))),
                    Ok(None)    => SocketResponse::err("key not found"),
                    Err(e)      => SocketResponse::err(e),
                },
                Err(e) => SocketResponse::err(e),
            }
        }

        // ── memory ────────────────────────────────────────────────────────────
        "memory_set" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let key   = req.name.clone().unwrap_or_default();
            let value = req.input.clone().unwrap_or(json!(null));
            let mut mem = MEMORY_STORE.lock().unwrap();
            mem.entry(sandbox_id)
               .or_insert_with(|| AgentMemory::new(sandbox_id, sandbox_id))
               .short_term
               .set_context(key, value);
            SocketResponse::ok(None)
        }

        "memory_get" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let key = req.name.clone().unwrap_or_default();
            let mem = MEMORY_STORE.lock().unwrap();
            let val = mem
                .get(&sandbox_id)
                .and_then(|m| m.short_term.get_context(&key))
                .cloned()
                .unwrap_or(json!(null));
            SocketResponse::ok(Some(json!({ "value": val })))
        }

        "memory_save" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let json_val = {
                let mem = MEMORY_STORE.lock().unwrap();
                match mem.get(&sandbox_id) {
                    Some(m) => match m.serialize_to_json() {
                        Ok(j) => j,
                        Err(e) => return SocketResponse::err(e),
                    },
                    None => return SocketResponse::err("no memory found for sandbox"),
                }
            };
            match PERSISTENCE.save_agent_memory(sandbox_id, &json_val) {
                Ok(())  => SocketResponse::ok(None),
                Err(e)  => SocketResponse::err(e),
            }
        }

        "memory_load" => {
            let sandbox_id = match req.sandbox.as_ref().and_then(parse_id) {
                Some(id) => id,
                None => return SocketResponse::err("missing sandbox id"),
            };
            let json_val = match PERSISTENCE.load_agent_memory(sandbox_id) {
                Ok(j)  => j,
                Err(e) => return SocketResponse::err(e),
            };
            let agent_mem = match AgentMemory::deserialize_from_json(&json_val) {
                Ok(m)  => m,
                Err(e) => return SocketResponse::err(e),
            };
            MEMORY_STORE.lock().unwrap().insert(sandbox_id, agent_mem);
            SocketResponse::ok(Some(json_val))
        }

        // ── agent coordination ────────────────────────────────────────────────
        "agent_spawn" => {
            let max_iter = req.input.as_ref().and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let agent_id = COORDINATOR.lock().unwrap().spawn_agent(max_iter);
            SocketResponse::ok(Some(json!({ "agent": agent_id })))
        }

        "agent_run" => {
            let agent_id = match req.agent {
                Some(id) => id,
                None => return SocketResponse::err("missing agent id"),
            };
            let prompt = req.command.clone().unwrap_or_default();
            let mut coord = COORDINATOR.lock().unwrap();
            match coord.get_agent(agent_id) {
                Some(agent) => {
                    let tools: Vec<Box<dyn crate::tools::Tool>> = tool_registry::create_all_tools();
                    match agent.run(&prompt, &tools) {
                        Ok(result) => SocketResponse::ok(Some(json!({ "result": result }))),
                        Err(e)     => SocketResponse::err(e),
                    }
                }
                None => SocketResponse::err(format!("agent {} not found", agent_id)),
            }
        }

        "agent_status" => {
            let agent_id = match req.agent {
                Some(id) => id,
                None => return SocketResponse::err("missing agent id"),
            };
            let coord = COORDINATOR.lock().unwrap();
            match coord.agents.get(&agent_id) {
                Some(agent) => SocketResponse::ok(Some(agent.status())),
                None        => SocketResponse::err(format!("agent {} not found", agent_id)),
            }
        }

        other => SocketResponse::err(format!("unknown request type '{}'", other)),
    }
}

// ── Connection handling ───────────────────────────────────────────────────────

fn handle_connection(mut stream: UnixStream, _state: Arc<Mutex<HashMap<u64, Sandbox>>>) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut buffer = String::new();
    reader.read_line(&mut buffer).context("read request")?;

    if buffer.trim().is_empty() {
        return Ok(());
    }

    let req: SocketRequest = serde_json::from_str(&buffer).context("parse request JSON")?;
    let resp = handle_request(req);
    let text = serde_json::to_string(&resp).context("serialize response")?;
    stream.write_all(text.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn create_listener(path: &str) -> Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).context("bind unix socket")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o666);
        std::fs::set_permissions(path, perms).context("chmod socket")?;
    }
    Ok(listener)
}

pub fn run_server(path: &str) -> Result<()> {
    std::fs::create_dir_all("/var/log/agentd").ok();
    let _ = PERSISTENCE.init();
    let listener = create_listener(path)?;

    println!("Socket server listening on {}", path);

    // clone Arc reference once for use when spawning threads
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&SANDBOXES);
                thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, state) {
                        eprintln!("connection error: {}", e);
                    }
                });
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::NamedTempFile;

    fn clear_store() {
        SANDBOXES.lock().unwrap().clear();
    }

    fn create_test_sandbox() -> u64 {
        let resp = handle_request(SocketRequest {
            request_type: "create_sandbox".into(),
            ..Default::default()
        });
        assert_eq!(resp.status, "ok");
        resp.result.unwrap()["sandbox"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
    }

    fn setup_sandbox_with_tool(tool: Box<dyn crate::tools::Tool>) -> u64 {
        let id = create_test_sandbox();
        let mut store = SANDBOXES.lock().unwrap();
        if let Some(sb) = store.get_mut(&id) {
            sb.register_tool(tool);
        }
        drop(store);
        id
    }

    #[test]
    fn listener_permission_bits() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap();
        let listener = create_listener(path).expect("bind");
        let metadata = fs::metadata(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o666);
        }
        drop(listener);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_socket_response_ok() {
        let resp = SocketResponse::ok(Some(json!({ "test": "value" })));
        assert_eq!(resp.status, "ok");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_socket_response_err() {
        let resp = SocketResponse::err("something went wrong");
        assert_eq!(resp.status, "error");
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
    }

    #[test]
    fn test_parse_id_string() {
        let v = json!("12345");
        assert_eq!(parse_id(&v), Some(12345u64));
    }

    #[test]
    fn test_parse_id_number() {
        let v = json!(12345u64);
        assert_eq!(parse_id(&v), Some(12345u64));
    }

    #[test]
    fn test_parse_id_invalid() {
        let v = json!("not-a-number");
        assert_eq!(parse_id(&v), None);
    }

    #[test]
    fn create_list_and_destroy() {
        clear_store();
        let resp = handle_request(SocketRequest {
            request_type: "create_sandbox".into(),
            ..Default::default()
        });
        assert_eq!(resp.status, "ok");
        let id_str = resp.result.unwrap()["sandbox"].as_str().unwrap().to_string();
        let id = id_str.parse::<u64>().unwrap();

        // Verify listed
        let list = handle_request(SocketRequest {
            request_type: "list".into(),
            ..Default::default()
        });
        let ids: Vec<String> = serde_json::from_value(list.result.unwrap()).unwrap();
        assert!(ids.contains(&id_str));

        // Destroy by string id
        let destroy = handle_request(SocketRequest {
            request_type: "destroy_sandbox".into(),
            sandbox: Some(json!(id_str)),
            ..Default::default()
        });
        assert_eq!(destroy.status, "ok");

        // Verify gone
        let list2 = handle_request(SocketRequest {
            request_type: "list".into(),
            ..Default::default()
        });
        let ids2: Vec<String> = serde_json::from_value(list2.result.unwrap()).unwrap();
        assert!(!ids2.contains(&id_str));
    }

    #[test]
    fn destroy_by_numeric_id() {
        clear_store();
        let id = create_test_sandbox();
        let resp = handle_request(SocketRequest {
            request_type: "destroy_sandbox".into(),
            sandbox: Some(json!(id)),
            ..Default::default()
        });
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn unknown_request_returns_error() {
        clear_store();
        let resp = handle_request(SocketRequest {
            request_type: "does_not_exist".into(),
            ..Default::default()
        });
        assert_eq!(resp.status, "error");
    }

    #[test]
    fn invoke_tool_without_container_returns_error() {
        clear_store();
        let id = create_test_sandbox();
        let resp = handle_request(SocketRequest {
            request_type: "invoke_tool".into(),
            sandbox: Some(json!(id.to_string())),
            name: Some("run_command".into()),
            input: Some(json!({ "cmd": "echo hello" })),
            ..Default::default()
        });
        assert_eq!(resp.status, "error");
        assert!(resp.error.unwrap().contains("container"));
    }

    #[test]
    fn test_multiple_sandboxes_unique_ids() {
        clear_store();
        let id1 = create_test_sandbox();
        let id2 = create_test_sandbox();
        let id3 = create_test_sandbox();
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_sandbox_returns_string_id() {
        clear_store();
        let resp = handle_request(SocketRequest {
            request_type: "create_sandbox".into(),
            ..Default::default()
        });
        assert_eq!(resp.status, "ok");
        let result = resp.result.unwrap();
        assert!(result["sandbox"].is_string());
        assert!(result["sandbox"].as_str().unwrap().parse::<u64>().is_ok());
    }

    #[test]
    fn test_missing_sandbox_errors_cleanly() {
        clear_store();
        let resp = handle_request(SocketRequest {
            request_type: "invoke_tool".into(),
            sandbox: Some(json!("99999999")),
            container: Some(json!("88888888")),
            name: Some("run_command".into()),
            input: Some(json!({ "cmd": "echo hi" })),
            ..Default::default()
        });
        assert_eq!(resp.status, "error");
        assert!(resp.error.unwrap().contains("not found"));
    }

    #[test]
    fn test_create_channel() {
        clear_store();
        let sb1 = create_test_sandbox();
        let sb2 = create_test_sandbox();
        let resp = handle_request(SocketRequest {
            request_type: "create_channel".into(),
            sandbox: Some(json!(sb1.to_string())),
            to: Some(sb2),
            ..Default::default()
        });
        assert_eq!(resp.status, "ok");
        assert!(resp.result.unwrap()["channel"].is_number());
    }

    #[test]
    fn test_unknown_policy_errors() {
        clear_store();
        let id = create_test_sandbox();
        let resp = handle_request(SocketRequest {
            request_type: "set_policy".into(),
            sandbox: Some(json!(id.to_string())),
            name: Some("nonexistent_policy".into()),
            ..Default::default()
        });
        assert_eq!(resp.status, "error");
    }
}