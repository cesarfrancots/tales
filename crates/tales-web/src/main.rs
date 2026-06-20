//! `tales-web` — browser supervision UI.
//!
//! A tiny local server that runs a Tales session and streams the agent
//! conversation to a web page over a WebSocket. You watch the discussion live,
//! type to interject, and click to approve (or reject) at the execution gate —
//! the same human-in-the-loop flow as the TUI, but in your browser.
//!
//! The core is untouched: this is just another frontend over the UI-agnostic
//! event/command bus.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use tales_core::agent::mock::MockAdapter;
use tales_core::agent::{
    known_tools_status_json, make_adapter, model_or_default, validate_effort, validate_roster,
    validate_tool_readiness, SpawnCtx,
};
use tales_core::build_info;
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::event::{OrchestratorEvent, UserCommand};
use tales_core::orchestrator::{
    Orchestrator, ProjectContextReport, RunOutcome, DEFAULT_CONTEXT_BUDGET_CHARS,
};
use tales_core::project_context::{
    load_or_build, local_change_summary, local_change_summary_status_json,
    project_context_status_json, LocalChangeSummary, ProjectContext, ProjectContextOptions,
    DEFAULT_LOCAL_CHANGE_LINES,
};
use tales_core::prompt_forecast::{
    build_run_advice, forecast_prompt_usage, parallel_rounds, prompt_forecast_to_json,
    run_advice_to_json, PreflightContextSummary, PromptForecast, RunAdviceInput,
};
use tales_core::session::{
    AgentSeatConfig, CwdPolicy, ExecutionPermission, ExecutionPolicy, PromptBudgetConfig,
    ReportPaths, SessionConfig, SessionMode,
};
use tales_core::tool_recommendation::{
    recommend_tools, recommendation_chips_json, RecommendationInput,
};
use tales_core::workspace_profile::{
    load_profile, load_profile_if_exists, profile_status_json, run_record_from_report,
    save_profile, ProfileUpdate, WorkspaceProfile,
};

const INDEX_HTML: &str = include_str!("index.html");

#[derive(Parser, Debug, Clone)]
#[command(
    name = "tales-web",
    version = build_info::version(),
    long_version = build_info::long_version(),
    about = "Browser UI to watch and steer Tales agent sessions"
)]
struct Args {
    /// The task the agents discuss (and, on your confirmation, execute).
    task: Option<String>,
    /// Drafter tool key. See `tales doctor --all` for known tools.
    #[arg(long, default_value = "claude")]
    drafter: String,
    /// Critic tool key. See `tales doctor --all` for known tools.
    #[arg(long, default_value = "codex")]
    critic: String,
    /// Model for the drafter.
    #[arg(long)]
    drafter_model: Option<String>,
    /// Model for the critic.
    #[arg(long)]
    critic_model: Option<String>,
    /// Executor tool key selected by the approval gate default.
    #[arg(long, default_value = "claude")]
    execute: String,
    /// Model for a separate executor.
    #[arg(long)]
    execute_model: Option<String>,
    /// Reasoning effort for the drafter.
    #[arg(long)]
    drafter_effort: Option<String>,
    /// Reasoning effort for the critic.
    #[arg(long)]
    critic_effort: Option<String>,
    /// Reasoning effort for a separate executor.
    #[arg(long)]
    execute_effort: Option<String>,
    /// Total planner turns before recommendation.
    #[arg(long, default_value_t = 4)]
    turns: usize,
    /// Working directory. If omitted and TASK is omitted, choose it in the browser.
    #[arg(long)]
    cwd: Option<String>,
    /// Sandbox policy passed to tools that support one.
    #[arg(long, default_value = "read-only")]
    sandbox: String,
    /// Scripted mock agents — no API calls. Great for trying the UI.
    #[arg(long)]
    demo: bool,
    /// Plan sequentially instead of the default parallel rounds.
    #[arg(long)]
    sequential: bool,
    /// Per-agent turn timeout in seconds. Useful for high-effort models.
    #[arg(long)]
    turn_timeout_secs: Option<u64>,
    /// Max transcript/plan chars pasted into stateless prompts. 0 disables.
    #[arg(long)]
    context_budget_chars: Option<usize>,
    /// Disable cached project map/manifest context in planning prompts.
    #[arg(long)]
    no_project_context: bool,
    /// Rebuild the cached project context before starting.
    #[arg(long)]
    refresh_project_context: bool,
    /// Maximum files to index into the compact project-context file map.
    #[arg(long, default_value_t = 220)]
    project_context_max_files: usize,
    /// Maximum characters copied from each important manifest in project context.
    #[arg(long, default_value_t = 1200)]
    project_context_max_manifest_chars: usize,
    /// Write the final local session report markdown to this path.
    #[arg(long)]
    report_path: Option<PathBuf>,
    /// Write the final structured session summary JSON to this path.
    #[arg(long)]
    report_json_path: Option<PathBuf>,
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
    launch_cwd: PathBuf,
    active_cfg: Mutex<Option<Arc<Args>>>,
    preview: Mutex<Option<Arc<SessionPreview>>>,
    bus: EventBus,
    commands_rx: Mutex<Option<mpsc::Receiver<UserCommand>>>,
    clients: AtomicUsize,
    started: AtomicUsize,
}

