//! `tales` — command-line frontend over the orchestration core.
//!
//! Two subcommands:
//!   * `solo`    — drive a single agent for one turn (claude or codex).
//!   * `discuss` — run a live drafter/critic discussion between two agents,
//!                 streaming the conversation to the console.
//!
//! This is a thin frontend: it only subscribes to `OrchestratorEvent`s and
//! sends commands. The ratatui TUI (M7) is a second frontend over the same bus.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use tales_core::agent::claude::ClaudeAdapter;
use tales_core::agent::codex::CodexAdapter;
use tales_core::agent::{AgentAdapter, AgentCommand, AgentEvent, SpawnCtx};
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::{OrchestratorEvent, UserCommand};
use tales_core::orchestrator::{Orchestrator, RunOutcome};
use tales_core::worktree::{MergeOutcome, WorktreeManager};

#[derive(Parser, Debug)]
#[command(name = "tales", about = "Multi-agent AI coding orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Drive a single agent for one turn.
    Solo {
        /// The prompt to send.
        prompt: String,
        /// Which agent: claude | codex.
        #[arg(long, default_value = "claude")]
        agent: String,
        /// Model alias/id (optional).
        #[arg(long)]
        model: Option<String>,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Codex sandbox policy.
        #[arg(long, default_value = "workspace-write")]
        sandbox: String,
    },
    /// Full pipeline, non-interactive: discuss → recommend → auto-confirm the
    /// chosen executor → execute. The scriptable counterpart to the live TUI.
    Run {
        /// The task to plan and execute.
        prompt: String,
        /// Drafter agent: claude | codex.
        #[arg(long, default_value = "claude")]
        drafter: String,
        /// Critic agent: claude | codex.
        #[arg(long, default_value = "codex")]
        critic: String,
        /// Which agent executes the agreed plan (auto-confirmed).
        #[arg(long, default_value = "claude")]
        execute: String,
        #[arg(long)]
        drafter_model: Option<String>,
        #[arg(long)]
        critic_model: Option<String>,
        #[arg(long, default_value_t = 4)]
        turns: usize,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value = "workspace-write")]
        sandbox: String,
        /// Run the executor in its own git worktree, then merge the result back
        /// into the current branch (requires a git repo).
        #[arg(long)]
        worktree: bool,
    },
    /// Run a live drafter/critic discussion between two agents.
    Discuss {
        /// The task the agents discuss.
        prompt: String,
        /// Drafter agent: claude | codex.
        #[arg(long, default_value = "claude")]
        drafter: String,
        /// Critic agent: claude | codex.
        #[arg(long, default_value = "codex")]
        critic: String,
        /// Total number of turns (alternating drafter/critic).
        #[arg(long, default_value_t = 4)]
        turns: usize,
        /// Model for the drafter (optional).
        #[arg(long)]
        drafter_model: Option<String>,
        /// Model for the critic (optional).
        #[arg(long)]
        critic_model: Option<String>,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Codex sandbox policy.
        #[arg(long, default_value = "read-only")]
        sandbox: String,
    },
}

fn make_adapter(name: &str) -> Result<Box<dyn AgentAdapter>, String> {
    match name {
        "codex" => Ok(Box::new(CodexAdapter::new())),
        "claude" => Ok(Box::new(ClaudeAdapter::new())),
        other => Err(format!("unknown agent '{other}' (expected: claude | codex)")),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Solo {
            prompt,
            agent,
            model,
            cwd,
            sandbox,
        } => run_solo(prompt, agent, model, cwd, sandbox).await,
        Command::Run {
            prompt,
            drafter,
            critic,
            execute,
            drafter_model,
            critic_model,
            turns,
            cwd,
            sandbox,
            worktree,
        } => {
            run_pipeline(
                prompt, drafter, critic, execute, drafter_model, critic_model, turns, cwd, sandbox,
                worktree,
            )
            .await
        }
        Command::Discuss {
            prompt,
            drafter,
            critic,
            turns,
            drafter_model,
            critic_model,
            cwd,
            sandbox,
        } => {
            run_discuss(
                prompt,
                drafter,
                critic,
                turns,
                drafter_model,
                critic_model,
                cwd,
                sandbox,
            )
            .await
        }
    }
}

