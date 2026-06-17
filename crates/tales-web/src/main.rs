//! `tales-web` — browser supervision UI.
//!
//! A tiny local server that runs a Tales session and streams the Claude↔Codex
//! conversation to a web page over a WebSocket. You watch the discussion live,
//! type to interject, and click to approve (or reject) at the execution gate —
//! the same human-in-the-loop flow as the TUI, but in your browser.
//!
//! The core is untouched: this is just another frontend over the UI-agnostic
//! event/command bus.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use tales_core::agent::claude::ClaudeAdapter;
use tales_core::agent::codex::CodexAdapter;
use tales_core::agent::mock::MockAdapter;
use tales_core::agent::{AgentAdapter, SpawnCtx};
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::{OrchestratorEvent, UserCommand};
use tales_core::orchestrator::Orchestrator;

const INDEX_HTML: &str = include_str!("index.html");

#[derive(Parser, Debug, Clone)]
#[command(name = "tales-web", about = "Browser UI to watch & steer Claude and Codex")]
struct Args {
    /// The task the agents discuss (and, on your confirmation, execute).
    task: String,
    #[arg(long, default_value = "claude")]
    drafter: String,
    #[arg(long, default_value = "codex")]
    critic: String,
    #[arg(long)]
    drafter_model: Option<String>,
    #[arg(long)]
    critic_model: Option<String>,
    #[arg(long, default_value_t = 4)]
    turns: usize,
    #[arg(long)]
    cwd: Option<String>,
    #[arg(long, default_value = "read-only")]
    sandbox: String,
    /// Scripted mock agents — no API calls. Great for trying the UI.
    #[arg(long)]
    demo: bool,
    /// Port to serve on.
    #[arg(long, default_value_t = 7878)]
    port: u16,
    /// Don't auto-open the browser.
    #[arg(long)]
    no_open: bool,
}

/// Open a URL in the default browser (best-effort, non-blocking).
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let cmd = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", url]);
    let _ = std::process::Command::new(cmd.0).args(cmd.1).spawn();
}

/// Shared across connections: ONE session per server run. The first WebSocket
/// to connect starts it (taking the single command receiver); later tabs just
/// subscribe to the same bus. When the last client disconnects, the session is
/// told to shut down so no agent processes linger.
struct AppState {
    cfg: Arc<Args>,
    bus: EventBus,
    commands_rx: Mutex<Option<mpsc::Receiver<UserCommand>>>,
    clients: AtomicUsize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let (bus, commands_rx) = EventBus::new(8192, 256);
    let state = Arc::new(AppState {
        cfg: Arc::new(args.clone()),
        bus,
        commands_rx: Mutex::new(Some(commands_rx)),
        clients: AtomicUsize::new(0),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let url = format!("http://{addr}");
    println!("\n  ❯ tales  ·  {url}\n    task: {}\n", args.task);
    if !args.no_open {
        let url = url.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
            open_browser(&url);
        });
    }
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> impl IntoResponse {
    Html(INDEX_HTML)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    // The first connection starts the one-and-only session for this server run.
    if let Some(commands_rx) = state.commands_rx.lock().await.take() {
        let bus = state.bus.clone();
        let cfg = state.cfg.clone();
        bus.emit(OrchestratorEvent::Log {
            level: "task".to_string(),
            msg: cfg.task.clone(),
        });
        tokio::spawn(async move { run_session(bus, commands_rx, cfg).await });
    }

    state.clients.fetch_add(1, Ordering::SeqCst);
    let commands_tx = state.bus.commands();
    let mut events = state.bus.subscribe();
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Bus events → browser.
    let forwarder = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    let payload = event_to_json(&ev).to_string();
                    if ws_tx.send(Message::Text(payload)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    // Browser → commands.
    while let Some(Ok(msg)) = ws_rx.next().await {
        if let Message::Text(text) = msg {
            if let Some(cmd) = parse_command(&text) {
                let _ = commands_tx.send(cmd).await;
            }
        }
    }
    forwarder.abort();

    // Last client out → shut the session down so no agent processes linger,
    // but allow a short grace for a transient reconnect (the page auto-retries).
    if state.clients.fetch_sub(1, Ordering::SeqCst) == 1 {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if state.clients.load(Ordering::SeqCst) == 0 {
                let _ = state.bus.commands().send(UserCommand::Shutdown).await;
            }
        });
    }
}