#[derive(Clone)]
struct SessionPreview {
    cwd: PathBuf,
    project_context: Option<ProjectContext>,
    local_changes: Option<LocalChangeSummary>,
    context_summary: PreflightContextSummary,
    prompt_forecast: PromptForecast,
    workspace_profile: Option<WorkspaceProfile>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let launch_cwd = std::env::current_dir()?;
    let (active_cfg, preview) = if args.task.is_some() {
        (
            Some(Arc::new(args.clone())),
            Some(Arc::new(build_session_preview(&args)?)),
        )
    } else {
        (None, None)
    };
    let (bus, commands_rx) = EventBus::new(8192, 256);
    let state = Arc::new(AppState {
        cfg: Arc::new(args.clone()),
        launch_cwd: launch_cwd.clone(),
        active_cfg: Mutex::new(active_cfg),
        preview: Mutex::new(preview),
        bus,
        commands_rx: Mutex::new(Some(commands_rx)),
        clients: AtomicUsize::new(0),
        started: AtomicUsize::new(0),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/tools", get(tools))
        .route("/api/session", get(session))
        .route("/api/session/start", post(start_session))
        .route("/favicon.ico", get(favicon))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let url = format!("http://{addr}");
    println!(
        "\n  ❯ tales  ·  {url}\n    {}\n",
        args.task
            .as_deref()
            .map(|task| format!("task: {task}"))
            .unwrap_or_else(|| format!("workspace: {} · waiting for task", launch_cwd.display()))
    );
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

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn tools() -> Json<Value> {
    Json(tool_status_json())
}

async fn session(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = state.active_cfg.lock().await.clone();
    let preview = state.preview.lock().await.clone();
    match (cfg, preview) {
        (Some(cfg), Some(preview)) => Json(session_config_json(&cfg, &preview)).into_response(),
        _ => Json(json!({
            "started": false,
            "task": null,
            "cwd": state.launch_cwd.display().to_string(),
            "launch_cwd": state.launch_cwd.display().to_string(),
            "mode": if state.cfg.demo { "demo" } else { "live" },
        }))
        .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct StartSessionRequest {
    cwd: String,
    task: String,
}

async fn start_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StartSessionRequest>,
) -> impl IntoResponse {
    let task = req.task.trim();
    if task.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "empty_task",
                "message": "Enter a task before starting Tales."
            })),
        )
            .into_response();
    }

    let cwd = match canonical_workspace_path(&req.cwd) {
        Ok(path) => path,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "invalid_workspace",
                    "message": message,
                })),
            )
                .into_response();
        }
    };

    let mut cfg = (*state.cfg).clone();
    cfg.task = Some(task.to_string());
    cfg.cwd = Some(cwd.display().to_string());

    let mut active_cfg = state.active_cfg.lock().await;
    if state.started.load(Ordering::SeqCst) != 0 || active_cfg.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "session_already_started",
                "message": "This Tales server already has a session."
            })),
        )
            .into_response();
    }

    let preview = match build_session_preview(&cfg) {
        Ok(preview) => Arc::new(preview),
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "preview_failed",
                    "message": err.to_string(),
                })),
            )
                .into_response();
        }
    };
    let cfg = Arc::new(cfg);

    if state
        .started
        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "session_already_started",
                "message": "This Tales server already has a session."
            })),
        )
            .into_response();
    }

    *active_cfg = Some(cfg.clone());
    *state.preview.lock().await = Some(preview.clone());
    drop(active_cfg);

    if let Err(message) = spawn_started_session(state.clone()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "start_failed",
                "message": message,
            })),
        )
            .into_response();
    }

    Json(session_config_json(&cfg, &preview)).into_response()
}

fn tool_status_json() -> Value {
    known_tools_status_json()
}

fn estimated_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

fn effective_context_budget_chars(chars: Option<usize>) -> Option<usize> {
    match chars {
        Some(0) => None,
        Some(chars) => Some(chars),
        None => Some(DEFAULT_CONTEXT_BUDGET_CHARS),
    }
}

fn canonical_workspace_path(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw.trim());
    if path.as_os_str().is_empty() {
        return Err("Choose a workspace path.".to_string());
    }
    let path = path
        .canonicalize()
        .map_err(|e| format!("Cannot open workspace: {e}"))?;
    if !path.is_dir() {
        return Err("Workspace must be a directory.".to_string());
    }
    Ok(path)
}