async fn run_solo(
    prompt: String,
    agent_name: String,
    model: Option<String>,
    cwd: Option<String>,
    sandbox: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let agent = Uuid::new_v4();
    let ctx = SpawnCtx {
        agent,
        label: agent_name.clone(),
        cwd,
        model,
        permission_mode: "acceptEdits".to_string(),
        sandbox,
        allowed_tools: None,
    };

    let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(256);
    let mut adapter = match make_adapter(&agent_name) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let cmd_tx = adapter.spawn(ctx, events_tx).await?;
    cmd_tx.send(AgentCommand::StartTurn { prompt }).await?;

    let mut streaming = false;
    while let Some(event) = events_rx.recv().await {
        match event {
            AgentEvent::SessionReady {
                native_session_id, ..
            } => println!("● session {native_session_id}"),
            AgentEvent::TurnStarted { turn, .. } => {
                print!("\n[turn {turn}] ");
                flush();
            }
            AgentEvent::TokenDelta { text, .. } => {
                streaming = true;
                print!("{text}");
                flush();
            }
            AgentEvent::MessageFinal { text, .. } => {
                if !streaming {
                    print!("{text}");
                    flush();
                }
            }
            AgentEvent::ToolCall { name, .. } => println!("\n  ⚙ tool: {name}"),
            AgentEvent::TurnComplete { cost_usd, .. } => {
                match cost_usd {
                    Some(c) => println!("\n✓ turn complete (${c:.4})"),
                    None => println!("\n✓ turn complete"),
                }
                let _ = cmd_tx.send(AgentCommand::Shutdown).await;
            }
            AgentEvent::Error { message, fatal, .. } => {
                eprintln!("\n✗ error{}: {message}", if fatal { " (fatal)" } else { "" });
            }
            AgentEvent::Exited { code, .. } => {
                println!("● agent exited ({code:?})");
                break;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_discuss(
    prompt: String,
    drafter: String,
    critic: String,
    turns: usize,
    drafter_model: Option<String>,
    critic_model: Option<String>,
    cwd: Option<String>,
    sandbox: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);

    let (bus, _commands_rx) = EventBus::new(1024, 64);

    // Console frontend: render the conversation as it streams onto the bus.
    let mut events = bus.subscribe();
    let printer = tokio::spawn(async move {
        let mut labels: HashMap<Uuid, String> = HashMap::new();
        loop {
            match events.recv().await {
                Ok(event) => match event {
                    OrchestratorEvent::AgentSpawned { agent, label, .. } => {
                        println!("● enrolled {label}");
                        labels.insert(agent, label);
                    }
                    OrchestratorEvent::Log { msg, .. } => {
                        println!("\n──────── {msg} ────────");
                    }
                    OrchestratorEvent::Message { agent, text } => {
                        let who = labels.get(&agent).cloned().unwrap_or_else(|| "?".into());
                        println!("\n{who}:\n{text}");
                    }
                    OrchestratorEvent::ToolActivity { agent, summary } => {
                        let who = labels.get(&agent).cloned().unwrap_or_else(|| "?".into());
                        println!("  ⚙ {who}: {summary}");
                    }
                    OrchestratorEvent::TurnComplete { cost_usd, .. } => {
                        if let Some(c) = cost_usd {
                            println!("  (turn cost ${c:.4})");
                        }
                    }
                    OrchestratorEvent::Fatal { msg } => println!("✗ fatal: {msg}"),
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break, // bus closed
            }
        }
    });

    let mut orch = Orchestrator::new(bus.clone());

    let drafter_ctx = SpawnCtx {
        agent: Uuid::new_v4(),
        label: drafter.clone(),
        cwd: cwd.clone(),
        model: drafter_model,
        permission_mode: "acceptEdits".to_string(),
        sandbox: sandbox.clone(),
        allowed_tools: None,
    };
    let critic_ctx = SpawnCtx {
        agent: Uuid::new_v4(),
        label: critic.clone(),
        cwd,
        model: critic_model,
        permission_mode: "acceptEdits".to_string(),
        sandbox,
        allowed_tools: None,
    };

    orch.add_agent(make_adapter(&drafter)?, drafter_ctx, Role::Drafter)
        .await?;
    orch.add_agent(make_adapter(&critic)?, critic_ctx, Role::Critic)
        .await?;

    println!("\n=== discussion: {prompt} ({turns} turns) ===");
    orch.run_discussion(&prompt, turns).await?;
    orch.shutdown().await;

    // Let the printer drain the final events, then stop it. We abort rather
    // than await: `orch` still holds a bus clone, so the broadcast never closes
    // on its own.
    sleep(Duration::from_millis(200)).await;
    printer.abort();

    println!("\n=== discussion complete ===");
    Ok(())
}

fn flush() {
    let _ = std::io::stdout().flush();
}

#[allow(clippy::too_many_arguments)]
fn mk_ctx(
    agent: Uuid,
    label: &str,
    cwd: &std::path::Path,
    model: Option<String>,
    sandbox: &str,
    permission_mode: &str,
    allowed_tools: Option<Vec<String>>,
) -> SpawnCtx {
    SpawnCtx {
        agent,
        label: label.to_string(),
        cwd: cwd.to_path_buf(),
        model,
        permission_mode: permission_mode.to_string(),
        sandbox: sandbox.to_string(),
        allowed_tools,
    }
}

async fn is_git_repo(cwd: &std::path::Path) -> bool {
    tokio::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    prompt: String,
    drafter: String,
    critic: String,
    execute: String,
    drafter_model: Option<String>,
    critic_model: Option<String>,
    turns: usize,
    cwd: Option<String>,
    sandbox: String,
    worktree: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let (bus, mut commands_rx) = EventBus::new(2048, 64);
    let commands_tx = bus.commands();

    let mut events = bus.subscribe();
    let printer = tokio::spawn(async move {
        let mut labels: HashMap<Uuid, String> = HashMap::new();
        loop {
            match events.recv().await {
                Ok(ev) => match ev {
                    OrchestratorEvent::AgentSpawned { agent, label, .. } => {
                        println!("● enrolled {label}");
                        labels.insert(agent, label);
                    }
                    OrchestratorEvent::Log { msg, .. } => println!("\n──────── {msg} ────────"),
                    OrchestratorEvent::Message { agent, text } => {
                        let who = labels.get(&agent).cloned().unwrap_or_else(|| "?".into());
                        println!("\n{who}:\n{text}");
                    }
                    OrchestratorEvent::UserMessage { text } => println!("\nyou:\n{text}"),
                    OrchestratorEvent::ToolActivity { agent, summary } => {
                        let who = labels.get(&agent).cloned().unwrap_or_else(|| "?".into());
                        println!("  ⚙ {who}: {summary}");
                    }
                    OrchestratorEvent::RecommendationReady { executor, rationale } => {
                        println!("\n★ recommended executor: {executor}\n{rationale}");
                    }
                    OrchestratorEvent::TurnComplete { cost_usd, .. } => {
                        if let Some(c) = cost_usd {
                            println!("  (turn cost ${c:.4})");
                        }
                    }
                    OrchestratorEvent::Fatal { msg } => println!("✗ fatal: {msg}"),
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    // The executor must be one of the two participants.
    let exec_lc = execute.to_lowercase();
    let exec_is_drafter = drafter.to_lowercase() == exec_lc;
    let exec_is_critic = critic.to_lowercase() == exec_lc;
    if !exec_is_drafter && !exec_is_critic {
        printer.abort();
        return Err(format!(
            "--execute '{execute}' must be one of the participants ('{drafter}', '{critic}')"
        )
        .into());
    }

    // Agents run on acceptEdits (Write/Edit auto-approved). The Claude executor
    // is additionally restricted to file tools so it cannot stall on an
    // unapproved Bash call in headless mode (Write creates parent dirs, so no
    // shell mkdir is needed). Non-executors and Codex are unrestricted.
    let tools_for = |label: &str| -> Option<Vec<String>> {
        if label.to_lowercase() == exec_lc && label.to_lowercase() == "claude" {
            Some(vec!["Write".into(), "Edit".into(), "MultiEdit".into(), "Read".into()])
        } else {
            None
        }
    };

    let drafter_id = Uuid::new_v4();
    let critic_id = Uuid::new_v4();
    let exec_id = if exec_is_drafter { drafter_id } else { critic_id };

    // Optional git-worktree isolation: the executor runs in its own worktree,
    // and the result is merged back after execution.
    let use_wt = worktree && is_git_repo(&cwd).await;
    if worktree && !use_wt {
        eprintln!("note: --worktree ignored (cwd is not a git repository)");
    }
    let mut wt_mgr: Option<WorktreeManager> = None;
    let mut exec_cwd = cwd.clone();
    if use_wt {
        let run_id = format!("run-{}", &exec_id.simple().to_string()[..8]);
        let mut mgr = WorktreeManager::init(&cwd, run_id).await?;
        let path = mgr.create(exec_id, &execute).await?;
        println!("● executor worktree: {}", path.display());
        exec_cwd = path;
        wt_mgr = Some(mgr);
    }
    // The orchestrator executes the FIRST roster entry whose label matches, so
    // when both roles share a label the drafter is the executor. Give the
    // worktree cwd only to that one agent; the other stays on the base cwd.
    let drafter_cwd = if exec_is_drafter { exec_cwd.clone() } else { cwd.clone() };
    let critic_cwd = if exec_is_critic && !exec_is_drafter {
        exec_cwd.clone()
    } else {
        cwd.clone()
    };

    // Run inside a block so that — no matter where it fails — we still abort the
    // printer task and prune the executor's worktree (its on-disk dir + branch).
    let run_result: Result<RunOutcome, Box<dyn std::error::Error>> = async {
        let mut orch = Orchestrator::new(bus.clone());
        orch.add_agent(
            make_adapter(&drafter)?,
            mk_ctx(drafter_id, &drafter, &drafter_cwd, drafter_model, &sandbox, "acceptEdits", tools_for(&drafter)),
            Role::Drafter,
        )
        .await?;
        orch.add_agent(
            make_adapter(&critic)?,
            mk_ctx(critic_id, &critic, &critic_cwd, critic_model, &sandbox, "acceptEdits", tools_for(&critic)),
            Role::Critic,
        )
        .await?;

        // Auto-confirm the executor: queued before the run, it is remembered and
        // honored when the gate opens (non-interactive equivalent of /confirm).
        commands_tx
            .send(UserCommand::ConfirmExecution { executor: execute.clone() })
            .await?;

        println!("\n=== run: {prompt}  (executor: {execute}) ===");
        let outcome = orch.run_interactive(&prompt, turns, &mut commands_rx).await?;
        orch.shutdown().await;
        Ok(outcome)
    }
    .await;

    sleep(Duration::from_millis(200)).await;
    printer.abort();

    // Merge the executor's worktree on success, and prune it on EVERY path.
    if let Some(mut mgr) = wt_mgr {
        if let Ok(RunOutcome::Executed { .. }) = &run_result {
            match mgr.diff(exec_id).await {
                Ok(d) => println!(
                    "\n=== executor changes ({} files) ===\n{}",
                    d.files_changed,
                    d.stat.trim()
                ),
                Err(e) => eprintln!("diff failed: {e}"),
            }
            match mgr.commit_and_merge(exec_id).await {
                Ok(MergeOutcome::Clean) => {
                    println!("✓ merged the executor's worktree into the base branch")
                }
                Ok(MergeOutcome::NoChanges) => println!("(executor produced no changes)"),
                Ok(MergeOutcome::Conflict { files }) => {
                    println!("⚠ merge conflicts — resolve in base: {}", files.join(", "))
                }
                Err(e) => eprintln!("merge failed: {e}"),
            }
        }
        if let Err(e) = mgr.remove(exec_id).await {
            eprintln!("worktree cleanup: {e}");
        }
    }

    let outcome = run_result?;
    println!("\n=== outcome: {outcome:?} ===");
    Ok(())
}