fn event_to_json(ev: &OrchestratorEvent) -> Value {
    match ev {
        OrchestratorEvent::AgentSpawned { agent, label, .. } => {
            json!({ "kind": "agent", "agent": agent.to_string(), "label": label })
        }
        OrchestratorEvent::Token { agent, text } => {
            json!({ "kind": "token", "agent": agent.to_string(), "text": text })
        }
        OrchestratorEvent::TurnStarted { agent, role } => {
            json!({ "kind": "turn_started", "agent": agent.to_string(), "role": role })
        }
        OrchestratorEvent::Message { agent, text } => {
            json!({ "kind": "message", "agent": agent.to_string(), "text": text })
        }
        OrchestratorEvent::UserMessage { text } => json!({ "kind": "user", "text": text }),
        OrchestratorEvent::ToolActivity { agent, summary } => {
            json!({ "kind": "tool", "agent": agent.to_string(), "summary": summary })
        }
        OrchestratorEvent::TurnComplete { agent, cost_usd } => {
            json!({ "kind": "turn_done", "agent": agent.to_string(), "cost": cost_usd })
        }
        OrchestratorEvent::AgentExited { agent, code } => {
            json!({ "kind": "exited", "agent": agent.to_string(), "code": code })
        }
        OrchestratorEvent::PhaseChanged { phase } => json!({ "kind": "phase", "phase": phase }),
        OrchestratorEvent::RecommendationReady { executor, rationale } => {
            json!({ "kind": "recommendation", "executor": executor, "rationale": rationale })
        }
        OrchestratorEvent::AwaitingConfirmation { prompt } => {
            json!({ "kind": "awaiting", "prompt": prompt })
        }
        OrchestratorEvent::Log { level, msg } => json!({ "kind": "log", "level": level, "msg": msg }),
        OrchestratorEvent::Fatal { msg } => json!({ "kind": "fatal", "msg": msg }),
    }
}

fn parse_command(text: &str) -> Option<UserCommand> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v.get("kind").and_then(Value::as_str)? {
        "say" => Some(UserCommand::InjectNote {
            agent: Uuid::nil(),
            text: v.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
        }),
        "confirm" => Some(UserCommand::ConfirmExecution {
            executor: v.get("executor").and_then(Value::as_str).unwrap_or("").to_string(),
        }),
        "reject" => Some(UserCommand::Reject),
        "shutdown" => Some(UserCommand::Shutdown),
        _ => None,
    }
}

fn make_adapter(name: &str) -> Result<Box<dyn AgentAdapter>, String> {
    match name {
        "codex" => Ok(Box::new(CodexAdapter::new())),
        "claude" => Ok(Box::new(ClaudeAdapter::new())),
        other => Err(format!("unknown agent '{other}'")),
    }
}

/// Claude is restricted to file tools so that, if chosen as executor, it can't
/// stall on an unapproved Bash call in headless mode.
fn ctx(agent: Uuid, label: &str, cwd: &std::path::Path, model: Option<String>, sandbox: &str) -> SpawnCtx {
    let allowed_tools = if label.eq_ignore_ascii_case("claude") {
        Some(vec!["Write".into(), "Edit".into(), "MultiEdit".into(), "Read".into()])
    } else {
        None
    };
    SpawnCtx {
        agent,
        label: label.to_string(),
        cwd: cwd.to_path_buf(),
        model,
        permission_mode: "acceptEdits".to_string(),
        sandbox: sandbox.to_string(),
        allowed_tools,
    }
}

async fn run_session(bus: EventBus, mut commands_rx: mpsc::Receiver<UserCommand>, cfg: Arc<Args>) {
    let mut orch = Orchestrator::new(bus.clone());

    let result = async {
        let cwd = cfg
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);

        if cfg.demo {
            let vote = r#"{"recommended_executor":"claude","confidence":0.9,"rationale":"best at writing the code"}"#;
            let drafter = MockAdapter::new(vec![
                "Draft plan:\n- token-bucket limiter per API key\n- Redis-backed counters\n- 429 + Retry-After header".into(),
                vote.into(),
                "Done — created limiter.rs and wired the middleware.".into(),
            ]);
            let critic = MockAdapter::new(vec![
                "Token-bucket vs fixed-window mismatch — which do we want? And what's the burst size? Who is the unauthenticated identity?".into(),
                r#"{"recommended_executor":"claude","confidence":0.7,"rationale":"agree"}"#.into(),
            ]);
            orch.add_agent(Box::new(drafter), ctx(Uuid::new_v4(), "claude", &cwd, None, &cfg.sandbox), Role::Drafter).await?;
            orch.add_agent(Box::new(critic), ctx(Uuid::new_v4(), "codex", &cwd, None, &cfg.sandbox), Role::Critic).await?;
        } else {
            orch.add_agent(
                make_adapter(&cfg.drafter).map_err(tales_core::TalesError::Other)?,
                ctx(Uuid::new_v4(), &cfg.drafter, &cwd, cfg.drafter_model.clone(), &cfg.sandbox),
                Role::Drafter,
            )
            .await?;
            orch.add_agent(
                make_adapter(&cfg.critic).map_err(tales_core::TalesError::Other)?,
                ctx(Uuid::new_v4(), &cfg.critic, &cwd, cfg.critic_model.clone(), &cfg.sandbox),
                Role::Critic,
            )
            .await?;
        }

        orch.run_interactive(&cfg.task, cfg.turns, &mut commands_rx).await
    }
    .await;

    match result {
        Ok(outcome) => bus.emit(OrchestratorEvent::Log {
            level: "done".to_string(),
            msg: format!("{outcome:?}"),
        }),
        Err(e) => bus.emit(OrchestratorEvent::Fatal { msg: e.to_string() }),
    }
    orch.shutdown().await;
}