fn build_session_preview(cfg: &Args) -> Result<SessionPreview, Box<dyn std::error::Error>> {
    let cwd = cfg
        .cwd
        .clone()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let project_context = if cfg.no_project_context {
        None
    } else {
        Some(load_or_build(
            &cwd,
            ProjectContextOptions {
                max_files: cfg.project_context_max_files,
                max_manifest_chars: cfg.project_context_max_manifest_chars,
                refresh: cfg.refresh_project_context,
                ..ProjectContextOptions::default()
            },
        )?)
    };
    let local_changes = local_change_summary(&cwd, DEFAULT_LOCAL_CHANGE_LINES);
    let context_summary = PreflightContextSummary {
        project_context_chars: project_context.as_ref().map(|ctx| ctx.text.chars().count()),
        project_context_cache_hit: project_context.as_ref().map(|ctx| ctx.cache_hit),
        local_change_handoff_chars: local_changes
            .as_ref()
            .map(|changes| changes.to_handoff_text().chars().count()),
    };
    let task_chars = cfg.task.as_deref().unwrap_or("").chars().count();
    let prompt_forecast = forecast_prompt_usage(
        task_chars,
        2,
        cfg.turns,
        cfg.sequential,
        true,
        &context_summary,
        effective_context_budget_chars(cfg.context_budget_chars),
    );
    let workspace_profile = load_profile_if_exists(&cwd, None).ok().flatten();

    Ok(SessionPreview {
        cwd,
        project_context,
        local_changes,
        context_summary,
        prompt_forecast,
        workspace_profile,
    })
}

fn session_config_json(cfg: &Args, preview: &SessionPreview) -> Value {
    let task = cfg.task.as_deref().unwrap_or("");
    let separate_executor = !cfg.drafter.eq_ignore_ascii_case(&cfg.execute)
        && !cfg.critic.eq_ignore_ascii_case(&cfg.execute);
    let executor_reuses = if separate_executor {
        None
    } else if cfg.drafter.eq_ignore_ascii_case(&cfg.execute) {
        Some("drafter")
    } else {
        Some("critic")
    };
    let (executor_model, executor_effort) = if separate_executor {
        (
            model_or_default(&cfg.execute, cfg.execute_model.clone()),
            cfg.execute_effort.clone(),
        )
    } else if cfg.drafter.eq_ignore_ascii_case(&cfg.execute) {
        (
            model_or_default(&cfg.drafter, cfg.drafter_model.clone()),
            cfg.drafter_effort.clone(),
        )
    } else {
        (
            model_or_default(&cfg.critic, cfg.critic_model.clone()),
            cfg.critic_effort.clone(),
        )
    };

    let project_context_json = project_context_status_json(
        preview.project_context.as_ref(),
        cfg.refresh_project_context,
        cfg.project_context_max_files,
        cfg.project_context_max_manifest_chars,
        effective_context_budget_chars(cfg.context_budget_chars),
    );
    let local_changes_json = local_change_summary_status_json(preview.local_changes.as_ref());
    let forecast = &preview.prompt_forecast;
    let advice = build_run_advice(RunAdviceInput {
        forecast,
        context: &preview.context_summary,
        effective_context_budget_chars: effective_context_budget_chars(cfg.context_budget_chars),
        turns: cfg.turns,
        sequential: cfg.sequential,
        include_execution: true,
        separate_executor,
    });
    let run_advice = run_advice_to_json(&advice);
    let shared_prompt_forecast = prompt_forecast_to_json(forecast, cfg.turns, cfg.sequential);
    let session_config = resolved_session_config(
        cfg,
        preview,
        executor_model.clone(),
        executor_effort.clone(),
    );
    let workspace_profile = preview
        .workspace_profile
        .as_ref()
        .map(|profile| profile_status_json(&preview.cwd, None, profile))
        .unwrap_or_else(|| {
            json!({
                "enabled": true,
                "present": false,
                "message": "no workspace profile yet; run `tales profile refresh`",
            })
        });
    let recommendation_candidates =
        vec![cfg.drafter.clone(), cfg.critic.clone(), cfg.execute.clone()];
    let tool_recommendations = recommendation_chips_json(&recommend_tools(
        &RecommendationInput {
            task: task.to_string(),
            candidates: recommendation_candidates,
            prefer_cheaper: separate_executor,
            prefer_faster: !cfg.sequential,
            needs_execution: true,
        },
        preview.workspace_profile.as_ref(),
    ));

    json!({
        "started": true,
        "build": build_info::json(),
        "task": task,
        "cwd": preview.cwd.display().to_string(),
        "mode": if cfg.demo { "demo" } else { "live" },
        "planning": if cfg.sequential { "sequential" } else { "parallel" },
        "turns": cfg.turns,
        "sandbox": cfg.sandbox,
        "session_config": session_config.to_json(),
        "context_budget_chars": cfg.context_budget_chars,
        "effective_context_budget_chars": effective_context_budget_chars(cfg.context_budget_chars),
        "context_budget_tokens_estimate": effective_context_budget_chars(cfg.context_budget_chars)
            .map(estimated_tokens),
        "project_context": project_context_json,
        "local_changes": local_changes_json,
        "prompt_forecast": {
            "planned_calls": forecast.total_prompts(),
            "base_planned_calls": forecast.total_prompts(),
            "worst_case_planned_calls": forecast.worst_case_total_prompts(),
            "planning_prompts": forecast.planning_prompts,
            "recommendation_prompts": forecast.recommendation_prompts,
            "execution_prompts": forecast.execution_prompts,
            "max_vote_repair_prompts": forecast.max_vote_repair_prompts,
            "parallel_rounds": if cfg.sequential { None } else { Some(parallel_rounds(cfg.turns, forecast.planner_count)) },
            "planner_count": forecast.planner_count,
            "known_payload_chars": forecast.known_payload_chars,
            "known_payload_tokens_estimate": estimated_tokens(forecast.known_payload_chars),
            "project_context_payload_chars": forecast.project_context_payload_chars,
            "local_change_payload_chars": forecast.local_change_payload_chars,
            "first_context_prompts": forecast.first_context_prompts,
        },
        "preflight": {
            "schema_version": 1,
            "build": build_info::json(),
            "prompt_forecast": shared_prompt_forecast,
            "run_advice": run_advice.clone(),
        },
        "run_advice": run_advice,
        "workspace_profile": workspace_profile,
        "tool_recommendations": tool_recommendations,
        "report_path": resolve_report_path(&preview.cwd, cfg.report_path.clone())
            .map(|path| path.display().to_string()),
        "report_json_path": resolve_report_path(&preview.cwd, cfg.report_json_path.clone())
            .map(|path| path.display().to_string()),
        "agents": [
            {
                "role": "drafter",
                "key": cfg.drafter,
                "model": model_or_default(&cfg.drafter, cfg.drafter_model.clone()),
                "effort": cfg.drafter_effort,
            },
            {
                "role": "critic",
                "key": cfg.critic,
                "model": model_or_default(&cfg.critic, cfg.critic_model.clone()),
                "effort": cfg.critic_effort,
            },
            {
                "role": "executor",
                "key": cfg.execute,
                "model": executor_model,
                "effort": executor_effort,
                "separate": separate_executor,
                "reuses": executor_reuses,
            },
        ],
    })
}

