use clap::{Parser, Subcommand};
use libagent::{socket_server, ResourceLimits, Sandbox};
use std::io::Write;
use std::path::PathBuf;

/// Command-line interface for the agent runtime.
#[derive(Parser)]
#[command(name = "agentd")]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new sandbox and print its id
    CreateSandbox {
        #[arg(long)]
        ram: Option<u64>,
        #[arg(long)]
        cpu: Option<u64>,
    },
    /// Run a prompt using an agent in a sandbox
    Run {
        #[arg(long)]
        sandbox: u64,
        prompt: String,
    },
    /// Register a tool with the sandbox
    RegisterTool {
        #[arg(long)]
        sandbox: u64,
        #[arg(long)]
        name: String,
    },
    /// Invoke a tool with JSON input
    InvokeTool {
        #[arg(long)]
        sandbox: u64,
        #[arg(long)]
        name: String,
        #[arg(long)]
        input: String,
    },
    /// List all active sandboxes
    List,
    /// Get status of an agent
    Status {
        #[arg(long)]
        agent: u64,
    },
    /// Start Unix socket API server
    Socket {
        #[arg(long, default_value = "/tmp/agentd.sock")]
        path: String,
    },
    /// Vertex AI Gemini loop: tools executed via agentd socket
    Agent {
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "/tmp/agentd.sock")]
        socket: String,
    },
    /// Multi-sandbox orchestration (Gemini plan + parallel agents + synthesis)
    Orchestrate {
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "/tmp/agentd.sock")]
        socket: String,
        #[arg(long, default_value_t = 10)]
        max_agents: usize,
        /// Verbose logging (HTTP/socket payloads, round timings, etc.)
        #[arg(long, default_value_t = false)]
        debug: bool,
    },
    /// Same as orchestrate, but stay in-process: enter follow-ups without exiting (reuses sandboxes by team name)
    OrchestrateInteractive {
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "/tmp/agentd.sock")]
        socket: String,
        #[arg(long, default_value_t = 10)]
        max_agents: usize,
        /// Persist transcript, context, sandbox map, and warm container ids (JSON). Also used with `--resume`.
        #[arg(long, value_name = "PATH")]
        session_file: Option<PathBuf>,
        /// Load `--session-file` and continue the REPL (skips the initial task prompt).
        #[arg(long, default_value_t = false)]
        resume: bool,
        /// Verbose logging (HTTP/socket payloads, round timings, etc.)
        #[arg(long, default_value_t = false)]
        debug: bool,
    },
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match cli.cmd {
        Commands::CreateSandbox { ram, cpu } => {
            let limits = ResourceLimits {
                ram_bytes: ram,
                cpu_millis: cpu,
            };
            match Sandbox::new(limits) {
                Ok(sb) => println!("created sandbox {}", sb.id()),
                Err(e) => eprintln!("error: {}", e),
            }
        }
        Commands::Run {
            sandbox: _,
            prompt: _,
        } => {
            println!("run: use library API directly for now");
        }
        Commands::RegisterTool { sandbox: _, name } => {
            println!("registered tool {} - use library API", name);
        }
        Commands::InvokeTool {
            sandbox: _,
            name,
            input: _,
        } => {
            println!("invoked {} - use library API", name);
        }
        Commands::List => {
            println!("list: use persistence layer or library API");
        }
        Commands::Status { agent: _ } => {
            println!("status: placeholder");
        }
        Commands::Socket { path } => {
            if let Err(e) = socket_server::run_server(&path) {
                eprintln!("socket server error: {}", e);
            }
        }
        Commands::Agent {
            prompt,
            project,
            socket,
        } => {
            libagent::vertex_agent::run(&prompt, &project, &socket)?;
        }
        Commands::Orchestrate {
            prompt,
            project,
            socket,
            max_agents,
            debug,
        } => {
            libagent::orchestration::set_debug(debug);
            libagent::orchestration::orchestrator::run(
                &prompt,
                &project,
                &socket,
                max_agents,
            )?;
        }
        Commands::OrchestrateInteractive {
            project,
            socket,
            max_agents,
            session_file,
            resume,
            debug,
        } => {
            libagent::orchestration::set_debug(debug);
            #[cfg(not(unix))]
            {
                return Err(anyhow::anyhow!(
                    "orchestrate interactive requires Unix (agentd socket)"
                ));
            }
            #[cfg(unix)]
            {
                use std::io::{self, BufRead};

                let save_path = session_file.as_deref();
                if resume && save_path.is_none() {
                    return Err(anyhow::anyhow!(
                        "--resume requires --session-file PATH"
                    ));
                }

                println!(
                    "Interactive orchestration — same process; sandboxes and (when agent_ids match) worker + merge containers are reused.\n\
                     Session coordinator refreshes a live briefing for workers each turn. Type exit or quit to stop."
                );

                let mut session = if resume {
                    let path = save_path.unwrap();
                    if !path.exists() {
                        return Err(anyhow::anyhow!("session file not found: {:?}", path));
                    }
                    let mut s =
                        libagent::orchestration::orchestrator::OrchestrationInteractiveSession::from_session_file(
                            path,
                        )?;
                    s.project_id = project;
                    s.socket_path = socket;
                    s.max_agents = max_agents;
                    println!(
                        "Resumed session ({} user turns, {} assistant turns). Enter next instruction.",
                        s.transcript.len(),
                        s.assistant_turns.len()
                    );
                    s
                } else {
                    print!("First task: ");
                    io::stdout().flush()?;
                    let mut first = String::new();
                    io::stdin().lock().read_line(&mut first)?;
                    let first = first.trim();
                    if first.is_empty() {
                        return Ok(());
                    }
                    if first.eq_ignore_ascii_case("exit") || first.eq_ignore_ascii_case("quit") {
                        return Ok(());
                    }
                    let (mut session, out) =
                        libagent::orchestration::orchestrator::run_session_first(
                            first, &project, &socket, max_agents,
                        )?;
                    println!("{out}");
                    if let Some(p) = save_path {
                        session.save_session_file(p)?;
                    }
                    session
                };

                loop {
                    print!("orchestrate> ");
                    io::stdout().flush()?;
                    let mut line = String::new();
                    if io::stdin().lock().read_line(&mut line)? == 0 {
                        break;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if line.eq_ignore_ascii_case("exit") || line.eq_ignore_ascii_case("quit") {
                        break;
                    }
                    match session.follow_up(line) {
                        Ok(t) => {
                            println!("{t}");
                            if let Some(p) = save_path {
                                if let Err(e) = session.save_session_file(p) {
                                    eprintln!("warning: could not save session: {e:#}");
                                }
                            }
                        }
                        Err(e) => eprintln!("error: {e:#}"),
                    }
                }
            }
        }
    }
    Ok(())
}