fn resolved_session_config(
    cfg: &Args,
    preview: &SessionPreview,
    executor_model: Option<String>,
    executor_effort: Option<String>,
) -> SessionConfig {
    let seats = vec![
        AgentSeatConfig {
            role: "drafter".into(),
            tool_key: cfg.drafter.clone(),
            model: model_or_default(&cfg.drafter, cfg.drafter_model.clone()),
            effort: cfg.drafter_effort.clone(),
            cwd_policy: CwdPolicy::Shared,
            execution_permission: ExecutionPermission::ReadOnly,
        },
        AgentSeatConfig {
            role: "critic".into(),
            tool_key: cfg.critic.clone(),
            model: model_or_default(&cfg.critic, cfg.critic_model.clone()),
            effort: cfg.critic_effort.clone(),
            cwd_policy: CwdPolicy::Shared,
            execution_permission: ExecutionPermission::ReadOnly,
        },
        AgentSeatConfig {
            role: "executor".into(),
            tool_key: cfg.execute.clone(),
            model: executor_model,
            effort: executor_effort,
            cwd_policy: CwdPolicy::Shared,
            execution_permission: ExecutionPermission::WorkspaceWrite,
        },
    ];

    SessionConfig {
        schema_version: 1,
        task: cfg.task.as_deref().unwrap_or_default().to_string(),
        cwd: preview.cwd.clone(),
        mode: SessionMode::Run,
        turns: cfg.turns,
        sequential: cfg.sequential,
        sandbox: cfg.sandbox.clone(),
        seats,
        prompt_budget: PromptBudgetConfig {
            context_budget_chars: effective_context_budget_chars(cfg.context_budget_chars),
            project_context_enabled: !cfg.no_project_context,
            refresh_project_context: cfg.refresh_project_context,
            project_context_max_files: cfg.project_context_max_files,
            project_context_max_manifest_chars: cfg.project_context_max_manifest_chars,
            local_change_handoff_lines: DEFAULT_LOCAL_CHANGE_LINES,
        },
        execution_policy: ExecutionPolicy::recommend_only(vec![cfg.execute.clone()]),
        report_paths: ReportPaths {
            markdown: resolve_report_path(&preview.cwd, cfg.report_path.clone()),
            json: resolve_report_path(&preview.cwd, cfg.report_json_path.clone()),
        },
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn start_session_once(state: Arc<AppState>) -> Result<bool, String> {
    if state
        .started
        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(false);
    }

    spawn_started_session(state).await?;
    Ok(true)
}

async fn spawn_started_session(state: Arc<AppState>) -> Result<(), String> {
    let cfg = state
        .active_cfg
        .lock()
        .await
        .clone()
        .ok_or_else(|| "No session configuration is ready.".to_string())?;
    let preview = state
        .preview
        .lock()
        .await
        .clone()
        .ok_or_else(|| "No session preview is ready.".to_string())?;
    let commands_rx = state
        .commands_rx
        .lock()
        .await
        .take()
        .ok_or_else(|| "Session command channel is already in use.".to_string())?;

    let task = cfg.task.clone().unwrap_or_default();
    let bus = state.bus.clone();
    bus.emit(OrchestratorEvent::Log {
        level: "task".to_string(),
        msg: task,
    });
    tokio::spawn(async move { run_session(bus, commands_rx, cfg, preview).await });
    Ok(())
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    state.clients.fetch_add(1, Ordering::SeqCst);
    let commands_tx = state.bus.commands();
    let (mut events, replay) = state.bus.subscribe_with_history();
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Subscribe before starting it so fast mock/demo sessions cannot outrun the
    // browser and drop the initial transcript, recommendation, or gate events.
    if state.active_cfg.lock().await.is_some() {
        if let Err(msg) = start_session_once(state.clone()).await {
            state.bus.emit(OrchestratorEvent::Fatal { msg });
        }
    }

    for ev in replay {
        let payload = event_to_json(&ev).to_string();
        if ws_tx.send(Message::Text(payload)).await.is_err() {
            release_client(state);
            return;
        }
    }

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
    release_client(state);
}

fn release_client(state: Arc<AppState>) {
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
        OrchestratorEvent::TurnComplete {
            agent,
            cost_usd,
            token_usage,
        } => {
            json!({
                "kind": "turn_done",
                "agent": agent.to_string(),
                "cost": cost_usd,
                "token_usage": token_usage.map(|usage| json!({
                    "input_tokens": usage.input_tokens,
                    "cached_input_tokens": usage.cached_input_tokens,
                    "output_tokens": usage.output_tokens,
                    "reasoning_output_tokens": usage.reasoning_output_tokens,
                    "total_tokens": usage.total_tokens,
                    "total_or_sum": usage.total_or_sum(),
                })),
            })
        }
        OrchestratorEvent::AgentExited { agent, code } => {
            json!({ "kind": "exited", "agent": agent.to_string(), "code": code })
        }
        OrchestratorEvent::PhaseChanged { phase } => json!({ "kind": "phase", "phase": phase }),
        OrchestratorEvent::RecommendationReady {
            executor,
            rationale,
            confident,
            scores,
        } => {
            json!({
                "kind": "recommendation",
                "executor": executor,
                "rationale": rationale,
                "confident": confident,
                "scores": scores.iter().map(|(executor, score)| json!({
                    "executor": executor,
                    "score": score,
                })).collect::<Vec<_>>(),
            })
        }
        OrchestratorEvent::ExecutionPacket {
            executor,
            text,
            included_in_prompt,
        } => {
            json!({
                "kind": "execution_packet",
                "executor": executor,
                "text": text,
                "included_in_prompt": included_in_prompt
            })
        }
        OrchestratorEvent::SessionReport { markdown, summary } => {
            json!({ "kind": "session_report", "markdown": markdown, "summary": summary })
        }
        OrchestratorEvent::AwaitingConfirmation { prompt } => {
            json!({ "kind": "awaiting", "prompt": prompt })
        }
        OrchestratorEvent::Log { level, msg } => {
            json!({ "kind": "log", "level": level, "msg": msg })
        }
        OrchestratorEvent::Fatal { msg } => json!({ "kind": "fatal", "msg": msg }),
    }
}

fn parse_command(text: &str) -> Option<UserCommand> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v.get("kind").and_then(Value::as_str)? {
        "say" => Some(UserCommand::InjectNote {
            agent: Uuid::nil(),
            text: v
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            attachments: Vec::new(),
        }),
        "confirm" => Some(UserCommand::ConfirmExecution {
            executor: v
                .get("executor")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        "reject" => Some(UserCommand::Reject),
        "shutdown" => Some(UserCommand::Shutdown),
        _ => None,
    }
}

/// Claude is restricted to file tools so that, if chosen as executor, it can't
/// stall on an unapproved Bash call in headless mode.
fn ctx(
    agent: Uuid,
    label: &str,
    cwd: &std::path::Path,
    model: Option<String>,
    effort: Option<String>,
    sandbox: &str,
    executor: &str,
) -> SpawnCtx {
    let allowed_tools =
        if label.eq_ignore_ascii_case("claude") && label.eq_ignore_ascii_case(executor) {
            Some(vec![
                "Write".into(),
                "Edit".into(),
                "MultiEdit".into(),
                "Read".into(),
            ])
        } else {
            None
        };
    SpawnCtx {
        agent,
        label: label.to_string(),
        cwd: cwd.to_path_buf(),
        model: model_or_default(label, model),
        effort,
        permission_mode: "acceptEdits".to_string(),
        sandbox: sandbox.to_string(),
        allowed_tools,
    }
}

fn demo_planning_turns(max_turns: usize, sequential: bool, planners: usize) -> usize {
    let planners = planners.max(1);
    if sequential {
        max_turns.div_ceil(planners).max(1)
    } else {
        (max_turns / planners).clamp(2, 6)
    }
}

fn demo_planning_responses(label: &str, role: Role, count: usize) -> Vec<String> {
    (0..count)
        .map(|idx| match role {
            Role::Drafter => format!(
                "Demo draft {} from {label}:\n- inspect the existing structure\n- make the smallest coherent change\n- verify with focused tests",
                idx + 1
            ),
            Role::Critic => format!(
                "Demo critique {} from {label}:\n- check edge cases before execution\n- keep token context compact\n- require a clear verification step",
                idx + 1
            ),
            _ => format!("Demo planning response {} from {label}.", idx + 1),
        })
        .collect()
}

fn demo_planner_responses(
    label: &str,
    role: Role,
    planning_turns: usize,
    execute: &str,
) -> Vec<String> {
    let mut responses = demo_planning_responses(label, role, planning_turns);
    responses.push(format!(
        r#"{{"recommended_executor":"{execute}","confidence":0.86,"rationale":"demo vote from {label}: {execute} is selected for the implementation pass"}}"#
    ));
    responses.push(format!(
        "Demo execution by {label}:\n- consumed the compact execution packet\n- applied the agreed plan in mock mode\n- no files were changed"
    ));
    responses
}

fn demo_executor_responses(label: &str) -> Vec<String> {
    vec![format!(
        "Demo execution by {label}:\n- received the compact execution packet\n- completed the mock implementation pass\n- no files were changed"
    )]
}

async fn run_session(
    bus: EventBus,
    mut commands_rx: mpsc::Receiver<UserCommand>,
    cfg: Arc<Args>,
    preview: Arc<SessionPreview>,
) {
    let mut orch = Orchestrator::new(bus.clone());
    orch.set_parallel_rounds(!cfg.sequential);
    if let Some(secs) = cfg.turn_timeout_secs {
        orch.set_turn_timeout(Duration::from_secs(secs.max(1)));
    }
    if let Some(chars) = cfg.context_budget_chars {
        let budget = if chars == 0 { None } else { Some(chars) };
        orch.set_context_budget_chars(budget);
    }

    let result = async {
        let cwd = preview.cwd.clone();
        let report_writer = spawn_report_writer(
            &bus,
            resolve_report_path(&cwd, cfg.report_path.clone()),
            resolve_report_path(&cwd, cfg.report_json_path.clone()),
        );
        let separate_executor = !cfg.drafter.eq_ignore_ascii_case(&cfg.execute)
            && !cfg.critic.eq_ignore_ascii_case(&cfg.execute);
        let mut roster = vec![cfg.drafter.clone(), cfg.critic.clone()];
        if separate_executor {
            roster.push(cfg.execute.clone());
        }
        validate_roster(&roster)?;
        if !cfg.demo {
            validate_tool_readiness(&roster)?;
            validate_effort(&cfg.drafter, cfg.drafter_effort.as_deref())?;
            validate_effort(&cfg.critic, cfg.critic_effort.as_deref())?;
            if separate_executor {
                validate_effort(&cfg.execute, cfg.execute_effort.as_deref())?;
            }
        }

        if let Some(ctx) = &preview.project_context {
            bus.emit(OrchestratorEvent::Log {
                level: "info".to_string(),
                msg: format!(
                    "project context {} ({}, max_files={}, max_manifest_chars={})",
                    if ctx.cache_hit { "cache hit" } else { "cached" },
                    ctx.cache_path.display(),
                    cfg.project_context_max_files,
                    cfg.project_context_max_manifest_chars
                ),
            });
            let report = ProjectContextReport {
                cache_hit: ctx.cache_hit,
                cache_path: ctx.cache_path.display().to_string(),
                chars: ctx.text.chars().count(),
                max_files: cfg.project_context_max_files,
                max_manifest_chars: cfg.project_context_max_manifest_chars,
            };
            orch.set_project_context(Some(ctx.text.clone()));
            orch.set_project_context_report(Some(report));
        }

        if let Some(changes) = &preview.local_changes {
            bus.emit(OrchestratorEvent::Log {
                level: "info".to_string(),
                msg: format!("local changes {}", changes.summary_line()),
            });
            orch.set_local_changes(Some(changes.clone()));
        }

        if cfg.demo {
            let planning_turns = demo_planning_turns(cfg.turns, cfg.sequential, 2);
            let drafter = MockAdapter::new(demo_planner_responses(
                &cfg.drafter,
                Role::Drafter,
                planning_turns,
                &cfg.execute,
            ));
            let critic = MockAdapter::new(demo_planner_responses(
                &cfg.critic,
                Role::Critic,
                planning_turns,
                &cfg.execute,
            ));
            orch.add_agent(
                Box::new(drafter),
                ctx(
                    Uuid::new_v4(),
                    &cfg.drafter,
                    &cwd,
                    None,
                    None,
                    &cfg.sandbox,
                    &cfg.execute,
                ),
                Role::Drafter,
            )
            .await?;
            orch.add_agent(
                Box::new(critic),
                ctx(
                    Uuid::new_v4(),
                    &cfg.critic,
                    &cwd,
                    None,
                    None,
                    &cfg.sandbox,
                    &cfg.execute,
                ),
                Role::Critic,
            )
            .await?;
            if separate_executor {
                orch.add_agent(
                    Box::new(MockAdapter::new(demo_executor_responses(&cfg.execute))),
                    ctx(
                        Uuid::new_v4(),
                        &cfg.execute,
                        &cwd,
                        None,
                        None,
                        &cfg.sandbox,
                        &cfg.execute,
                    ),
                    Role::Executor,
                )
                .await?;
            }
        } else {
            orch.add_agent(
                make_adapter(&cfg.drafter)?,
                ctx(
                    Uuid::new_v4(),
                    &cfg.drafter,
                    &cwd,
                    cfg.drafter_model.clone(),
                    cfg.drafter_effort.clone(),
                    &cfg.sandbox,
                    &cfg.execute,
                ),
                Role::Drafter,
            )
            .await?;
            orch.add_agent(
                make_adapter(&cfg.critic)?,
                ctx(
                    Uuid::new_v4(),
                    &cfg.critic,
                    &cwd,
                    cfg.critic_model.clone(),
                    cfg.critic_effort.clone(),
                    &cfg.sandbox,
                    &cfg.execute,
                ),
                Role::Critic,
            )
            .await?;
            if separate_executor {
                orch.add_agent(
                    make_adapter(&cfg.execute)?,
                    ctx(
                        Uuid::new_v4(),
                        &cfg.execute,
                        &cwd,
                        cfg.execute_model.clone(),
                        cfg.execute_effort.clone(),
                        &cfg.sandbox,
                        &cfg.execute,
                    ),
                    Role::Executor,
                )
                .await?;
            }
        }

        let task = cfg.task.as_deref().unwrap_or("");
        let outcome = orch
            .run_interactive(task, cfg.turns, &mut commands_rx)
            .await;
        if let Some(handle) = report_writer {
            let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
        }
        outcome
    }
    .await;

    match result {
        Ok(outcome) => {
            update_workspace_profile_after_web_run(&preview.cwd, &cfg, &outcome);
            bus.emit(OrchestratorEvent::Log {
                level: "done".to_string(),
                msg: format!("{outcome:?}"),
            });
        }
        Err(e) => bus.emit(OrchestratorEvent::Fatal { msg: e.to_string() }),
    }
    orch.shutdown().await;
}

fn update_workspace_profile_after_web_run(cwd: &Path, cfg: &Args, outcome: &RunOutcome) {
    let Ok(mut profile) = load_profile(cwd, None) else {
        return;
    };
    let report_path = resolve_report_path(cwd, cfg.report_path.clone());
    profile.apply_update(ProfileUpdate {
        command: Some("tales-web".into()),
        report_path: report_path.as_ref().map(|path| path.display().to_string()),
        run: Some(run_record_from_report(
            cfg.task
                .as_deref()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect::<String>(),
            Some(cfg.execute.clone()),
            matches!(outcome, RunOutcome::Executed { .. }),
            None,
            None,
            None,
            None,
            None,
            report_path.map(|path| path.display().to_string()),
        )),
        ..ProfileUpdate::default()
    });
    let _ = save_profile(cwd, None, &profile);
}

fn spawn_report_writer(
    bus: &EventBus,
    markdown_path: Option<PathBuf>,
    json_path: Option<PathBuf>,
) -> Option<tokio::task::JoinHandle<()>> {
    if markdown_path.is_none() && json_path.is_none() {
        return None;
    }
    let mut events = bus.subscribe();
    let bus = bus.clone();
    Some(tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            if let OrchestratorEvent::SessionReport { markdown, summary } = ev {
                if let Some(path) = markdown_path.as_deref() {
                    match write_report_file(path, &markdown) {
                        Ok(()) => bus.emit(OrchestratorEvent::Log {
                            level: "info".to_string(),
                            msg: format!("session report written to {}", path.display()),
                        }),
                        Err(e) => bus.emit(OrchestratorEvent::Log {
                            level: "warn".to_string(),
                            msg: format!(
                                "failed to write session report to {}: {e}",
                                path.display()
                            ),
                        }),
                    }
                }
                if let Some(path) = json_path.as_deref() {
                    match serde_json::to_string_pretty(&summary)
                        .map_err(std::io::Error::other)
                        .and_then(|text| write_report_file(path, &text))
                    {
                        Ok(()) => bus.emit(OrchestratorEvent::Log {
                            level: "info".to_string(),
                            msg: format!("session summary JSON written to {}", path.display()),
                        }),
                        Err(e) => bus.emit(OrchestratorEvent::Log {
                            level: "warn".to_string(),
                            msg: format!(
                                "failed to write session summary JSON to {}: {e}",
                                path.display()
                            ),
                        }),
                    }
                }
                break;
            }
        }
    }))
}

fn write_report_file(path: &Path, markdown: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, markdown)
}

fn resolve_report_path(cwd: &Path, path: Option<PathBuf>) -> Option<PathBuf> {
    path.map(|p| if p.is_absolute() { p } else { cwd.join(p) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tales_core::agent::KNOWN_TOOLS;

    #[test]
    fn tool_status_lists_known_tools_without_running_them() {
        let status = tool_status_json();
        let tools = status["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), KNOWN_TOOLS.len());
        let claude = tools
            .iter()
            .find(|tool| tool["key"] == "claude")
            .expect("claude row");
        assert_eq!(claude["kind"], "claude-stream");
        assert_eq!(claude["caps"]["resume"], true);
        assert!(claude["install"].as_str().unwrap_or("").contains("claude"));
    }

    #[test]
    fn report_paths_resolve_relative_to_cwd() {
        let cwd = PathBuf::from("/tmp/tales-web-cwd");
        assert_eq!(
            resolve_report_path(&cwd, Some(PathBuf::from(".tales/report.md"))).unwrap(),
            PathBuf::from("/tmp/tales-web-cwd/.tales/report.md")
        );
        assert_eq!(
            resolve_report_path(&cwd, Some(PathBuf::from("/tmp/report.md"))).unwrap(),
            PathBuf::from("/tmp/report.md")
        );
    }

    #[test]
    fn session_report_event_json_includes_structured_summary() {
        let value = event_to_json(&OrchestratorEvent::SessionReport {
            markdown: "# report".into(),
            summary: json!({
                "kind": "tales_session_summary",
                "outcome": { "status": "executed" },
            }),
        });

        assert_eq!(value["kind"], "session_report");
        assert_eq!(value["markdown"], "# report");
        assert_eq!(value["summary"]["kind"], "tales_session_summary");
        assert_eq!(value["summary"]["outcome"]["status"], "executed");
    }

    #[test]
    fn session_config_reports_models_efforts_and_paths() {
        let cfg = Args {
            task: Some("plan it".to_string()),
            drafter: "claude".to_string(),
            critic: "codex".to_string(),
            drafter_model: Some("opus".to_string()),
            critic_model: Some("gpt-5-codex".to_string()),
            execute: "codex".to_string(),
            execute_model: None,
            drafter_effort: Some("max".to_string()),
            critic_effort: Some("high".to_string()),
            execute_effort: None,
            turns: 6,
            cwd: Some("/tmp/tales-web-cwd".to_string()),
            sandbox: "workspace-write".to_string(),
            demo: false,
            sequential: false,
            turn_timeout_secs: Some(900),
            context_budget_chars: Some(12_000),
            no_project_context: false,
            refresh_project_context: true,
            project_context_max_files: 180,
            project_context_max_manifest_chars: 900,
            report_path: Some(PathBuf::from(".tales/report.md")),
            report_json_path: Some(PathBuf::from(".tales/report.json")),
            port: 7878,
            no_open: true,
        };
        let context_summary = PreflightContextSummary {
            project_context_chars: Some(2_048),
            project_context_cache_hit: Some(true),
            local_change_handoff_chars: Some(256),
        };
        let preview = SessionPreview {
            cwd: PathBuf::from("/tmp/tales-web-cwd"),
            project_context: Some(ProjectContext {
                text: "x".repeat(2_048),
                cache_hit: true,
                cache_path: PathBuf::from("/tmp/tales-context.txt"),
            }),
            local_changes: None,
            prompt_forecast: forecast_prompt_usage(
                cfg.task.as_deref().unwrap_or("").chars().count(),
                2,
                cfg.turns,
                cfg.sequential,
                true,
                &context_summary,
                effective_context_budget_chars(cfg.context_budget_chars),
            ),
            context_summary,
            workspace_profile: None,
        };
        let session = session_config_json(&cfg, &preview);

        assert_eq!(session["planning"], "parallel");
        assert_eq!(session["context_budget_chars"], 12_000);
        assert_eq!(session["effective_context_budget_chars"], 12_000);
        assert_eq!(session["context_budget_tokens_estimate"], 3_000);
        assert_eq!(
            session["report_path"],
            "/tmp/tales-web-cwd/.tales/report.md"
        );
        assert_eq!(
            session["report_json_path"],
            "/tmp/tales-web-cwd/.tales/report.json"
        );
        assert_eq!(session["project_context"]["refresh"], true);
        assert_eq!(session["project_context"]["cache_hit"], true);
        assert_eq!(session["project_context"]["tokens_estimate"], 512);
        assert_eq!(
            session["project_context"]["max_manifest_tokens_estimate"],
            225
        );
        assert_eq!(session["prompt_forecast"]["planned_calls"], 9);
        assert_eq!(session["prompt_forecast"]["base_planned_calls"], 9);
        assert_eq!(session["prompt_forecast"]["worst_case_planned_calls"], 11);
        assert_eq!(session["prompt_forecast"]["planning_prompts"], 6);
        assert_eq!(
            session["prompt_forecast"]["known_payload_tokens_estimate"],
            1104
        );
        assert_eq!(session["preflight"]["schema_version"], 1);
        assert_eq!(
            session["preflight"]["prompt_forecast"]["planned_calls"]["total"],
            9
        );
        assert_eq!(
            session["preflight"]["prompt_forecast"]["planned_calls"]["worst_case"],
            11
        );
        assert_eq!(
            session["preflight"]["prompt_forecast"]["planning_waits"]["mode"],
            "parallel"
        );
        assert!(session["preflight"]["run_advice"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["key"] == "planning_shape"));
        assert_eq!(session["agents"][0]["model"], "opus");
        assert_eq!(session["agents"][0]["effort"], "max");
        assert_eq!(session["agents"][2]["reuses"], "critic");
        assert_eq!(session["agents"][2]["effort"], "high");
    }
}
