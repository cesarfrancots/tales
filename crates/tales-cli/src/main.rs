//! `tales` — command-line frontend over the orchestration core.
//!
//! Subcommands:
//! - `term` — open the Warp-style Tales terminal workspace.
//! - `solo` — drive a single agent for one turn (claude or codex).
//! - `discuss` — a live drafter/critic discussion between two agents.
//! - `run` — the full pipeline (discuss → recommend → execute), scriptable.
//!
//! A thin frontend: it subscribes to `OrchestratorEvent`s and sends commands.
//! `tales-tui` (terminal) and `tales-web` (browser) are richer frontends over
//! the same bus.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use tales_core::agent::mock::MockAdapter;
use tales_core::agent::{
    adapter_kind_label, agent_caps_json, bin_path, make_adapter, tool_info, tool_info_status_json,
    tool_roster_advice_json, validate_effort, validate_roster, validate_tool_readiness, AgentCaps,
    AgentCommand, AgentEvent, Attachment, SpawnCtx, KNOWN_TOOLS,
};
use tales_core::build_info;
use tales_core::bus::EventBus;
use tales_core::conductor::Role;
use tales_core::eval_harness::{
    compare_results, default_scenarios, run_mock_eval, save_eval_report, EvalScenario,
};
use tales_core::event::{OrchestratorEvent, UserCommand};
use tales_core::orchestrator::{
    Orchestrator, ProjectContextReport, RunOutcome, DEFAULT_CONTEXT_BUDGET_CHARS,
};
use tales_core::project_context::{
    load_or_build, local_change_summary, local_change_summary_status_json,
    project_context_status_json, ProjectContextOptions, DEFAULT_LOCAL_CHANGE_LINES,
};
use tales_core::prompt_forecast::{
    build_run_advice, forecast_prompt_usage, parallel_rounds, prompt_forecast_to_json,
    run_advice_to_json, PreflightContextSummary, RunAdviceInput,
};
use tales_core::session::{
    AgentSeatConfig, CwdPolicy, ExecutionPermission, ExecutionPolicy, PromptBudgetConfig,
    ReportPaths, SessionConfig, SessionMode,
};
use tales_core::tool_recommendation::{
    recommend_tools, recommendation_chips_json, RecommendationInput,
};
use tales_core::workspace_profile::{
    clear_profile, load_profile, load_profile_if_exists, profile_status_json, refresh_profile,
    run_record_from_report, save_profile, ProfileUpdate,
};
use tales_core::worktree::{MergeOutcome, WorktreeManager};
use tales_core::TokenUsage;

const TALES_DIR: &str = ".tales";
const RUNS_DIR: &str = "runs";

#[derive(Parser, Debug)]
#[command(
    name = "tales",
    version = build_info::version(),
    long_version = build_info::long_version(),
    about = "Multi-agent AI coding orchestrator",
    long_about = "Multi-agent AI coding orchestrator.\n\nRun `tales` with no subcommand to open the terminal workspace — Tales is the \
                  default pane, with sibling panes for shells, Codex, Claude Code, and Open Code. \
                  The subcommands below are the scriptable, non-interactive counterparts."
)]
struct Cli {
    /// With no subcommand, `tales` opens the terminal workspace (the headline
    /// experience). The subcommands are for scripting or explicit launching.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the Warp-style Tales terminal workspace.
    Term,
    /// Print the Tales command reference.
    Commands,
    /// List or inspect saved `.tales/runs` artifacts.
    Recover {
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Inspect the newest saved run.
        #[arg(long)]
        latest: bool,
        /// Print the saved plan.md for the selected run.
        #[arg(long)]
        print: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Run id, run directory name, or path to a run artifact directory.
        run: Option<String>,
    },
    /// Preflight local model CLIs and project context without model calls.
    Doctor {
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Agent/tool key to require. Repeatable. Defaults to claude + codex.
        #[arg(long = "agent")]
        agent: Vec<String>,
        /// Inventory every known tool without failing on missing optional tools.
        #[arg(long)]
        all: bool,
        /// Disable the project context cache check.
        #[arg(long)]
        no_project_context: bool,
        /// Rebuild the cached project context during the check.
        #[arg(long)]
        refresh_project_context: bool,
        /// Maximum files to index into the compact file map.
        #[arg(long, default_value_t = 220)]
        max_files: usize,
        /// Maximum characters copied from each important manifest.
        #[arg(long, default_value_t = 1200)]
        max_manifest_chars: usize,
        /// Override the project context cache directory.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Emit machine-readable doctor output for launchers and agents.
        #[arg(long)]
        json: bool,
    },
    /// Build, refresh, or inspect the cached project context without model calls.
    Context {
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Rebuild the cached project context before printing metadata.
        #[arg(long)]
        refresh: bool,
        /// Maximum files to index into the compact file map.
        #[arg(long, default_value_t = 220)]
        max_files: usize,
        /// Maximum characters copied from each important manifest.
        #[arg(long, default_value_t = 1200)]
        max_manifest_chars: usize,
        /// Override the cache directory for this command.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Print the full context instead of a bounded preview.
        #[arg(long)]
        print: bool,
        /// Emit machine-readable cache metadata. Pair with --print to include text.
        #[arg(long)]
        json: bool,
        /// Write the generated context text to this path.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Drive a single agent for one turn.
    Solo {
        /// The prompt to send.
        prompt: String,
        /// Which agent/tool key to run. See `tales doctor --all` for known tools.
        #[arg(long, default_value = "claude")]
        agent: String,
        /// Model alias/id (optional).
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort for agents that support it.
        #[arg(long)]
        effort: Option<String>,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Codex sandbox policy.
        #[arg(long, default_value = "workspace-write")]
        sandbox: String,
        /// Attach an image/PDF (repeatable): --image a.png --image b.pdf
        #[arg(long)]
        image: Vec<String>,
    },
    /// Full pipeline, non-interactive: discuss → recommend → auto-confirm the
    /// chosen executor → execute. The scriptable counterpart to the live TUI.
    Run {
        /// The task to plan and execute.
        prompt: String,
        /// Drafter tool key. See `tales doctor --all` for known tools.
        #[arg(long, default_value = "claude")]
        drafter: String,
        /// Critic tool key. See `tales doctor --all` for known tools.
        #[arg(long, default_value = "codex")]
        critic: String,
        /// Which agent executes the agreed plan (auto-confirmed). May be a
        /// participant, or a different tool entirely (tiered execution).
        #[arg(long, default_value = "claude")]
        execute: String,
        /// Model for the drafter.
        #[arg(long)]
        drafter_model: Option<String>,
        /// Model for the critic.
        #[arg(long)]
        critic_model: Option<String>,
        /// Model for the executor when it's a *separate* tool from the planners
        /// — the cheap/fast model that implements what the strong planners agreed
        /// on. Only valid when `--execute` isn't one of `--drafter`/`--critic`.
        #[arg(long)]
        execute_model: Option<String>,
        /// Reasoning effort for the drafter (e.g. Codex low|medium|high).
        #[arg(long)]
        drafter_effort: Option<String>,
        /// Reasoning effort for the critic (e.g. Codex low|medium|high).
        #[arg(long)]
        critic_effort: Option<String>,
        /// Reasoning effort for a separate executor (e.g. Codex low|medium|high).
        #[arg(long)]
        execute_effort: Option<String>,
        /// Total planner turns before recommendation.
        #[arg(long, default_value_t = 4)]
        turns: usize,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Sandbox policy passed to tools that support one.
        #[arg(long, default_value = "workspace-write")]
        sandbox: String,
        /// Run the executor in its own git worktree, then merge the result back
        /// into the current branch (requires a git repo).
        #[arg(long)]
        worktree: bool,
        /// Plan sequentially (drafter→critic ping-pong) instead of the default
        /// parallel rounds (both planners draft concurrently, then synthesize) —
        /// the parallel path is faster and cuts per-turn re-sent context.
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
        /// Use scripted mock agents. No model calls, useful for rehearsing flows.
        #[arg(long)]
        demo: bool,
        /// Validate and print the run setup without spawning agents or model calls.
        #[arg(long)]
        dry_run: bool,
        /// Emit the dry-run preflight as JSON for other tools. Requires --dry-run.
        #[arg(long)]
        json: bool,
        /// Write the final local session report markdown to this path.
        #[arg(long)]
        report_path: Option<PathBuf>,
        /// Write the final structured session summary JSON to this path.
        #[arg(long)]
        report_json_path: Option<PathBuf>,
    },
    /// Run a live drafter/critic discussion between two agents.
    Discuss {
        /// The task the agents discuss.
        prompt: String,
        /// Drafter tool key. See `tales doctor --all` for known tools.
        #[arg(long, default_value = "claude")]
        drafter: String,
        /// Critic tool key. See `tales doctor --all` for known tools.
        #[arg(long, default_value = "codex")]
        critic: String,
        /// Total number of turns (alternating drafter/critic).
        #[arg(long, default_value_t = 4)]
        turns: usize,
        /// Model for the drafter (optional).
        #[arg(long)]
        drafter_model: Option<String>,
        /// Reasoning effort for the drafter.
        #[arg(long)]
        drafter_effort: Option<String>,
        /// Model for the critic (optional).
        #[arg(long)]
        critic_model: Option<String>,
        /// Reasoning effort for the critic.
        #[arg(long)]
        critic_effort: Option<String>,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Codex sandbox policy.
        #[arg(long, default_value = "read-only")]
        sandbox: String,
        /// Plan sequentially instead of the default parallel rounds.
        #[arg(long)]
        sequential: bool,
        /// Per-agent turn timeout in seconds. Useful for high-effort models.
        #[arg(long)]
        turn_timeout_secs: Option<u64>,
        /// Max transcript chars pasted into stateless prompts. 0 disables.
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
        /// Use scripted mock agents. No model calls, useful for rehearsing flows.
        #[arg(long)]
        demo: bool,
        /// Validate and print the discussion setup without spawning agents or model calls.
        #[arg(long)]
        dry_run: bool,
        /// Emit the dry-run preflight as JSON for other tools. Requires --dry-run.
        #[arg(long)]
        json: bool,
        /// Write the local discussion report markdown to this path.
        #[arg(long)]
        report_path: Option<PathBuf>,
        /// Write the local structured discussion summary JSON to this path.
        #[arg(long)]
        report_json_path: Option<PathBuf>,
    },
    /// Open the terminal workspace in a NEW window — for harnesses with no TTY
    /// (the `/tales` command shells out to this). Deterministic: no AI reasoning
    /// is needed to launch. macOS only.
    Open {
        /// Pre-connect a tool in the new window (repeatable): --connect claude.
        #[arg(long = "connect")]
        connect: Vec<String>,
        /// Optional task to pre-fill the Tales prompt.
        task: Option<String>,
    },
    /// Inspect, refresh, or clear local workspace metadata memory.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Run deterministic Tales collaboration evaluations without model calls.
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ProfileCommand {
    /// Show the local metadata profile for a workspace.
    Show {
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Override the workspace profile cache directory.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Rebuild detected commands, conventions, and installed tool preferences.
    Refresh {
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Override the workspace profile cache directory.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete the local metadata profile for a workspace.
    Clear {
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Override the workspace profile cache directory.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum EvalCommand {
    /// Run one deterministic mock scenario.
    Run {
        /// Task label/prompt used for the eval scenario.
        #[arg(default_value = "fix lint errors")]
        task: String,
        /// Scenario name: solo_strong, debate_parallel, or tiered_parallel.
        #[arg(long, default_value = "debate_parallel")]
        scenario: String,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Write the JSON report to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compare solo, debate, and tiered deterministic scenarios.
    Compare {
        /// Task label/prompt used for comparison scenarios.
        #[arg(default_value = "fix lint errors")]
        task: String,
        /// Working directory (default: current).
        #[arg(long)]
        cwd: Option<String>,
        /// Write the JSON report to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Render a saved eval JSON report.
    Report {
        /// Saved eval JSON report path.
        input: PathBuf,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Resolve the `tales-tui` binary: next to this exe first, then `PATH`.
fn tui_path() -> std::ffi::OsString {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("tales-tui")))
        .filter(|p| p.exists())
        .map(|p| p.into_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("tales-tui"))
}

/// `tales` with no subcommand IS the terminal workspace. The live UI lives in
/// the sibling `tales-tui` binary (it pulls in ratatui/crossterm, which the
/// scriptable CLI doesn't), so we hand off to it — replacing this process on
/// unix so there's no extra shell in the tree.
fn launch_tui() -> ! {
    let target = tui_path();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&target).exec();
        eprintln!(
            "tales: could not launch the terminal workspace ({err}).\n\
             Build it with `cargo build --release` and put target/release on PATH."
        );
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&target).status() {
            Ok(s) => std::process::exit(s.code().unwrap_or(0)),
            Err(e) => {
                eprintln!(
                    "tales: could not launch the terminal workspace ({e}).\n\
                     Build it with `cargo build --release` and put target/release on PATH."
                );
                std::process::exit(127);
            }
        }
    }
}

/// Single-quote a string for safe embedding in the generated bash `.command`
/// (handles spaces, quotes, `$`, etc. — everything inside `'...'` is literal).
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''"); // close, escaped quote, reopen
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Build the contents of the `.command` launcher: cd to the project, then exec
/// `tales-tui` with the pre-connect/pre-fill flags. An empty/whitespace task is
/// omitted so the prompt screen opens blank.
fn build_open_script(tui: &str, cwd: &str, connect: &[String], task: Option<&str>) -> String {
    let mut s = String::from("#!/bin/bash\n");
    s.push_str(&format!("cd {} || exit 1\n", sh_quote(cwd)));
    s.push_str("exec ");
    s.push_str(&sh_quote(tui));
    for c in connect {
        s.push_str(" --connect ");
        s.push_str(&sh_quote(c));
    }
    if let Some(t) = task {
        let t = t.trim();
        if !t.is_empty() {
            s.push_str(" --prefill ");
            s.push_str(&sh_quote(t));
        }
    }
    s.push('\n');
    s
}

/// `tales open` — write a one-shot `.command` and hand it to macOS `open`, which
/// launches it in a new Terminal window. Pure mechanics; no AI reasoning needed.
fn run_open(connect: Vec<String>, task: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let tui = tui_path();
    let cwd = std::env::current_dir()?;
    let script = build_open_script(
        &tui.to_string_lossy(),
        &cwd.to_string_lossy(),
        &connect,
        task.as_deref(),
    );

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("tales-run.{}.{nanos}.command", std::process::id()));
        std::fs::write(&path, script)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        let status = std::process::Command::new("open").arg(&path).status()?;
        if !status.success() {
            return Err(format!("`open` failed ({status})").into());
        }
        println!("Tales terminal opening in a new window.");
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = script;
        eprintln!(
            "`tales open` opens a new macOS Terminal window. On this OS, run `tales` \
             (or `tales-tui`) directly in a terminal instead."
        );
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn run_context(
    cwd: Option<String>,
    refresh: bool,
    max_files: usize,
    max_manifest_chars: usize,
    cache_dir: Option<PathBuf>,
    print: bool,
    json_output: bool,
    output: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let ctx = load_or_build(
        &cwd,
        ProjectContextOptions {
            max_files,
            max_manifest_chars,
            refresh,
            cache_dir,
        },
    )?;
    let output_path = resolve_report_path(&cwd, output);
    if let Some(path) = output_path.as_deref() {
        write_text_file(path, &ctx.text)?;
    }
    if json_output {
        let mut project_context = project_context_status_json(
            Some(&ctx),
            refresh,
            max_files,
            max_manifest_chars,
            Some(DEFAULT_CONTEXT_BUDGET_CHARS),
        );
        if let Some(object) = project_context.as_object_mut() {
            object.insert("text_included".into(), json!(print));
            if print {
                object.insert("text".into(), json!(ctx.text.as_str()));
            }
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "tales_project_context",
                "schema_version": 1,
                "build": build_info::json(),
                "cwd": cwd.display().to_string(),
                "project_context": project_context,
                "output_path": output_path.map(|path| path.display().to_string()),
                "status": "ok",
            }))?
        );
        return Ok(());
    }

    let chars = ctx.text.chars().count();
    println!(
        "project context: {}",
        if ctx.cache_hit {
            "cache hit"
        } else {
            "rebuilt"
        }
    );
    println!("cache: {}", ctx.cache_path.display());
    println!("chars: {chars}");
    println!("tokens~: {}", estimated_tokens(chars));
    println!(
        "default budget fit: {}",
        context_fit_label(chars, Some(DEFAULT_CONTEXT_BUDGET_CHARS))
    );

    if let Some(path) = output_path {
        println!("written: {}", path.display());
    }

    if print {
        println!("\n{}", ctx.text);
    } else {
        println!("\n{}", preview_chars(&ctx.text, 2200));
    }
    Ok(())
}

fn doctor_targets(all: bool, agents: &[String]) -> Result<(Vec<String>, bool), String> {
    if all && !agents.is_empty() {
        return Err("--all cannot be combined with --agent".to_string());
    }
    if all {
        return Ok((
            KNOWN_TOOLS
                .iter()
                .map(|tool| tool.key.to_string())
                .collect(),
            false,
        ));
    }
    if agents.is_empty() {
        return Ok((vec!["claude".to_string(), "codex".to_string()], true));
    }
    Ok((agents.to_vec(), true))
}

fn unique_tool_keys(keys: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        if !out
            .iter()
            .any(|seen: &String| seen.eq_ignore_ascii_case(key))
        {
            out.push(key.clone());
        }
    }
    out
}

fn format_caps(caps: AgentCaps) -> String {
    format!(
        "resume={}, midturn={}, structured={}, mcp={}",
        caps.resumable, caps.midturn_injection, caps.structured_output, caps.mcp
    )
}

fn doctor_tool_json(key: &str, strict_tools: bool, failures: &mut Vec<String>) -> Value {
    let Some(info) = tool_info(key) else {
        let message = format!("unknown tool '{key}'");
        failures.push(message.clone());
        return json!({
            "key": key,
            "known": false,
            "installed": false,
            "blocking": strict_tools,
            "error": message,
        });
    };

    let path = bin_path(info.bin);
    let installed = path.is_some();
    if strict_tools && !installed {
        failures.push(format!("{} CLI missing from PATH", info.pretty));
    }
    let mut value = tool_info_status_json(info);
    if let Some(object) = value.as_object_mut() {
        object.insert("blocking".into(), json!(strict_tools && !installed));
    }
    value
}

fn doctor_project_context_json(
    cwd: &Path,
    disabled: bool,
    refresh: bool,
    max_files: usize,
    max_manifest_chars: usize,
    cache_dir: Option<PathBuf>,
) -> Result<Value, Box<dyn std::error::Error>> {
    if disabled {
        return Ok(project_context_status_json(
            None,
            refresh,
            max_files,
            max_manifest_chars,
            Some(DEFAULT_CONTEXT_BUDGET_CHARS),
        ));
    }

    let ctx = load_or_build(
        cwd,
        ProjectContextOptions {
            max_files,
            max_manifest_chars,
            refresh,
            cache_dir,
        },
    )?;
    Ok(project_context_status_json(
        Some(&ctx),
        refresh,
        max_files,
        max_manifest_chars,
        Some(DEFAULT_CONTEXT_BUDGET_CHARS),
    ))
}

#[allow(clippy::too_many_arguments)]
fn doctor_json_report(
    cwd: &Path,
    targets: &[String],
    strict_tools: bool,
    no_project_context: bool,
    refresh_project_context: bool,
    max_files: usize,
    max_manifest_chars: usize,
    cache_dir: Option<PathBuf>,
) -> Result<(Value, Vec<String>), Box<dyn std::error::Error>> {
    let mut failures = Vec::new();
    let roster_check = match validate_roster(targets) {
        Ok(()) => json!({ "ok": true, "error": null }),
        Err(err) => {
            let message = err.to_string();
            failures.push(message.clone());
            json!({ "ok": false, "error": message })
        }
    };
    let tools = unique_tool_keys(targets)
        .iter()
        .map(|key| doctor_tool_json(key, strict_tools, &mut failures))
        .collect::<Vec<_>>();
    let project_context = doctor_project_context_json(
        cwd,
        no_project_context,
        refresh_project_context,
        max_files,
        max_manifest_chars,
        cache_dir,
    )?;
    let status = if failures.is_empty() {
        "ok"
    } else {
        "attention_required"
    };

    Ok((
        json!({
            "kind": "tales_doctor",
            "schema_version": 1,
            "build": build_info::json(),
            "cwd": cwd.display().to_string(),
            "mode": if strict_tools { "preflight" } else { "inventory" },
            "strict_tools": strict_tools,
            "roster": targets,
            "roster_check": roster_check,
            "roster_advice": roster_advice_for_targets(targets),
            "tools": tools,
            "project_context": project_context,
            "failures": failures.clone(),
            "status": status,
        }),
        failures,
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_doctor(
    cwd: Option<String>,
    agents: Vec<String>,
    all: bool,
    no_project_context: bool,
    refresh_project_context: bool,
    max_files: usize,
    max_manifest_chars: usize,
    cache_dir: Option<PathBuf>,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let (targets, strict_tools) = doctor_targets(all, &agents)?;
    if json_output {
        let (payload, failures) = doctor_json_report(
            &cwd,
            &targets,
            strict_tools,
            no_project_context,
            refresh_project_context,
            max_files,
            max_manifest_chars,
            cache_dir,
        )?;
        println!("{}", serde_json::to_string_pretty(&payload)?);
        if failures.is_empty() {
            return Ok(());
        }
        return Err(format!(
            "doctor found {} issue(s): {}",
            failures.len(),
            failures.join("; ")
        )
        .into());
    }

    let mut failures = Vec::new();

    println!("Tales doctor");
    println!("cwd: {}", cwd.display());
    println!(
        "mode: {}",
        if strict_tools {
            "preflight"
        } else {
            "inventory"
        }
    );
    println!("roster: {}", targets.join(", "));

    match validate_roster(&targets) {
        Ok(()) => println!("roster check: ok"),
        Err(err) => {
            println!("roster check: error - {err}");
            failures.push(err.to_string());
        }
    }
    print_roster_advice_for_targets(&targets);

    println!("\nTools");
    for key in unique_tool_keys(&targets) {
        let Some(info) = tool_info(&key) else {
            println!("✗ {key}: unknown tool");
            failures.push(format!("unknown tool '{key}'"));
            continue;
        };
        let adapter = make_adapter(info.key)?;
        let caps = adapter.caps();
        let path = bin_path(info.bin);
        let mark = if path.is_some() { "✓" } else { "!" };
        println!(
            "{mark} {} ({}) [{}]",
            info.key,
            info.pretty,
            adapter_kind_label(info.kind)
        );
        match &path {
            Some(path) => println!("  bin: {} -> {}", info.bin, path.display()),
            None => {
                println!("  bin: {} -> missing", info.bin);
                println!("  install: {}", info.install);
                if strict_tools {
                    failures.push(format!("{} CLI missing from PATH", info.pretty));
                }
            }
        }
        println!("  caps: {}", format_caps(caps));
        if !info.models.is_empty() {
            println!("  suggested models: {}", info.models.join(", "));
        }
        if !info.efforts.is_empty() {
            println!("  efforts: {}", info.efforts.join(", "));
        }
    }

    println!("\nProject context");
    if no_project_context {
        println!("skipped");
    } else {
        let ctx = load_or_build(
            &cwd,
            ProjectContextOptions {
                max_files,
                max_manifest_chars,
                refresh: refresh_project_context,
                cache_dir,
            },
        )?;
        println!(
            "{}",
            if ctx.cache_hit {
                "cache: hit"
            } else {
                "cache: rebuilt"
            }
        );
        println!("path: {}", ctx.cache_path.display());
        let chars = ctx.text.chars().count();
        println!("chars: {chars}");
        println!("tokens~: {}", estimated_tokens(chars));
        println!(
            "default budget fit: {}",
            context_fit_label(chars, Some(DEFAULT_CONTEXT_BUDGET_CHARS))
        );
        println!("budgets: max_files={max_files}, max_manifest_chars={max_manifest_chars}");
    }

    if failures.is_empty() {
        println!("\nstatus: ok");
        return Ok(());
    }

    println!("\nstatus: attention required");
    Err(format!(
        "doctor found {} issue(s): {}",
        failures.len(),
        failures.join("; ")
    )
    .into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let Some(command) = Cli::parse().command else {
        // Bare `tales` → open the terminal workspace.
        launch_tui();
    };

    match command {
        Command::Term => launch_tui(),
        Command::Commands => {
            print_commands_reference();
            Ok(())
        }
        Command::Doctor {
            cwd,
            agent,
            all,
            no_project_context,
            refresh_project_context,
            max_files,
            max_manifest_chars,
            cache_dir,
            json,
        } => run_doctor(
            cwd,
            agent,
            all,
            no_project_context,
            refresh_project_context,
            max_files,
            max_manifest_chars,
            cache_dir,
            json,
        ),
        Command::Context {
            cwd,
            refresh,
            max_files,
            max_manifest_chars,
            cache_dir,
            print,
            json,
            output,
        } => run_context(
            cwd,
            refresh,
            max_files,
            max_manifest_chars,
            cache_dir,
            print,
            json,
            output,
        ),
        Command::Recover {
            cwd,
            latest,
            print,
            json,
            run,
        } => run_recover(cwd, run, latest, print, json),
        // `tales open` needs no async runtime or agents — it just spawns a new
        // Terminal window. Kept dead-simple so `/tales` is a zero-reasoning launcher.
        Command::Open { connect, task } => run_open(connect, task),
        Command::Profile { command } => run_profile_command(command),
        Command::Eval { command } => run_eval_command(command),
        Command::Solo {
            prompt,
            agent,
            model,
            effort,
            cwd,
            sandbox,
            image,
        } => run_solo(prompt, agent, model, effort, cwd, sandbox, image).await,
        Command::Run {
            prompt,
            drafter,
            critic,
            execute,
            drafter_model,
            critic_model,
            execute_model,
            drafter_effort,
            critic_effort,
            execute_effort,
            turns,
            cwd,
            sandbox,
            worktree,
            sequential,
            turn_timeout_secs,
            context_budget_chars,
            no_project_context,
            refresh_project_context,
            project_context_max_files,
            project_context_max_manifest_chars,
            demo,
            dry_run,
            json,
            report_path,
            report_json_path,
        } => {
            run_pipeline(
                prompt,
                drafter,
                critic,
                execute,
                drafter_model,
                critic_model,
                execute_model,
                drafter_effort,
                critic_effort,
                execute_effort,
                turns,
                cwd,
                sandbox,
                worktree,
                sequential,
                turn_timeout_secs,
                context_budget_chars,
                no_project_context,
                refresh_project_context,
                project_context_max_files,
                project_context_max_manifest_chars,
                demo,
                dry_run,
                json,
                report_path,
                report_json_path,
            )
            .await
        }
        Command::Discuss {
            prompt,
            drafter,
            critic,
            turns,
            drafter_model,
            drafter_effort,
            critic_model,
            critic_effort,
            cwd,
            sandbox,
            sequential,
            turn_timeout_secs,
            context_budget_chars,
            no_project_context,
            refresh_project_context,
            project_context_max_files,
            project_context_max_manifest_chars,
            demo,
            dry_run,
            json,
            report_path,
            report_json_path,
        } => {
            run_discuss(
                prompt,
                drafter,
                critic,
                turns,
                drafter_model,
                drafter_effort,
                critic_model,
                critic_effort,
                cwd,
                sandbox,
                sequential,
                turn_timeout_secs,
                context_budget_chars,
                no_project_context,
                refresh_project_context,
                project_context_max_files,
                project_context_max_manifest_chars,
                demo,
                dry_run,
                json,
                report_path,
                report_json_path,
            )
            .await
        }
    }
}

fn print_commands_reference() {
    println!("Tales command reference\n");
    println!("Startup");
    println!("  tales                 Open the interactive Tales terminal");
    println!("  tales --help          Show CLI help");
    println!("  tales help <command>  Show help for one command");
    println!("  tales commands        Show this command reference");
    println!();
    println!("Inside the Tales terminal");
    println!("  help or /help         Quick guidance");
    println!("  commands or /commands List available commands");
    println!("  /attach <path>        Attach an image or PDF to your next note");
    println!("  /confirm [tool|n]     Approve the recommended executor or choose one");
    println!("  /reject               Stop before execution");
    println!("  /artifacts            Show saved run files for recovery");
    println!("  /handoff [tool|n]     Resend the current plan or reopen the executor");
    println!("  /switch <tool|n>      Open a fresh executor pane with the current plan");
    println!("  /quit                 Leave Tales");
    println!();
    println!("Useful shortcuts");
    println!("  Ctrl-N shell          Open a shell pane");
    println!("  Ctrl-X Codex          Open a Codex pane");
    println!("  Ctrl-L Claude         Open a Claude Code pane");
    println!("  Ctrl-O Open Code      Open an Open Code pane");
    println!("  Ctrl-S send plan      Send the saved plan to the focused CLI pane");
    println!("  Ctrl-A approve        Answer yes to an approval prompt in the focused pane");
    println!("  Tab / Shift-Tab       Switch panes");
    println!("  Ctrl-Q                Quit");
    println!();
    println!("Artifacts and recovery");
    println!("  .tales/runs/<run>/plan.md       Rolling plan and transcript snapshot");
    println!("  .tales/runs/<run>/events.jsonl  Event log for stalled or timed-out runs");
    println!("  .tales/runs/<run>/manifest.json Run metadata and current status");
    println!("  .tales/last-plan.md             Latest executor handoff");
    println!("  tales recover                   List saved run artifacts");
    println!("  tales recover --latest --print  Print the newest saved plan");
    println!();
    println!("Scriptable commands");
    println!("  tales doctor --all    Check installed CLIs and workspace context");
    println!("  tales context         Build or inspect cached project context");
    println!("  tales solo <prompt>   Run one tool for one turn");
    println!("  tales discuss <task>  Run planner discussion only");
    println!("  tales run <task>      Plan, recommend, and execute non-interactively");
    println!("  tales open [task]     Open Tales in a new macOS Terminal window");
    println!();
    println!("Known tools");
    for tool in KNOWN_TOOLS {
        let status = if bin_path(tool.bin).is_some() {
            "ready"
        } else {
            "missing"
        };
        println!(
            "  {:<12} {:<10} {:<8} {}",
            tool.pretty, tool.bin, status, tool.install
        );
    }
}

fn cwd_from_arg(cwd: Option<String>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?))
}

fn run_profile_command(command: ProfileCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        ProfileCommand::Show {
            cwd,
            cache_dir,
            json,
        } => {
            let cwd = cwd_from_arg(cwd)?;
            let profile = load_profile(&cwd, cache_dir.as_deref())?;
            let status = profile_status_json(&cwd, cache_dir.as_deref(), &profile);
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_profile_status(&status);
            }
            Ok(())
        }
        ProfileCommand::Refresh {
            cwd,
            cache_dir,
            json,
        } => {
            let cwd = cwd_from_arg(cwd)?;
            let profile = refresh_profile(&cwd, cache_dir.as_deref())?;
            let status = profile_status_json(&cwd, cache_dir.as_deref(), &profile);
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("profile refreshed");
                print_profile_status(&status);
            }
            Ok(())
        }
        ProfileCommand::Clear {
            cwd,
            cache_dir,
            json,
        } => {
            let cwd = cwd_from_arg(cwd)?;
            let path = clear_profile(&cwd, cache_dir.as_deref())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "kind": "tales_profile_clear",
                        "schema_version": 1,
                        "cwd": cwd.display().to_string(),
                        "path": path.display().to_string(),
                        "status": "cleared",
                    }))?
                );
            } else {
                println!("profile cleared: {}", path.display());
            }
            Ok(())
        }
    }
}

fn print_profile_status(status: &Value) {
    println!("Tales workspace profile");
    println!("workspace: {}", status["workspace"].as_str().unwrap_or(""));
    println!("path: {}", status["path"].as_str().unwrap_or(""));
    print_json_string_list("commands", &status["commands"]);
    print_json_string_list("preferred tools", &status["preferred_tools"]);
    print_json_string_list("conventions", &status["conventions"]);
    print_json_string_list("last successful checks", &status["last_successful_checks"]);
    print_json_string_list("known warnings", &status["known_warnings"]);
}

fn print_json_string_list(label: &str, value: &Value) {
    let items = value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if items.is_empty() {
        println!("{label}: none");
    } else {
        println!("{label}:");
        for item in items {
            println!("  - {item}");
        }
    }
}

fn run_eval_command(command: EvalCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        EvalCommand::Run {
            task,
            scenario,
            cwd,
            out,
            json,
        } => {
            let cwd = cwd_from_arg(cwd)?;
            let context = PreflightContextSummary::default();
            let profile = load_profile(&cwd, None).ok();
            let scenario = select_eval_scenario(&task, &scenario)?;
            let result = run_mock_eval(&cwd, scenario, &context, profile.as_ref());
            let value = json!({
                "kind": "tales_eval_run",
                "schema_version": 1,
                "result": result,
                "side_effects": {
                    "model_calls": false,
                    "agents_spawned": false,
                },
            });
            if let Some(path) = out {
                save_eval_report(&path, &value)?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print_eval_summary(&value);
            }
            Ok(())
        }
        EvalCommand::Compare {
            task,
            cwd,
            out,
            json,
        } => {
            let cwd = cwd_from_arg(cwd)?;
            let context = PreflightContextSummary::default();
            let profile = load_profile(&cwd, None).ok();
            let results = default_scenarios(&task)
                .into_iter()
                .map(|scenario| run_mock_eval(&cwd, scenario, &context, profile.as_ref()))
                .collect::<Vec<_>>();
            let value = compare_results(&results);
            if let Some(path) = out {
                save_eval_report(&path, &value)?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print_eval_summary(&value);
            }
            Ok(())
        }
        EvalCommand::Report { input, json } => {
            let text = std::fs::read_to_string(&input)?;
            let value: Value = serde_json::from_str(&text)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print_eval_summary(&value);
            }
            Ok(())
        }
    }
}

fn select_eval_scenario(
    task: &str,
    name: &str,
) -> Result<EvalScenario, Box<dyn std::error::Error>> {
    default_scenarios(task)
        .into_iter()
        .find(|scenario| scenario.name == name)
        .ok_or_else(|| {
            format!("unknown eval scenario '{name}' (valid: solo_strong, debate_parallel, tiered_parallel)").into()
        })
}

fn print_eval_summary(value: &Value) {
    println!("Tales eval");
    if let Some(result) = value.get("result") {
        print_eval_result(result);
        return;
    }
    if let Some(results) = value.get("scenarios").and_then(Value::as_array) {
        println!(
            "best latency shape: {}",
            value["best_latency_shape"].as_str().unwrap_or("unknown")
        );
        println!(
            "cheapest payload shape: {}",
            value["cheapest_payload_shape"]
                .as_str()
                .unwrap_or("unknown")
        );
        for result in results {
            print_eval_result(result);
        }
    }
}

fn print_eval_result(result: &Value) {
    let scenario = &result["scenario"];
    let metrics = &result["metrics"];
    println!(
        "- {}: calls={} worst={} payload={} chars (~{} tokens)",
        scenario["name"].as_str().unwrap_or("scenario"),
        metrics["planned_calls"].as_u64().unwrap_or(0),
        metrics["worst_case_planned_calls"].as_u64().unwrap_or(0),
        metrics["known_payload_chars"].as_u64().unwrap_or(0),
        metrics["estimated_tokens"].as_u64().unwrap_or(0)
    );
}

async fn run_solo(
    prompt: String,
    agent_name: String,
    model: Option<String>,
    effort: Option<String>,
    cwd: Option<String>,
    sandbox: String,
    images: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    validate_tool_readiness(std::slice::from_ref(&agent_name))?;
    validate_effort(&agent_name, effort.as_deref())?;
    let agent = Uuid::new_v4();
    let allowed_tools = if agent_name.eq_ignore_ascii_case("claude") {
        Some(vec![
            "Write".into(),
            "Edit".into(),
            "MultiEdit".into(),
            "Read".into(),
        ])
    } else {
        None
    };
    let ctx = SpawnCtx {
        agent,
        label: agent_name.clone(),
        cwd,
        model,
        effort,
        permission_mode: "acceptEdits".to_string(),
        sandbox,
        allowed_tools,
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
    let attachments: Vec<Attachment> = images.iter().map(Attachment::new).collect();
    cmd_tx
        .send(AgentCommand::StartTurn {
            prompt,
            attachments,
        })
        .await?;

    let mut streaming = false;
    while let Some(event) = events_rx.recv().await {
        match event {
            AgentEvent::SessionReady {
                native_session_id, ..
            } => println!("● session {native_session_id}"),
            AgentEvent::Skills { skills, .. } => {
                println!("● {} skills available", skills.len());
            }
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
                eprintln!(
                    "\n✗ error{}: {message}",
                    if fatal { " (fatal)" } else { "" }
                );
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
    drafter_effort: Option<String>,
    critic_model: Option<String>,
    critic_effort: Option<String>,
    cwd: Option<String>,
    sandbox: String,
    sequential: bool,
    turn_timeout_secs: Option<u64>,
    context_budget_chars: Option<usize>,
    no_project_context: bool,
    refresh_project_context: bool,
    project_context_max_files: usize,
    project_context_max_manifest_chars: usize,
    demo: bool,
    dry_run: bool,
    json_output: bool,
    report_path: Option<PathBuf>,
    report_json_path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    if json_output && !dry_run {
        return Err("--json requires --dry-run".into());
    }
    if !demo {
        validate_tool_readiness(&[drafter.clone(), critic.clone()])?;
        validate_effort(&drafter, drafter_effort.as_deref())?;
        validate_effort(&critic, critic_effort.as_deref())?;
    }
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let report_path = resolve_report_path(&cwd, report_path);
    let report_json_path = resolve_report_path(&cwd, report_json_path);
    let preflight_agents = vec![
        PreflightAgent {
            role: "drafter",
            key: drafter.clone(),
            model: drafter_model.clone(),
            effort: drafter_effort.clone(),
        },
        PreflightAgent {
            role: "critic",
            key: critic.clone(),
            model: critic_model.clone(),
            effort: critic_effort.clone(),
        },
    ];
    if dry_run {
        return print_discuss_dry_run(
            &prompt,
            &cwd,
            &preflight_agents,
            turns,
            &sandbox,
            sequential,
            context_budget_chars,
            no_project_context,
            refresh_project_context,
            project_context_max_files,
            project_context_max_manifest_chars,
            demo,
            json_output,
            report_path.as_deref(),
            report_json_path.as_deref(),
        );
    }

    let roster = preflight_agents
        .iter()
        .map(|agent| agent.key.clone())
        .collect::<Vec<_>>();
    let artifacts = RunArtifacts::create(
        &cwd,
        "discuss",
        &prompt,
        &roster,
        report_path.as_deref(),
        report_json_path.as_deref(),
    )?;
    println!("● artifacts: {}", artifacts.relative_dir());

    let (bus, _commands_rx) = EventBus::new(1024, 64);

    // Console frontend: render the conversation as it streams onto the bus.
    let printer = spawn_printer(&bus);
    let artifact_writer = spawn_artifact_writer(&bus, artifacts.clone());
    let report_writer = spawn_report_writer(&bus, report_path, report_json_path);

    let mut orch = Orchestrator::new(bus.clone());
    // Parallel rounds by default — planners draft concurrently, then synthesize.
    orch.set_parallel_rounds(!sequential);
    configure_orchestrator(&mut orch, turn_timeout_secs, context_budget_chars);
    configure_project_context(
        &mut orch,
        &cwd,
        no_project_context,
        refresh_project_context,
        project_context_max_files,
        project_context_max_manifest_chars,
    )?;
    configure_local_changes(&mut orch, &cwd);

    let drafter_ctx = SpawnCtx {
        agent: Uuid::new_v4(),
        label: drafter.clone(),
        cwd: cwd.clone(),
        model: drafter_model,
        effort: drafter_effort,
        permission_mode: "acceptEdits".to_string(),
        sandbox: sandbox.clone(),
        allowed_tools: None,
    };
    let critic_ctx = SpawnCtx {
        agent: Uuid::new_v4(),
        label: critic.clone(),
        cwd,
        model: critic_model,
        effort: critic_effort,
        permission_mode: "acceptEdits".to_string(),
        sandbox,
        allowed_tools: None,
    };

    let planning_turns = demo_planning_turns(turns, sequential, 2);
    if demo {
        orch.add_agent(
            Box::new(MockAdapter::new(demo_planning_responses(
                &drafter,
                Role::Drafter,
                planning_turns,
            ))),
            drafter_ctx,
            Role::Drafter,
        )
        .await?;
        orch.add_agent(
            Box::new(MockAdapter::new(demo_planning_responses(
                &critic,
                Role::Critic,
                planning_turns,
            ))),
            critic_ctx,
            Role::Critic,
        )
        .await?;
    } else {
        orch.add_agent(make_adapter(&drafter)?, drafter_ctx, Role::Drafter)
            .await?;
        orch.add_agent(make_adapter(&critic)?, critic_ctx, Role::Critic)
            .await?;
    }

    println!(
        "\n=== discussion{}: {prompt} ({turns} turns) ===",
        if demo { " demo" } else { "" }
    );
    let discuss_result = orch.run_discussion(&prompt, turns).await;
    orch.shutdown().await;

    if let Some(handle) = report_writer {
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    // Let the printer drain the final events, then stop it. We abort rather
    // than await: `orch` still holds a bus clone, so the broadcast never closes
    // on its own.
    sleep(Duration::from_millis(200)).await;
    printer.abort();
    artifact_writer.abort();

    match &discuss_result {
        Ok(_) => artifacts.mark_finished("done", None),
        Err(e) => artifacts.mark_failed(&e.to_string(), None),
    }

    discuss_result?;
    println!("\n=== discussion complete ===");
    Ok(())
}

fn flush() {
    let _ = std::io::stdout().flush();
}

fn preview_chars(text: &str, max_chars: usize) -> String {
    let mut preview = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count == max_chars {
            preview.push_str("\n... truncated ...");
            break;
        }
        preview.push(ch);
    }
    preview
}

/// Spawn a console printer that renders orchestration events as they stream onto
/// the bus. Shared by `discuss` and `run`.
fn spawn_printer(bus: &EventBus) -> tokio::task::JoinHandle<()> {
    let mut events = bus.subscribe();
    tokio::spawn(async move {
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
                    OrchestratorEvent::RecommendationReady {
                        executor,
                        rationale,
                        confident,
                        scores,
                    } => {
                        let confidence = if confident {
                            "consensus"
                        } else {
                            "needs your call"
                        };
                        let scores = format_scores(&scores);
                        println!(
                            "\n★ recommended executor: {executor} ({confidence})\n{scores}\n{rationale}"
                        );
                    }
                    OrchestratorEvent::ExecutionPacket {
                        executor,
                        text,
                        included_in_prompt,
                    } => {
                        let delivery = if included_in_prompt {
                            "sent to prompt"
                        } else {
                            "session audit"
                        };
                        println!(
                            "\n──────── execution packet for {executor} ({delivery}) ────────\n{}",
                            preview_chars(&text, 1800)
                        );
                    }
                    OrchestratorEvent::SessionReport { markdown, .. } => {
                        println!(
                            "\n──────── session report ────────\n{}",
                            preview_chars(&markdown, 3000)
                        );
                    }
                    OrchestratorEvent::TurnComplete {
                        cost_usd: Some(c), ..
                    } => {
                        // Claude reports a session-cumulative cost (Codex / Open
                        // Code report none), so this is the running total, not a
                        // per-turn delta.
                        println!("  (session cost ${c:.4})");
                    }
                    OrchestratorEvent::TurnComplete {
                        token_usage: Some(usage),
                        ..
                    } => {
                        println!("  ({})", format_token_usage(usage));
                    }
                    OrchestratorEvent::Fatal { msg } => println!("✗ fatal: {msg}"),
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    })
}

fn spawn_report_writer(
    bus: &EventBus,
    markdown_path: Option<PathBuf>,
    json_path: Option<PathBuf>,
) -> Option<tokio::task::JoinHandle<()>> {
    let enabled = markdown_path.is_some() || json_path.is_some();
    if !enabled {
        return None;
    }
    let mut events = bus.subscribe();
    let bus = bus.clone();
    Some(tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            if let OrchestratorEvent::SessionReport { markdown, summary } = ev {
                if let Some(path) = markdown_path.as_deref() {
                    match write_text_file(path, &markdown) {
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
                        .and_then(|text| write_text_file(path, &text))
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

fn write_text_file(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

fn resolve_report_path(cwd: &Path, path: Option<PathBuf>) -> Option<PathBuf> {
    path.map(|p| if p.is_absolute() { p } else { cwd.join(p) })
}

#[derive(Clone, Debug)]
struct RecoverRun {
    id: String,
    path: PathBuf,
    task: Option<String>,
    status: Option<String>,
    executor: Option<String>,
    updated_unix: Option<u64>,
}

fn run_recover(
    cwd: Option<String>,
    run: Option<String>,
    latest: bool,
    print: bool,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let runs = collect_recover_runs(&cwd)?;
    if runs.is_empty() {
        if json_output {
            println!("{}", json!({ "runs": [] }));
        } else {
            println!(
                "No Tales run artifacts found under {}",
                runs_dir(&cwd).display()
            );
        }
        return Ok(());
    }

    let selected = match (run.as_deref(), latest) {
        (Some(id), _) => Some(resolve_recover_run(&cwd, &runs, id)?),
        (None, true) => runs.first().cloned(),
        (None, false) => None,
    };

    if let Some(selected) = selected {
        if json_output {
            let plan = if print {
                fs::read_to_string(selected.path.join("plan.md")).ok()
            } else {
                None
            };
            println!(
                "{}",
                json!({
                    "run": recover_run_json(&selected),
                    "plan": plan,
                })
            );
        } else {
            print_recover_run(&selected);
            if print {
                let path = selected.path.join("plan.md");
                println!("\n──────── {} ────────", path.display());
                match fs::read_to_string(&path) {
                    Ok(text) => println!("{text}"),
                    Err(e) => println!("Could not read plan: {e}"),
                }
            }
        }
        return Ok(());
    }

    if json_output {
        let value = json!({
            "runs": runs.iter().map(recover_run_json).collect::<Vec<_>>(),
        });
        println!("{value}");
    } else {
        println!("Tales run artifacts in {}", runs_dir(&cwd).display());
        for run in &runs {
            print_recover_run_line(run);
        }
        println!("\nUse `tales recover --latest --print` or `tales recover <run-id> --print`.");
    }
    Ok(())
}

fn runs_dir(cwd: &Path) -> PathBuf {
    cwd.join(TALES_DIR).join(RUNS_DIR)
}

fn collect_recover_runs(cwd: &Path) -> std::io::Result<Vec<RecoverRun>> {
    let dir = runs_dir(cwd);
    let mut runs = Vec::new();
    if !dir.is_dir() {
        return Ok(runs);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        runs.push(load_recover_run(path));
    }
    runs.sort_by(|a, b| {
        b.updated_unix
            .cmp(&a.updated_unix)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(runs)
}

fn load_recover_run(path: PathBuf) -> RecoverRun {
    let id = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("run")
        .to_string();
    let manifest = fs::read_to_string(path.join("manifest.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let fallback_updated_unix = id
        .split('-')
        .next()
        .and_then(|prefix| prefix.parse::<u64>().ok());
    RecoverRun {
        id,
        path,
        task: manifest
            .as_ref()
            .and_then(|value| value.get("task"))
            .and_then(Value::as_str)
            .map(str::to_string),
        status: manifest
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .map(str::to_string),
        executor: manifest
            .as_ref()
            .and_then(|value| value.get("executor"))
            .and_then(Value::as_str)
            .map(str::to_string),
        updated_unix: manifest
            .as_ref()
            .and_then(|value| value.get("updated_unix"))
            .and_then(Value::as_u64)
            .or(fallback_updated_unix),
    }
}

fn resolve_recover_run(
    cwd: &Path,
    runs: &[RecoverRun],
    id_or_path: &str,
) -> std::io::Result<RecoverRun> {
    if let Some(run) = runs.iter().find(|run| run.id == id_or_path) {
        return Ok(run.clone());
    }
    let path = PathBuf::from(id_or_path);
    let path = if path.is_absolute() {
        path
    } else {
        runs_dir(cwd).join(path)
    };
    if path.is_dir() {
        return Ok(load_recover_run(path));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("no Tales run artifact found for {id_or_path}"),
    ))
}

fn recover_run_json(run: &RecoverRun) -> Value {
    json!({
        "id": run.id,
        "path": run.path.display().to_string(),
        "task": run.task,
        "status": run.status,
        "executor": run.executor,
        "updated_unix": run.updated_unix,
        "plan_path": run.path.join("plan.md").display().to_string(),
        "events_path": run.path.join("events.jsonl").display().to_string(),
        "manifest_path": run.path.join("manifest.json").display().to_string(),
    })
}

fn print_recover_run(run: &RecoverRun) {
    print_recover_run_line(run);
    println!("  plan: {}", run.path.join("plan.md").display());
    println!("  events: {}", run.path.join("events.jsonl").display());
    println!("  manifest: {}", run.path.join("manifest.json").display());
}

fn print_recover_run_line(run: &RecoverRun) {
    println!(
        "- {}  status={}  executor={}  updated={}  task={}",
        run.id,
        run.status.as_deref().unwrap_or("unknown"),
        run.executor.as_deref().unwrap_or("-"),
        run.updated_unix
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string()),
        run.task.as_deref().unwrap_or("-")
    );
}

#[derive(Clone)]
struct RunArtifacts {
    dir: PathBuf,
    manifest_path: PathBuf,
    events_path: PathBuf,
    plan_path: PathBuf,
    started_at: u64,
    mode: String,
    task: String,
    workspace: PathBuf,
    roster: Vec<String>,
    report_path: Option<PathBuf>,
    report_json_path: Option<PathBuf>,
}

impl RunArtifacts {
    fn create(
        workspace: &Path,
        mode: &str,
        task: &str,
        roster: &[String],
        report_path: Option<&Path>,
        report_json_path: Option<&Path>,
    ) -> std::io::Result<Self> {
        let started_at = unix_time();
        let base_name = format!("{started_at}-{}", slug(task));
        let root = runs_dir(workspace);
        fs::create_dir_all(&root)?;
        let mut dir = root.join(&base_name);
        let mut suffix = 2;
        while dir.exists() {
            dir = root.join(format!("{base_name}-{suffix}"));
            suffix += 1;
        }
        fs::create_dir_all(&dir)?;
        let artifacts = Self {
            manifest_path: dir.join("manifest.json"),
            events_path: dir.join("events.jsonl"),
            plan_path: dir.join("plan.md"),
            dir,
            started_at,
            mode: mode.to_string(),
            task: task.to_string(),
            workspace: workspace.to_path_buf(),
            roster: roster.to_vec(),
            report_path: report_path.map(Path::to_path_buf),
            report_json_path: report_json_path.map(Path::to_path_buf),
        };
        artifacts.write_manifest("planning", None)?;
        artifacts.write_plan_markdown("planning", None, "")?;
        artifacts.append_manual_event("run_started", &format!("{mode}: {task}"))?;
        Ok(artifacts)
    }

    fn relative_dir(&self) -> String {
        self.dir
            .strip_prefix(&self.workspace)
            .unwrap_or(&self.dir)
            .display()
            .to_string()
    }

    fn write_manifest(&self, status: &str, executor: Option<&str>) -> std::io::Result<()> {
        let value = json!({
            "kind": "tales_run_manifest",
            "schema_version": 1,
            "mode": self.mode,
            "task": self.task,
            "workspace": self.workspace.display().to_string(),
            "run_dir": self.dir.display().to_string(),
            "started_unix": self.started_at,
            "updated_unix": unix_time(),
            "status": status,
            "executor": executor,
            "roster": self.roster,
            "plan_path": self.plan_path.display().to_string(),
            "events_path": self.events_path.display().to_string(),
            "report_path": self.report_path.as_ref().map(|path| path.display().to_string()),
            "report_json_path": self.report_json_path.as_ref().map(|path| path.display().to_string()),
        });
        write_text_file(&self.manifest_path, &format!("{value:#}\n"))
    }

    fn append_manual_event(&self, kind: &str, message: &str) -> std::io::Result<()> {
        self.append_json(json!({
            "time_unix": unix_time(),
            "kind": kind,
            "message": message,
        }))
    }

    fn append_event(&self, ev: &OrchestratorEvent) -> std::io::Result<()> {
        self.append_json(event_record(ev))
    }

    fn append_json(&self, value: Value) -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)?;
        writeln!(file, "{value}")
    }

    fn write_plan_markdown(
        &self,
        status: &str,
        executor: Option<&str>,
        transcript: &str,
    ) -> std::io::Result<()> {
        let roster = self
            .roster
            .iter()
            .map(|key| tool_label(key))
            .collect::<Vec<_>>()
            .join(" + ");
        let executor_line = executor
            .map(|key| format!("- Executor: {} (`{key}`)\n", tool_label(key)))
            .unwrap_or_default();
        let report_lines = match (&self.report_path, &self.report_json_path) {
            (Some(markdown), Some(json)) => format!(
                "- Report markdown: {}\n- Report JSON: {}\n",
                markdown.display(),
                json.display()
            ),
            (Some(markdown), None) => format!("- Report markdown: {}\n", markdown.display()),
            (None, Some(json)) => format!("- Report JSON: {}\n", json.display()),
            (None, None) => String::new(),
        };
        let transcript = if transcript.trim().is_empty() {
            "_Planning has started. No transcript has been captured yet._"
        } else {
            transcript.trim()
        };
        let text = format!(
            "# Tales {} plan\n\n\
             - Status: {status}\n\
             - Task: {}\n\
             - Workspace: {}\n\
             - Roster: {roster}\n\
             - Started unix time: {}\n\
             {executor_line}\
             {report_lines}\
             ## Current transcript and plan\n\n\
             {transcript}\n",
            self.mode,
            self.task,
            self.workspace.display(),
            self.started_at
        );
        write_text_file(&self.plan_path, &text)
    }

    fn mark_finished(&self, status: &str, executor: Option<&str>) {
        let _ = self.append_manual_event("run_finished", status);
        let _ = self.write_manifest(status, executor);
    }

    fn mark_failed(&self, message: &str, executor: Option<&str>) {
        let _ = self.append_manual_event("run_failed", message);
        let _ = self.write_manifest("failed", executor);
    }
}

struct ArtifactSnapshot {
    labels: HashMap<Uuid, String>,
    transcript: String,
    status: String,
    executor: Option<String>,
}

impl ArtifactSnapshot {
    fn new() -> Self {
        Self {
            labels: HashMap::new(),
            transcript: String::new(),
            status: "planning".to_string(),
            executor: None,
        }
    }

    fn label_for(&self, agent: &Uuid) -> String {
        self.labels
            .get(agent)
            .cloned()
            .unwrap_or_else(|| agent.to_string())
    }

    fn push_section(&mut self, title: &str, body: &str) {
        if !self.transcript.is_empty() {
            self.transcript.push_str("\n\n");
        }
        self.transcript.push_str("## ");
        self.transcript.push_str(title);
        self.transcript.push_str("\n\n");
        self.transcript.push_str(body.trim());
        self.transcript.push('\n');
    }

    fn apply(&mut self, ev: &OrchestratorEvent) -> bool {
        match ev {
            OrchestratorEvent::AgentSpawned { agent, label, .. } => {
                self.labels.insert(*agent, label.clone());
                false
            }
            OrchestratorEvent::Message { agent, text } => {
                self.push_section(&self.label_for(agent), text);
                true
            }
            OrchestratorEvent::UserMessage { text } => {
                self.push_section("You", text);
                true
            }
            OrchestratorEvent::ToolActivity { agent, summary } => {
                let body = format!("{}: {summary}", self.label_for(agent));
                self.push_section("Tool activity", &body);
                true
            }
            OrchestratorEvent::RecommendationReady {
                executor,
                rationale,
                confident,
                scores,
            } => {
                self.status = "recommended".to_string();
                self.executor = Some(executor.clone());
                let body = format!(
                    "Executor: {} (`{executor}`)\nConfidence: {}\n{}\n\n{rationale}",
                    tool_label(executor),
                    if *confident {
                        "consensus"
                    } else {
                        "needs review"
                    },
                    format_scores(scores)
                );
                self.push_section("Recommendation", &body);
                true
            }
            OrchestratorEvent::ExecutionPacket {
                executor,
                text,
                included_in_prompt,
            } => {
                self.status = "execution_packet".to_string();
                self.executor = Some(executor.clone());
                let body = format!(
                    "Executor: {} (`{executor}`)\nDelivery: {}\n\n{text}",
                    tool_label(executor),
                    if *included_in_prompt {
                        "sent to prompt"
                    } else {
                        "session audit"
                    }
                );
                self.push_section("Execution packet", &body);
                true
            }
            OrchestratorEvent::SessionReport { markdown, .. } => {
                if self.status != "done" && self.status != "failed" {
                    self.status = "report_ready".to_string();
                }
                self.push_section("Session report", markdown);
                true
            }
            OrchestratorEvent::AwaitingConfirmation { prompt } => {
                self.status = "awaiting_executor".to_string();
                self.push_section("Awaiting confirmation", prompt);
                true
            }
            OrchestratorEvent::PhaseChanged { phase } => {
                self.status = event_status(ev).to_string();
                self.push_section("Phase", phase);
                true
            }
            OrchestratorEvent::Log { level, msg } if level == "warn" || level == "error" => {
                let body = format!("{level}: {msg}");
                self.push_section("Log", &body);
                true
            }
            OrchestratorEvent::Fatal { msg } => {
                self.status = "failed".to_string();
                self.push_section("Fatal error", msg);
                true
            }
            _ => false,
        }
    }
}

fn spawn_artifact_writer(bus: &EventBus, artifacts: RunArtifacts) -> tokio::task::JoinHandle<()> {
    let mut events = bus.subscribe();
    tokio::spawn(async move {
        let mut snapshot = ArtifactSnapshot::new();
        while let Ok(ev) = events.recv().await {
            let _ = artifacts.append_event(&ev);
            if snapshot.apply(&ev) {
                let _ = artifacts.write_manifest(&snapshot.status, snapshot.executor.as_deref());
                let _ = artifacts.write_plan_markdown(
                    &snapshot.status,
                    snapshot.executor.as_deref(),
                    &snapshot.transcript,
                );
            }
        }
    })
}

fn event_status(ev: &OrchestratorEvent) -> &'static str {
    match ev {
        OrchestratorEvent::AwaitingConfirmation { .. } => "awaiting_executor",
        OrchestratorEvent::RecommendationReady { .. } => "recommended",
        OrchestratorEvent::ExecutionPacket { .. } => "execution_packet",
        OrchestratorEvent::SessionReport { .. } => "report_ready",
        OrchestratorEvent::Fatal { .. } => "failed",
        OrchestratorEvent::PhaseChanged { phase } if phase == "done" => "done",
        OrchestratorEvent::PhaseChanged { phase } if phase == "executing" => "executing",
        OrchestratorEvent::PhaseChanged { phase } if phase == "recommending" => "recommending",
        _ => "planning",
    }
}

fn event_record(ev: &OrchestratorEvent) -> Value {
    match ev {
        OrchestratorEvent::AgentSpawned {
            agent,
            label,
            session_id,
        } => json!({
            "time_unix": unix_time(),
            "kind": "agent_spawned",
            "agent": agent.to_string(),
            "label": label,
            "session_id": session_id,
        }),
        OrchestratorEvent::Token { agent, text } => json!({
            "time_unix": unix_time(),
            "kind": "token",
            "agent": agent.to_string(),
            "chars": text.chars().count(),
            "preview": json_preview(text, 240),
        }),
        OrchestratorEvent::TurnStarted { agent, role } => json!({
            "time_unix": unix_time(),
            "kind": "turn_started",
            "agent": agent.to_string(),
            "role": role,
        }),
        OrchestratorEvent::Message { agent, text } => json!({
            "time_unix": unix_time(),
            "kind": "message",
            "agent": agent.to_string(),
            "chars": text.chars().count(),
            "text": text,
        }),
        OrchestratorEvent::UserMessage { text } => json!({
            "time_unix": unix_time(),
            "kind": "user_message",
            "chars": text.chars().count(),
            "text": text,
        }),
        OrchestratorEvent::ToolActivity { agent, summary } => json!({
            "time_unix": unix_time(),
            "kind": "tool_activity",
            "agent": agent.to_string(),
            "summary": summary,
        }),
        OrchestratorEvent::TurnComplete {
            agent,
            cost_usd,
            token_usage,
        } => json!({
            "time_unix": unix_time(),
            "kind": "turn_complete",
            "agent": agent.to_string(),
            "cost_usd": cost_usd,
            "token_usage": token_usage.as_ref().map(|usage| json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "total_tokens": usage.total_tokens,
            })),
        }),
        OrchestratorEvent::AgentExited { agent, code } => json!({
            "time_unix": unix_time(),
            "kind": "agent_exited",
            "agent": agent.to_string(),
            "code": code,
        }),
        OrchestratorEvent::PhaseChanged { phase } => json!({
            "time_unix": unix_time(),
            "kind": "phase_changed",
            "phase": phase,
        }),
        OrchestratorEvent::RecommendationReady {
            executor,
            rationale,
            confident,
            scores,
        } => json!({
            "time_unix": unix_time(),
            "kind": "recommendation_ready",
            "executor": executor,
            "rationale": rationale,
            "confident": confident,
            "scores": scores,
        }),
        OrchestratorEvent::ExecutionPacket {
            executor,
            text,
            included_in_prompt,
        } => json!({
            "time_unix": unix_time(),
            "kind": "execution_packet",
            "executor": executor,
            "chars": text.chars().count(),
            "included_in_prompt": included_in_prompt,
            "text": text,
        }),
        OrchestratorEvent::SessionReport { markdown, summary } => json!({
            "time_unix": unix_time(),
            "kind": "session_report",
            "markdown_chars": markdown.chars().count(),
            "markdown": markdown,
            "summary": summary,
        }),
        OrchestratorEvent::AwaitingConfirmation { prompt } => json!({
            "time_unix": unix_time(),
            "kind": "awaiting_confirmation",
            "prompt": prompt,
        }),
        OrchestratorEvent::Log { level, msg } => json!({
            "time_unix": unix_time(),
            "kind": "log",
            "level": level,
            "message": msg,
        }),
        OrchestratorEvent::Fatal { msg } => json!({
            "time_unix": unix_time(),
            "kind": "fatal",
            "message": msg,
        }),
    }
}

fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn slug(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if (ch.is_whitespace() || ch == '-' || ch == '_') && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "run".to_string()
    } else {
        out
    }
}

fn json_preview(text: &str, limit: usize) -> String {
    let mut out = text.chars().take(limit).collect::<String>();
    if text.chars().count() > limit {
        out.push_str("...");
    }
    out
}

fn tool_label(key: &str) -> String {
    tool_info(key)
        .map(|info| info.pretty.to_string())
        .unwrap_or_else(|| key.to_string())
}

#[allow(clippy::too_many_arguments)]
fn mk_ctx(
    agent: Uuid,
    label: &str,
    cwd: &std::path::Path,
    model: Option<String>,
    effort: Option<String>,
    sandbox: &str,
    permission_mode: &str,
    allowed_tools: Option<Vec<String>>,
) -> SpawnCtx {
    SpawnCtx {
        agent,
        label: label.to_string(),
        cwd: cwd.to_path_buf(),
        model,
        effort,
        permission_mode: permission_mode.to_string(),
        sandbox: sandbox.to_string(),
        allowed_tools,
    }
}

fn configure_orchestrator(
    orch: &mut Orchestrator,
    turn_timeout_secs: Option<u64>,
    context_budget_chars: Option<usize>,
) {
    if let Some(secs) = turn_timeout_secs {
        orch.set_turn_timeout(Duration::from_secs(secs.max(1)));
    }
    if let Some(chars) = context_budget_chars {
        let budget = if chars == 0 { None } else { Some(chars) };
        orch.set_context_budget_chars(budget);
    }
}

fn configure_project_context(
    orch: &mut Orchestrator,
    cwd: &std::path::Path,
    disabled: bool,
    refresh: bool,
    max_files: usize,
    max_manifest_chars: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if disabled {
        return Ok(());
    }
    let ctx = load_or_build(
        cwd,
        ProjectContextOptions {
            max_files,
            max_manifest_chars,
            refresh,
            ..ProjectContextOptions::default()
        },
    )?;
    eprintln!(
        "● project context {} ({}, max_files={}, max_manifest_chars={})",
        if ctx.cache_hit { "cache hit" } else { "cached" },
        ctx.cache_path.display(),
        max_files,
        max_manifest_chars
    );
    let report = ProjectContextReport {
        cache_hit: ctx.cache_hit,
        cache_path: ctx.cache_path.display().to_string(),
        chars: ctx.text.chars().count(),
        max_files,
        max_manifest_chars,
    };
    orch.set_project_context(Some(ctx.text));
    orch.set_project_context_report(Some(report));
    Ok(())
}

fn configure_local_changes(orch: &mut Orchestrator, cwd: &std::path::Path) {
    if let Some(changes) = local_change_summary(cwd, DEFAULT_LOCAL_CHANGE_LINES) {
        eprintln!("● local changes {}", changes.summary_line());
        orch.set_local_changes(Some(changes));
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

#[derive(Clone, Debug)]
struct PreflightAgent {
    role: &'static str,
    key: String,
    model: Option<String>,
    effort: Option<String>,
}

fn context_budget_label(chars: Option<usize>) -> String {
    match chars {
        Some(0) => "unlimited".to_string(),
        Some(chars) => format!("{chars} chars (~{} tokens)", estimated_tokens(chars)),
        None => format!(
            "default {DEFAULT_CONTEXT_BUDGET_CHARS} chars (~{} tokens)",
            estimated_tokens(DEFAULT_CONTEXT_BUDGET_CHARS)
        ),
    }
}

fn estimated_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

fn format_token_usage(usage: TokenUsage) -> String {
    let mut parts = Vec::new();
    if let Some(total) = usage.total_or_sum() {
        parts.push(format!("total={total}"));
    }
    if let Some(input) = usage.input_tokens {
        parts.push(format!("input={input}"));
    }
    if let Some(cached) = usage.cached_input_tokens {
        parts.push(format!("cached_input={cached}"));
    }
    if let Some(output) = usage.output_tokens {
        parts.push(format!("output={output}"));
    }
    if let Some(reasoning) = usage.reasoning_output_tokens {
        parts.push(format!("reasoning_output={reasoning}"));
    }
    if parts.is_empty() {
        "tokens unavailable".to_string()
    } else {
        format!("tokens {}", parts.join(", "))
    }
}

fn format_scores(scores: &[(String, f32)]) -> String {
    if scores.is_empty() {
        return "scores: unavailable".to_string();
    }
    let mut parts = scores
        .iter()
        .take(4)
        .map(|(executor, score)| format!("{executor}={score:.2}"))
        .collect::<Vec<_>>();
    if scores.len() > 4 {
        parts.push(format!("+{} more", scores.len() - 4));
    }
    format!("scores: {}", parts.join(", "))
}

fn effective_context_budget(chars: Option<usize>) -> Option<usize> {
    match chars {
        Some(0) => None,
        Some(chars) => Some(chars),
        None => Some(DEFAULT_CONTEXT_BUDGET_CHARS),
    }
}

fn context_fit_label(context_chars: usize, budget_chars: Option<usize>) -> String {
    let context_tokens = estimated_tokens(context_chars);
    match budget_chars {
        Some(budget) => {
            let budget_tokens = estimated_tokens(budget);
            let pct = if budget == 0 {
                0
            } else {
                (context_chars.saturating_mul(100)).div_ceil(budget)
            };
            let status = if context_chars > budget {
                "over budget"
            } else {
                "fits"
            };
            format!(
                "{context_chars}/{budget} chars ({pct}%, ~{context_tokens}/{budget_tokens} tokens) - {status}"
            )
        }
        None => format!("{context_chars} chars (~{context_tokens} tokens) - unlimited budget"),
    }
}

fn print_preflight_agents(agents: &[PreflightAgent], demo: bool) {
    println!("\nAgents");
    for agent in agents {
        let model = agent.model.as_deref().unwrap_or("default");
        let effort = agent.effort.as_deref().unwrap_or("default");
        match tool_info(&agent.key) {
            Some(info) => {
                let path = bin_path(info.bin)
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "missing".to_string());
                println!(
                    "- {}: {} ({}) [{}]",
                    agent.role,
                    agent.key,
                    info.pretty,
                    adapter_kind_label(info.kind)
                );
                println!("  bin: {} -> {path}", info.bin);
                println!("  model: {model}; effort: {effort}");
            }
            None if demo => {
                println!("- {}: {} (demo mock)", agent.role, agent.key);
                println!("  model: {model}; effort: {effort}");
            }
            None => {
                println!("- {}: {} (unknown)", agent.role, agent.key);
                println!("  model: {model}; effort: {effort}");
            }
        }
    }
}

fn print_preflight_context(
    cwd: &Path,
    disabled: bool,
    refresh: bool,
    max_files: usize,
    max_manifest_chars: usize,
    context_budget_chars: Option<usize>,
) -> Result<PreflightContextSummary, Box<dyn std::error::Error>> {
    println!("\nProject context");
    if disabled {
        println!("cache: disabled");
        println!("budgets: max_files={max_files}, max_manifest_chars={max_manifest_chars}");
        let local_change_handoff_chars = print_preflight_local_changes(cwd);
        return Ok(PreflightContextSummary {
            project_context_chars: None,
            project_context_cache_hit: None,
            local_change_handoff_chars,
        });
    }

    let ctx = load_or_build(
        cwd,
        ProjectContextOptions {
            max_files,
            max_manifest_chars,
            refresh,
            ..ProjectContextOptions::default()
        },
    )?;
    println!("cache: {}", if ctx.cache_hit { "hit" } else { "rebuilt" });
    println!("path: {}", ctx.cache_path.display());
    let chars = ctx.text.chars().count();
    println!("chars: {chars}");
    println!("tokens~: {}", estimated_tokens(chars));
    println!(
        "budget fit: {}",
        context_fit_label(chars, effective_context_budget(context_budget_chars))
    );
    println!("budgets: max_files={max_files}, max_manifest_chars={max_manifest_chars}");
    let local_change_handoff_chars = print_preflight_local_changes(cwd);
    Ok(PreflightContextSummary {
        project_context_chars: Some(chars),
        project_context_cache_hit: Some(ctx.cache_hit),
        local_change_handoff_chars,
    })
}

fn print_preflight_local_changes(cwd: &Path) -> Option<usize> {
    println!("\nLocal changes");
    match local_change_summary(cwd, DEFAULT_LOCAL_CHANGE_LINES) {
        Some(changes) => {
            println!("status: {}", changes.summary_line());
            if !changes.changes.is_empty() {
                for line in changes.changes.iter().take(8) {
                    println!("- {line}");
                }
                if changes.changes.len() > 8 || changes.truncated {
                    println!("- ...");
                }
            }
            Some(changes.to_handoff_text().chars().count())
        }
        None => {
            println!("status: unavailable (not a git repository or git unavailable)");
            None
        }
    }
}

#[derive(Debug)]
struct PreflightContextJson {
    summary: PreflightContextSummary,
    project_context: Value,
    local_changes: Value,
}

fn preflight_agent_json(agent: &PreflightAgent, demo: bool) -> Value {
    let model = agent.model.as_deref().unwrap_or("default");
    let effort = agent.effort.as_deref().unwrap_or("default");
    match tool_info(&agent.key) {
        Some(info) => {
            let bin_path = bin_path(info.bin).map(|path| path.display().to_string());
            let installed = bin_path.is_some();
            let caps = make_adapter(info.key)
                .ok()
                .map(|adapter| agent_caps_json(adapter.caps()));
            json!({
                "role": agent.role,
                "key": agent.key.as_str(),
                "pretty": info.pretty,
                "kind": adapter_kind_label(info.kind),
                "bin": info.bin,
                "bin_path": bin_path,
                "installed": installed,
                "model": model,
                "model_override": agent.model.as_deref(),
                "effort": effort,
                "effort_override": agent.effort.as_deref(),
                "suggested_models": info.models,
                "supported_efforts": info.efforts,
                "caps": caps,
                "demo_mock": demo,
            })
        }
        None => json!({
            "role": agent.role,
            "key": agent.key.as_str(),
            "pretty": if demo { "Demo mock" } else { "Unknown tool" },
            "kind": if demo { "mock" } else { "unknown" },
            "bin": null,
            "bin_path": null,
            "installed": false,
            "model": model,
            "model_override": agent.model.as_deref(),
            "effort": effort,
            "effort_override": agent.effort.as_deref(),
            "suggested_models": [],
            "supported_efforts": [],
            "caps": null,
            "demo_mock": demo,
        }),
    }
}

fn roster_advice_for_agents(agents: &[PreflightAgent]) -> Vec<Value> {
    let roster = agents
        .iter()
        .map(|agent| (agent.role, agent.key.as_str()))
        .collect::<Vec<_>>();
    tool_roster_advice_json(&roster)
}

fn roster_advice_for_targets(targets: &[String]) -> Vec<Value> {
    let roster = targets
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let role = match index {
                0 => "drafter",
                1 => "critic",
                2 => "executor",
                _ => "tool",
            };
            (role, key.as_str())
        })
        .collect::<Vec<_>>();
    tool_roster_advice_json(&roster)
}

fn print_roster_advice(agents: &[PreflightAgent]) {
    let advice = roster_advice_for_agents(agents);
    print_roster_advice_items(&advice);
}

fn print_roster_advice_for_targets(targets: &[String]) {
    let advice = roster_advice_for_targets(targets);
    print_roster_advice_items(&advice);
}

fn print_roster_advice_items(advice: &[Value]) {
    if advice.is_empty() {
        return;
    }
    println!("\nRoster advice");
    for item in advice.iter() {
        let level = item["level"].as_str().unwrap_or("info");
        let title = item["title"].as_str().unwrap_or("advice");
        let detail = item["detail"].as_str().unwrap_or("");
        println!("- {level}: {title} - {detail}");
    }
}

fn context_budget_json(context_budget_chars: Option<usize>) -> Value {
    let effective_chars = effective_context_budget(context_budget_chars);
    json!({
        "requested_chars": context_budget_chars,
        "effective_chars": effective_chars,
        "effective_tokens_estimate": effective_chars.map(estimated_tokens),
        "label": context_budget_label(context_budget_chars),
    })
}

fn collect_preflight_local_changes_json(cwd: &Path) -> (Option<usize>, Value) {
    match local_change_summary(cwd, DEFAULT_LOCAL_CHANGE_LINES) {
        Some(changes) => {
            let handoff_text = changes.to_handoff_text();
            let handoff_chars = handoff_text.chars().count();
            (
                Some(handoff_chars),
                local_change_summary_status_json(Some(&changes)),
            )
        }
        None => (None, local_change_summary_status_json(None)),
    }
}

fn collect_preflight_context_json(
    cwd: &Path,
    disabled: bool,
    refresh: bool,
    max_files: usize,
    max_manifest_chars: usize,
    context_budget_chars: Option<usize>,
) -> Result<PreflightContextJson, Box<dyn std::error::Error>> {
    let (local_change_handoff_chars, local_changes) = collect_preflight_local_changes_json(cwd);
    if disabled {
        return Ok(PreflightContextJson {
            summary: PreflightContextSummary {
                project_context_chars: None,
                project_context_cache_hit: None,
                local_change_handoff_chars,
            },
            project_context: project_context_status_json(
                None,
                refresh,
                max_files,
                max_manifest_chars,
                effective_context_budget(context_budget_chars),
            ),
            local_changes,
        });
    }

    let ctx = load_or_build(
        cwd,
        ProjectContextOptions {
            max_files,
            max_manifest_chars,
            refresh,
            ..ProjectContextOptions::default()
        },
    )?;
    let chars = ctx.text.chars().count();
    Ok(PreflightContextJson {
        summary: PreflightContextSummary {
            project_context_chars: Some(chars),
            project_context_cache_hit: Some(ctx.cache_hit),
            local_change_handoff_chars,
        },
        project_context: project_context_status_json(
            Some(&ctx),
            refresh,
            max_files,
            max_manifest_chars,
            effective_context_budget(context_budget_chars),
        ),
        local_changes,
    })
}

#[allow(clippy::too_many_arguments)]
fn print_preflight_dry_run_json(
    mode: &str,
    prompt: &str,
    cwd: &Path,
    agents: &[PreflightAgent],
    turns: usize,
    sandbox: &str,
    sequential: bool,
    include_execution: bool,
    separate_executor: bool,
    context_budget_chars: Option<usize>,
    no_project_context: bool,
    refresh_project_context: bool,
    project_context_max_files: usize,
    project_context_max_manifest_chars: usize,
    demo: bool,
    worktree: Value,
    report_path: Option<&Path>,
    report_json_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = collect_preflight_context_json(
        cwd,
        no_project_context,
        refresh_project_context,
        project_context_max_files,
        project_context_max_manifest_chars,
        context_budget_chars,
    )?;
    let forecast = prompt_forecast(
        prompt,
        agents,
        turns,
        sequential,
        include_execution,
        context_budget_chars,
        &context.summary,
    );
    let session_config = resolved_session_config(
        mode,
        prompt,
        cwd,
        agents,
        turns,
        sandbox,
        sequential,
        include_execution,
        context_budget_chars,
        !no_project_context,
        refresh_project_context,
        project_context_max_files,
        project_context_max_manifest_chars,
        report_path.map(Path::to_path_buf),
        report_json_path.map(Path::to_path_buf),
    );
    let workspace_profile = load_profile_if_exists(cwd, None)?;
    let profile_json = workspace_profile
        .as_ref()
        .map(|profile| profile_status_json(cwd, None, profile))
        .unwrap_or_else(|| {
            json!({
                "enabled": true,
                "present": false,
                "message": "no workspace profile yet; run `tales profile refresh`",
            })
        });
    let candidates = agents
        .iter()
        .map(|agent| agent.key.clone())
        .collect::<Vec<_>>();
    let recommendations = recommendation_chips_json(&recommend_tools(
        &RecommendationInput {
            task: prompt.to_string(),
            candidates,
            prefer_cheaper: include_execution && separate_executor,
            prefer_faster: !sequential,
            needs_execution: include_execution,
        },
        workspace_profile.as_ref(),
    ));
    let advice = build_run_advice(RunAdviceInput {
        forecast: &forecast,
        context: &context.summary,
        effective_context_budget_chars: effective_context_budget(context_budget_chars),
        turns,
        sequential,
        include_execution,
        separate_executor,
    });
    let payload = json!({
        "kind": "tales_preflight",
        "schema_version": 1,
        "build": build_info::json(),
        "mode": mode,
        "demo": demo,
        "cwd": cwd.display().to_string(),
        "task": prompt,
        "turns": turns,
        "planning": if sequential { "sequential" } else { "parallel" },
        "execution": if include_execution {
            json!({
                "enabled": true,
                "shape": if separate_executor { "tiered" } else { "participant" },
            })
        } else {
            json!({
                "enabled": false,
                "shape": null,
            })
        },
        "sandbox": sandbox,
        "session_config": session_config.to_json(),
        "context_budget": context_budget_json(context_budget_chars),
        "report_path": report_path.map(|path| path.display().to_string()),
        "report_json_path": report_json_path.map(|path| path.display().to_string()),
        "worktree": worktree,
            "agents": agents
                .iter()
                .map(|agent| preflight_agent_json(agent, demo))
                .collect::<Vec<_>>(),
            "roster_advice": roster_advice_for_agents(agents),
            "project_context": context.project_context,
            "local_changes": context.local_changes,
            "prompt_forecast": prompt_forecast_to_json(&forecast, turns, sequential),
        "run_advice": run_advice_to_json(&advice),
        "workspace_profile": profile_json,
        "tool_recommendations": recommendations,
        "side_effects": {
            "agents_spawned": false,
            "model_calls": false,
        },
        "status": "ready",
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn planner_count(agents: &[PreflightAgent]) -> usize {
    agents
        .iter()
        .filter(|agent| matches!(agent.role, "drafter" | "critic"))
        .count()
        .max(1)
}

fn prompt_forecast(
    prompt: &str,
    agents: &[PreflightAgent],
    turns: usize,
    sequential: bool,
    include_execution: bool,
    context_budget_chars: Option<usize>,
    context: &PreflightContextSummary,
) -> tales_core::prompt_forecast::PromptForecast {
    forecast_prompt_usage(
        prompt.chars().count(),
        planner_count(agents),
        turns,
        sequential,
        include_execution,
        context,
        effective_context_budget(context_budget_chars),
    )
}

#[allow(clippy::too_many_arguments)]
fn resolved_session_config(
    mode: &str,
    prompt: &str,
    cwd: &Path,
    agents: &[PreflightAgent],
    turns: usize,
    sandbox: &str,
    sequential: bool,
    include_execution: bool,
    context_budget_chars: Option<usize>,
    project_context_enabled: bool,
    refresh_project_context: bool,
    project_context_max_files: usize,
    project_context_max_manifest_chars: usize,
    report_path: Option<PathBuf>,
    report_json_path: Option<PathBuf>,
) -> SessionConfig {
    let seats = agents
        .iter()
        .map(|agent| {
            let mut seat = AgentSeatConfig::new(agent.role, agent.key.clone());
            seat.model = agent.model.clone();
            seat.effort = agent.effort.clone();
            seat.cwd_policy = CwdPolicy::Shared;
            seat.execution_permission = if agent.role == "executor" {
                ExecutionPermission::WorkspaceWrite
            } else {
                ExecutionPermission::ReadOnly
            };
            seat
        })
        .collect::<Vec<_>>();
    let mut allowed_executors = agents
        .iter()
        .filter(|agent| include_execution && agent.role == "executor")
        .map(|agent| agent.key.clone())
        .collect::<Vec<_>>();
    if allowed_executors.is_empty() {
        allowed_executors = agents.iter().map(|agent| agent.key.clone()).collect();
        allowed_executors.sort();
        allowed_executors.dedup();
    }

    SessionConfig {
        schema_version: 1,
        task: prompt.to_string(),
        cwd: cwd.to_path_buf(),
        mode: match mode {
            "run" => SessionMode::Run,
            "discuss" => SessionMode::Discuss,
            "solo" => SessionMode::Solo,
            "eval" => SessionMode::Eval,
            _ => SessionMode::Run,
        },
        turns,
        sequential,
        sandbox: sandbox.to_string(),
        seats,
        prompt_budget: PromptBudgetConfig {
            context_budget_chars: effective_context_budget(context_budget_chars),
            project_context_enabled,
            refresh_project_context,
            project_context_max_files,
            project_context_max_manifest_chars,
            local_change_handoff_lines: DEFAULT_LOCAL_CHANGE_LINES,
        },
        execution_policy: ExecutionPolicy::recommend_only(allowed_executors),
        report_paths: ReportPaths {
            markdown: report_path,
            json: report_json_path,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn print_prompt_forecast(
    prompt: &str,
    agents: &[PreflightAgent],
    turns: usize,
    sequential: bool,
    include_execution: bool,
    separate_executor: bool,
    context_budget_chars: Option<usize>,
    context: &PreflightContextSummary,
) {
    let forecast = prompt_forecast(
        prompt,
        agents,
        turns,
        sequential,
        include_execution,
        context_budget_chars,
        context,
    );
    println!("\nPrompt forecast");
    println!(
        "planned calls: {} base, {} worst-case (planning={}, recommendation={}, execution={})",
        forecast.total_prompts(),
        forecast.worst_case_total_prompts(),
        forecast.planning_prompts,
        forecast.recommendation_prompts,
        forecast.execution_prompts
    );
    if forecast.max_vote_repair_prompts > 0 {
        println!(
            "repair risk: 0..{} extra vote prompt(s); common vote aliases and unambiguous candidate mentions avoid repair",
            forecast.max_vote_repair_prompts
        );
    }
    if sequential {
        println!("planning waits: {turns} sequential planner turn(s)");
    } else {
        let rounds = parallel_rounds(turns, forecast.planner_count);
        println!(
            "planning waits: {rounds} parallel round(s) x {} planner(s)",
            forecast.planner_count
        );
    }
    println!(
        "known local payload: {} chars (~{} tokens)",
        forecast.known_payload_chars,
        estimated_tokens(forecast.known_payload_chars)
    );
    if let Some(chars) = context.project_context_chars {
        let project_context_chars_per_prompt = forecast
            .project_context_payload_chars
            .checked_div(forecast.first_context_prompts)
            .unwrap_or(0);
        println!(
            "project context payload: {project_context_chars_per_prompt} chars x {} first planner prompt(s) = {} chars",
            forecast.first_context_prompts, forecast.project_context_payload_chars
        );
        if let Some(budget) = effective_context_budget(context_budget_chars) {
            if chars > budget {
                println!(
                    "hint: project context source is {chars} chars, budgeted to first-turn payloads by the context cap."
                );
            }
        }
    } else {
        println!("project context payload: disabled/unavailable");
    }
    if include_execution {
        match forecast.local_change_payload_chars {
            0 => println!("local-change handoff payload: unavailable/clean"),
            chars => println!(
                "local-change handoff payload: {chars} chars (~{} tokens)",
                estimated_tokens(chars)
            ),
        }
    }
    println!(
        "explicit context cap: {}",
        context_budget_label(context_budget_chars)
    );
    println!(
        "note: forecast excludes model replies, provider-side resumed memory, media tokenization, and later transcript growth; the session report records actual prompt telemetry."
    );

    let advice = build_run_advice(RunAdviceInput {
        forecast: &forecast,
        context,
        effective_context_budget_chars: effective_context_budget(context_budget_chars),
        turns,
        sequential,
        include_execution,
        separate_executor,
    });
    if !advice.is_empty() {
        println!("\nRun advice");
        for item in advice {
            println!(
                "- [{}] {}: {}",
                item.level.as_str(),
                item.title,
                item.detail
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn print_run_dry_run(
    prompt: &str,
    cwd: &Path,
    agents: &[PreflightAgent],
    turns: usize,
    sandbox: &str,
    sequential: bool,
    context_budget_chars: Option<usize>,
    no_project_context: bool,
    refresh_project_context: bool,
    project_context_max_files: usize,
    project_context_max_manifest_chars: usize,
    demo: bool,
    worktree: bool,
    separate_executor: bool,
    json_output: bool,
    report_path: Option<&Path>,
    report_json_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    if json_output {
        let worktree_json = if worktree {
            let available = !demo && is_git_repo(cwd).await;
            json!({
                "requested": true,
                "available": available,
                "status": if available {
                    "enabled"
                } else if demo {
                    "ignored_demo_mode"
                } else {
                    "ignored_not_git_repository"
                },
            })
        } else {
            json!({
                "requested": false,
                "available": null,
                "status": "disabled",
            })
        };
        return print_preflight_dry_run_json(
            "run",
            prompt,
            cwd,
            agents,
            turns,
            sandbox,
            sequential,
            true,
            separate_executor,
            context_budget_chars,
            no_project_context,
            refresh_project_context,
            project_context_max_files,
            project_context_max_manifest_chars,
            demo,
            worktree_json,
            report_path,
            report_json_path,
        );
    }

    println!("Tales dry run");
    println!("mode: run{}", if demo { " demo" } else { "" });
    println!("cwd: {}", cwd.display());
    println!("task: {prompt}");
    println!("turns: {turns}");
    println!(
        "planning: {}",
        if sequential { "sequential" } else { "parallel" }
    );
    println!(
        "execution: {}",
        if separate_executor {
            "tiered"
        } else {
            "participant"
        }
    );
    println!("sandbox: {sandbox}");
    println!(
        "context budget: {}",
        context_budget_label(context_budget_chars)
    );
    if let Some(path) = report_path {
        println!("report path: {}", path.display());
    }
    if let Some(path) = report_json_path {
        println!("report json path: {}", path.display());
    }
    if worktree {
        let available = !demo && is_git_repo(cwd).await;
        println!(
            "executor worktree: {}",
            if available {
                "enabled"
            } else if demo {
                "ignored in demo mode"
            } else {
                "ignored; cwd is not a git repository"
            }
        );
    } else {
        println!("executor worktree: disabled");
    }

    print_preflight_agents(agents, demo);
    print_roster_advice(agents);
    let context = print_preflight_context(
        cwd,
        no_project_context,
        refresh_project_context,
        project_context_max_files,
        project_context_max_manifest_chars,
        context_budget_chars,
    )?;
    print_prompt_forecast(
        prompt,
        agents,
        turns,
        sequential,
        true,
        separate_executor,
        context_budget_chars,
        &context,
    );
    println!("\nstatus: ready (no agents spawned)");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_discuss_dry_run(
    prompt: &str,
    cwd: &Path,
    agents: &[PreflightAgent],
    turns: usize,
    sandbox: &str,
    sequential: bool,
    context_budget_chars: Option<usize>,
    no_project_context: bool,
    refresh_project_context: bool,
    project_context_max_files: usize,
    project_context_max_manifest_chars: usize,
    demo: bool,
    json_output: bool,
    report_path: Option<&Path>,
    report_json_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    if json_output {
        return print_preflight_dry_run_json(
            "discuss",
            prompt,
            cwd,
            agents,
            turns,
            sandbox,
            sequential,
            false,
            false,
            context_budget_chars,
            no_project_context,
            refresh_project_context,
            project_context_max_files,
            project_context_max_manifest_chars,
            demo,
            json!(null),
            report_path,
            report_json_path,
        );
    }

    println!("Tales dry run");
    println!("mode: discuss{}", if demo { " demo" } else { "" });
    println!("cwd: {}", cwd.display());
    println!("task: {prompt}");
    println!("turns: {turns}");
    println!(
        "planning: {}",
        if sequential { "sequential" } else { "parallel" }
    );
    println!("sandbox: {sandbox}");
    println!(
        "context budget: {}",
        context_budget_label(context_budget_chars)
    );
    if let Some(path) = report_path {
        println!("report path: {}", path.display());
    }
    if let Some(path) = report_json_path {
        println!("report json path: {}", path.display());
    }

    print_preflight_agents(agents, demo);
    print_roster_advice(agents);
    let context = print_preflight_context(
        cwd,
        no_project_context,
        refresh_project_context,
        project_context_max_files,
        project_context_max_manifest_chars,
        context_budget_chars,
    )?;
    print_prompt_forecast(
        prompt,
        agents,
        turns,
        sequential,
        false,
        false,
        context_budget_chars,
        &context,
    );
    println!("\nstatus: ready (no agents spawned)");
    Ok(())
}

fn validate_pipeline_efforts(
    drafter: &str,
    drafter_effort: Option<&str>,
    critic: &str,
    critic_effort: Option<&str>,
    execute: &str,
    execute_effort: Option<&str>,
    separate_executor: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_effort(drafter, drafter_effort)?;
    validate_effort(critic, critic_effort)?;
    if separate_executor {
        validate_effort(execute, execute_effort)?;
    }
    Ok(())
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
    execute_model: Option<String>,
    drafter_effort: Option<String>,
    critic_effort: Option<String>,
    execute_effort: Option<String>,
    turns: usize,
    cwd: Option<String>,
    sandbox: String,
    worktree: bool,
    sequential: bool,
    turn_timeout_secs: Option<u64>,
    context_budget_chars: Option<usize>,
    no_project_context: bool,
    refresh_project_context: bool,
    project_context_max_files: usize,
    project_context_max_manifest_chars: usize,
    demo: bool,
    dry_run: bool,
    json_output: bool,
    report_path: Option<PathBuf>,
    report_json_path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    if json_output && !dry_run {
        return Err("--json requires --dry-run".into());
    }
    let exec_lc = execute.to_lowercase();
    let exec_is_drafter = drafter.to_lowercase() == exec_lc;
    let exec_is_critic = critic.to_lowercase() == exec_lc;
    // The executor may be a planning participant (reuses its planning model) or
    // a SEPARATE tool — the tiered case: strong models plan, a cheap/fast model
    // implements the agreed plan.
    let separate_executor = !exec_is_drafter && !exec_is_critic;

    if !separate_executor && execute_model.is_some() {
        return Err(format!(
            "--execute-model is only valid when --execute ('{execute}') is a different tool \
             from --drafter/--critic (same-tool tiering isn't supported yet)"
        )
        .into());
    }
    if !separate_executor && execute_effort.is_some() {
        return Err(format!(
            "--execute-effort is only valid when --execute ('{execute}') is a different tool \
             from --drafter/--critic"
        )
        .into());
    }
    // Roster safety includes the separate executor (so e.g. duplicate Open Code
    // is rejected), and fail fast on an unknown executor tool.
    if !demo {
        let mut roster_keys = vec![drafter.clone(), critic.clone()];
        if separate_executor {
            roster_keys.push(execute.clone());
        }
        validate_tool_readiness(&roster_keys)?;
        validate_pipeline_efforts(
            &drafter,
            drafter_effort.as_deref(),
            &critic,
            critic_effort.as_deref(),
            &execute,
            execute_effort.as_deref(),
            separate_executor,
        )?;
    }

    let cwd = cwd.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let report_path = resolve_report_path(&cwd, report_path);
    let report_json_path = resolve_report_path(&cwd, report_json_path);
    let mut preflight_agents = vec![
        PreflightAgent {
            role: "drafter",
            key: drafter.clone(),
            model: drafter_model.clone(),
            effort: drafter_effort.clone(),
        },
        PreflightAgent {
            role: "critic",
            key: critic.clone(),
            model: critic_model.clone(),
            effort: critic_effort.clone(),
        },
    ];
    if separate_executor {
        preflight_agents.push(PreflightAgent {
            role: "executor",
            key: execute.clone(),
            model: execute_model.clone(),
            effort: execute_effort.clone(),
        });
    } else {
        let (model, effort) = if exec_is_drafter {
            (drafter_model.clone(), drafter_effort.clone())
        } else {
            (critic_model.clone(), critic_effort.clone())
        };
        preflight_agents.push(PreflightAgent {
            role: "executor",
            key: execute.clone(),
            model,
            effort,
        });
    }
    if dry_run {
        return print_run_dry_run(
            &prompt,
            &cwd,
            &preflight_agents,
            turns,
            &sandbox,
            sequential,
            context_budget_chars,
            no_project_context,
            refresh_project_context,
            project_context_max_files,
            project_context_max_manifest_chars,
            demo,
            worktree,
            separate_executor,
            json_output,
            report_path.as_deref(),
            report_json_path.as_deref(),
        )
        .await;
    }

    let roster = preflight_agents
        .iter()
        .map(|agent| agent.key.clone())
        .collect::<Vec<_>>();
    let artifacts = RunArtifacts::create(
        &cwd,
        "run",
        &prompt,
        &roster,
        report_path.as_deref(),
        report_json_path.as_deref(),
    )?;
    println!("● artifacts: {}", artifacts.relative_dir());

    let (bus, mut commands_rx) = EventBus::new(2048, 64);
    let commands_tx = bus.commands();

    let printer = spawn_printer(&bus);
    let artifact_writer = spawn_artifact_writer(&bus, artifacts.clone());
    let report_path_for_profile = report_path.clone();
    let report_writer = spawn_report_writer(&bus, report_path.clone(), report_json_path.clone());

    // Agents run on acceptEdits (Write/Edit auto-approved). The Claude executor
    // is additionally restricted to file tools so it cannot stall on an
    // unapproved Bash call in headless mode (Write creates parent dirs, so no
    // shell mkdir is needed). Non-executors and Codex are unrestricted.
    let tools_for = |label: &str| -> Option<Vec<String>> {
        if label.to_lowercase() == exec_lc && label.to_lowercase() == "claude" {
            Some(vec![
                "Write".into(),
                "Edit".into(),
                "MultiEdit".into(),
                "Read".into(),
            ])
        } else {
            None
        }
    };

    let drafter_id = Uuid::new_v4();
    let critic_id = Uuid::new_v4();
    let executor_id = Uuid::new_v4(); // only enrolled when the executor is separate
    let exec_id = if exec_is_drafter {
        drafter_id
    } else if exec_is_critic {
        critic_id
    } else {
        executor_id
    };

    // Optional git-worktree isolation: the executor runs in its own worktree,
    // and the result is merged back after execution.
    let use_wt = worktree && !demo && is_git_repo(&cwd).await;
    if worktree && demo {
        eprintln!("note: --worktree ignored in --demo mode");
    } else if worktree && !use_wt {
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
    let drafter_cwd = if exec_is_drafter {
        exec_cwd.clone()
    } else {
        cwd.clone()
    };
    let critic_cwd = if exec_is_critic && !exec_is_drafter {
        exec_cwd.clone()
    } else {
        cwd.clone()
    };
    // A separate executor gets the worktree cwd; the planners stay on the base.
    let executor_cwd = exec_cwd.clone();

    // Run inside a block so that — no matter where it fails — we still abort the
    // printer task and prune the executor's worktree (its on-disk dir + branch).
    let run_result: Result<RunOutcome, Box<dyn std::error::Error>> = async {
        let mut orch = Orchestrator::new(bus.clone());
        // Parallel planning rounds by default (faster, less re-sent context); the
        // round-2 synthesizer still runs a real cross-review. `--sequential` opts out.
        orch.set_parallel_rounds(!sequential);
        configure_orchestrator(&mut orch, turn_timeout_secs, context_budget_chars);
        configure_project_context(
            &mut orch,
            &cwd,
            no_project_context,
            refresh_project_context,
            project_context_max_files,
            project_context_max_manifest_chars,
        )?;
        configure_local_changes(&mut orch, &cwd);
        let planning_turns = demo_planning_turns(turns, sequential, 2);
        if demo {
            orch.add_agent(
                Box::new(MockAdapter::new(demo_planner_responses(
                    &drafter,
                    Role::Drafter,
                    planning_turns,
                    &execute,
                ))),
                mk_ctx(
                    drafter_id,
                    &drafter,
                    &drafter_cwd,
                    drafter_model,
                    drafter_effort,
                    &sandbox,
                    "acceptEdits",
                    tools_for(&drafter),
                ),
                Role::Drafter,
            )
            .await?;
            orch.add_agent(
                Box::new(MockAdapter::new(demo_planner_responses(
                    &critic,
                    Role::Critic,
                    planning_turns,
                    &execute,
                ))),
                mk_ctx(
                    critic_id,
                    &critic,
                    &critic_cwd,
                    critic_model,
                    critic_effort,
                    &sandbox,
                    "acceptEdits",
                    tools_for(&critic),
                ),
                Role::Critic,
            )
            .await?;
        } else {
            orch.add_agent(
                make_adapter(&drafter)?,
                mk_ctx(
                    drafter_id,
                    &drafter,
                    &drafter_cwd,
                    drafter_model,
                    drafter_effort,
                    &sandbox,
                    "acceptEdits",
                    tools_for(&drafter),
                ),
                Role::Drafter,
            )
            .await?;
            orch.add_agent(
                make_adapter(&critic)?,
                mk_ctx(
                    critic_id,
                    &critic,
                    &critic_cwd,
                    critic_model,
                    critic_effort,
                    &sandbox,
                    "acceptEdits",
                    tools_for(&critic),
                ),
                Role::Critic,
            )
            .await?;
        }

        // Tiered execution: enroll a separate executor tool that sits out the
        // debate and implements the agreed plan with its own (cheap/fast) model.
        if separate_executor {
            orch.add_agent(
                if demo {
                    Box::new(MockAdapter::new(demo_executor_responses(&execute)))
                } else {
                    make_adapter(&execute)?
                },
                mk_ctx(
                    executor_id,
                    &execute,
                    &executor_cwd,
                    execute_model.clone(),
                    execute_effort.clone(),
                    &sandbox,
                    "acceptEdits",
                    tools_for(&execute),
                ),
                Role::Executor,
            )
            .await?;
        }

        // Auto-confirm the executor: queued before the run, it is remembered and
        // honored when the gate opens (non-interactive equivalent of /confirm).
        commands_tx
            .send(UserCommand::ConfirmExecution {
                executor: execute.clone(),
            })
            .await?;

        let tier = if separate_executor {
            match &execute_model {
                Some(m) => format!("  (tiered: {drafter}+{critic} plan → {execute}/{m} executes)"),
                None => format!("  (tiered: {drafter}+{critic} plan → {execute} executes)"),
            }
        } else {
            format!("  (executor: {execute})")
        };
        println!(
            "\n=== run{}: {prompt}{tier} ===",
            if demo { " demo" } else { "" }
        );
        let outcome = orch
            .run_interactive(&prompt, turns, &mut commands_rx)
            .await?;
        orch.shutdown().await;
        Ok(outcome)
    }
    .await;

    if let Some(handle) = report_writer {
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }
    sleep(Duration::from_millis(200)).await;
    printer.abort();
    artifact_writer.abort();

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

    let outcome = match run_result {
        Ok(outcome) => {
            artifacts.mark_finished("done", Some(&execute));
            outcome
        }
        Err(e) => {
            artifacts.mark_failed(&e.to_string(), Some(&execute));
            return Err(e);
        }
    };
    update_workspace_profile_after_run(
        &cwd,
        &prompt,
        &execute,
        matches!(&outcome, RunOutcome::Executed { .. }),
        report_path_for_profile.as_deref(),
    );
    println!("\n=== outcome: {outcome:?} ===");
    Ok(())
}

fn update_workspace_profile_after_run(
    cwd: &Path,
    prompt: &str,
    executor: &str,
    approved: bool,
    report_path: Option<&Path>,
) {
    let Ok(mut profile) = load_profile(cwd, None) else {
        return;
    };
    profile.apply_update(ProfileUpdate {
        command: Some("tales run".into()),
        report_path: report_path.map(|path| path.display().to_string()),
        run: Some(run_record_from_report(
            prompt.chars().take(80).collect::<String>(),
            Some(executor.to_string()),
            approved,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_script_quotes_and_includes_flags() {
        let s = build_open_script(
            "/path/to/tales-tui",
            "/proj dir",
            &["claude".into()],
            Some("build it's thing"),
        );
        assert!(s.starts_with("#!/bin/bash\n"), "{s}");
        assert!(s.contains("cd '/proj dir' || exit 1"), "{s}");
        assert!(s.contains("exec '/path/to/tales-tui'"), "{s}");
        assert!(s.contains("--connect 'claude'"), "{s}");
        // The apostrophe in the task is safely escaped.
        assert!(s.contains("--prefill 'build it'\\''s thing'"), "{s}");
    }

    #[test]
    fn open_script_omits_empty_task() {
        let s = build_open_script("tales-tui", "/p", &["codex".into()], Some("   "));
        assert!(!s.contains("--prefill"), "{s}");
        let s2 = build_open_script("tales-tui", "/p", &["codex".into()], None);
        assert!(!s2.contains("--prefill"), "{s2}");
    }

    #[test]
    fn commands_subcommand_parses() {
        let cli = Cli::parse_from(["tales", "commands"]);
        assert!(matches!(cli.command, Some(Command::Commands)));
    }

    #[test]
    fn recover_subcommand_parses() {
        let cli = Cli::parse_from(["tales", "recover", "--latest", "--print"]);
        assert!(matches!(
            cli.command,
            Some(Command::Recover {
                latest: true,
                print: true,
                ..
            })
        ));
    }

    #[test]
    fn recover_runs_sort_by_manifest_update_time() {
        let cwd = temp_test_dir("tales-recover-runs");
        let runs = cwd.join(".tales").join("runs");
        let old = runs.join("100-old");
        let new = runs.join("200-new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(
            old.join("manifest.json"),
            r#"{"task":"old task","status":"planning","updated_unix":100}"#,
        )
        .unwrap();
        std::fs::write(
            new.join("manifest.json"),
            r#"{"task":"new task","status":"done","executor":"codex","updated_unix":200}"#,
        )
        .unwrap();
        std::fs::write(new.join("plan.md"), "# New plan").unwrap();

        let found = collect_recover_runs(&cwd).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].id, "200-new");
        assert_eq!(found[0].task.as_deref(), Some("new task"));
        assert_eq!(found[0].executor.as_deref(), Some("codex"));
        assert_eq!(
            resolve_recover_run(&cwd, &found, "100-old").unwrap().id,
            "100-old"
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn run_artifacts_write_manifest_events_and_plan() {
        let cwd = temp_test_dir("tales-run-artifacts");
        let report_path = cwd.join(".tales").join("report.md");
        let artifacts = RunArtifacts::create(
            &cwd,
            "run",
            "Fix artifact persistence",
            &["claude".into(), "codex".into()],
            Some(&report_path),
            None,
        )
        .unwrap();

        let agent = Uuid::new_v4();
        let mut snapshot = ArtifactSnapshot::new();
        assert!(!snapshot.apply(&OrchestratorEvent::AgentSpawned {
            agent,
            label: "claude".into(),
            session_id: "session-1".into(),
        }));
        assert!(snapshot.apply(&OrchestratorEvent::Message {
            agent,
            text: "Draft the persistence plan.".into(),
        }));
        let recommendation = OrchestratorEvent::RecommendationReady {
            executor: "codex".into(),
            rationale: "Codex can make the local CLI patch.".into(),
            confident: true,
            scores: vec![("codex".into(), 0.9)],
        };
        artifacts.append_event(&recommendation).unwrap();
        assert!(snapshot.apply(&recommendation));
        artifacts
            .write_manifest(&snapshot.status, snapshot.executor.as_deref())
            .unwrap();
        artifacts
            .write_plan_markdown(
                &snapshot.status,
                snapshot.executor.as_deref(),
                &snapshot.transcript,
            )
            .unwrap();

        let manifest = std::fs::read_to_string(&artifacts.manifest_path).unwrap();
        let events = std::fs::read_to_string(&artifacts.events_path).unwrap();
        let plan = std::fs::read_to_string(&artifacts.plan_path).unwrap();

        assert!(manifest.contains("\"mode\": \"run\""), "{manifest}");
        assert!(
            manifest.contains("\"status\": \"recommended\""),
            "{manifest}"
        );
        assert!(
            manifest.contains(report_path.to_string_lossy().as_ref()),
            "{manifest}"
        );
        assert!(events.contains("\"kind\":\"run_started\""), "{events}");
        assert!(
            events.contains("\"kind\":\"recommendation_ready\""),
            "{events}"
        );
        assert!(plan.contains("Status: recommended"), "{plan}");
        assert!(plan.contains("Executor: Codex"), "{plan}");
        assert!(plan.contains("Draft the persistence plan."), "{plan}");

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn report_paths_resolve_relative_to_cwd() {
        let cwd = PathBuf::from("/tmp/tales-cwd");
        assert_eq!(
            resolve_report_path(&cwd, Some(PathBuf::from(".tales/report.md"))).unwrap(),
            PathBuf::from("/tmp/tales-cwd/.tales/report.md")
        );
        assert_eq!(
            resolve_report_path(&cwd, Some(PathBuf::from("/tmp/report.md"))).unwrap(),
            PathBuf::from("/tmp/report.md")
        );
    }

    fn temp_test_dir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{label}-{}-{unique}", std::process::id()))
    }

    #[test]
    fn doctor_targets_default_to_primary_roster() {
        let (targets, strict) = doctor_targets(false, &[]).unwrap();
        assert!(strict);
        assert_eq!(targets, vec!["claude", "codex"]);
    }

    #[test]
    fn doctor_targets_all_inventory_is_not_strict() {
        let (targets, strict) = doctor_targets(true, &[]).unwrap();
        assert!(!strict);
        assert_eq!(targets.len(), KNOWN_TOOLS.len());
        assert!(targets.iter().any(|tool| tool == "claude"));
    }

    #[test]
    fn doctor_targets_rejects_mixed_all_and_agents() {
        let err = doctor_targets(true, &["codex".into()]).unwrap_err();
        assert!(err.contains("--all"), "{err}");
    }

    #[test]
    fn doctor_caps_are_explicit() {
        let caps = AgentCaps {
            midturn_injection: true,
            structured_output: false,
            resumable: true,
            mcp: false,
        };
        assert_eq!(
            format_caps(caps),
            "resume=true, midturn=true, structured=false, mcp=false"
        );
    }

    #[test]
    fn doctor_json_inventory_is_machine_readable_without_strict_failures() {
        let targets = vec!["claude".to_string()];
        let (payload, failures) = doctor_json_report(
            &PathBuf::from("/tmp/tales-doctor-json"),
            &targets,
            false,
            true,
            false,
            220,
            1200,
            None,
        )
        .unwrap();

        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(payload["kind"], "tales_doctor");
        assert_eq!(payload["schema_version"], 1);
        assert_eq!(payload["mode"], "inventory");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["project_context"]["enabled"], false);
        assert_eq!(payload["tools"][0]["key"], "claude");
        assert_eq!(payload["tools"][0]["blocking"], false);
        assert!(payload["tools"][0]["caps"].is_object());
        assert!(payload["roster_advice"].as_array().unwrap().is_empty());
    }

    #[test]
    fn doctor_json_reports_unknown_strict_tool_as_failure() {
        let targets = vec!["nope".to_string()];
        let (payload, failures) = doctor_json_report(
            &PathBuf::from("/tmp/tales-doctor-json"),
            &targets,
            true,
            true,
            false,
            220,
            1200,
            None,
        )
        .unwrap();

        assert_eq!(payload["status"], "attention_required");
        assert_eq!(payload["tools"][0]["known"], false);
        assert_eq!(payload["tools"][0]["blocking"], true);
        assert!(failures.iter().any(|failure| failure.contains("unknown")));
        assert!(!payload["failures"].as_array().unwrap().is_empty());
        assert_eq!(payload["roster_advice"][0]["key"], "unknown_tool");
    }

    #[test]
    fn roster_advice_flags_single_tool_debate() {
        let agents = vec![
            PreflightAgent {
                role: "drafter",
                key: "claude".into(),
                model: None,
                effort: None,
            },
            PreflightAgent {
                role: "critic",
                key: "claude".into(),
                model: None,
                effort: None,
            },
        ];

        let advice = roster_advice_for_agents(&agents);

        assert!(advice.iter().any(|item| {
            item["key"] == "planner_diversity"
                && item["level"] == "warn"
                && item["title"] == "Single-tool debate"
        }));
    }

    #[test]
    fn context_budget_labels_match_cli_semantics() {
        assert_eq!(
            context_budget_label(None),
            "default 24000 chars (~6000 tokens)"
        );
        assert_eq!(context_budget_label(Some(0)), "unlimited");
        assert_eq!(
            context_budget_label(Some(4096)),
            "4096 chars (~1024 tokens)"
        );
    }

    #[test]
    fn context_fit_label_reports_budget_pressure() {
        assert_eq!(
            context_fit_label(6000, Some(24000)),
            "6000/24000 chars (25%, ~1500/6000 tokens) - fits"
        );
        assert_eq!(
            context_fit_label(25000, Some(24000)),
            "25000/24000 chars (105%, ~6250/6000 tokens) - over budget"
        );
        assert_eq!(
            context_fit_label(6000, None),
            "6000 chars (~1500 tokens) - unlimited budget"
        );
    }

    #[test]
    fn context_json_serializer_can_include_or_omit_text() {
        let ctx = tales_core::project_context::ProjectContext {
            text: "Project context\n- src/lib.rs\n".into(),
            cache_hit: true,
            cache_path: PathBuf::from("/tmp/tales-context.txt"),
        };
        let mut metadata = project_context_status_json(Some(&ctx), false, 220, 1200, Some(24_000));
        metadata
            .as_object_mut()
            .unwrap()
            .insert("text_included".into(), json!(false));

        assert_eq!(metadata["enabled"], true);
        assert_eq!(metadata["cache_hit"], true);
        assert_eq!(metadata["text_included"], false);
        assert!(metadata.get("text").is_none());

        metadata
            .as_object_mut()
            .unwrap()
            .insert("text".into(), json!(ctx.text.as_str()));
        metadata
            .as_object_mut()
            .unwrap()
            .insert("text_included".into(), json!(true));
        assert_eq!(metadata["text_included"], true);
        assert!(metadata["text"].as_str().unwrap().contains("src/lib.rs"));
    }

    #[test]
    fn prompt_forecast_counts_parallel_run_payload() {
        let agents = vec![
            PreflightAgent {
                role: "drafter",
                key: "claude".into(),
                model: None,
                effort: None,
            },
            PreflightAgent {
                role: "critic",
                key: "codex".into(),
                model: None,
                effort: None,
            },
            PreflightAgent {
                role: "executor",
                key: "claude".into(),
                model: None,
                effort: None,
            },
        ];
        let context = PreflightContextSummary {
            project_context_chars: Some(1_000),
            project_context_cache_hit: Some(true),
            local_change_handoff_chars: Some(200),
        };

        let forecast = prompt_forecast("task", &agents, 4, false, true, None, &context);

        assert_eq!(forecast.planner_count, 2);
        assert_eq!(forecast.planning_prompts, 4);
        assert_eq!(forecast.recommendation_prompts, 2);
        assert_eq!(forecast.execution_prompts, 1);
        assert_eq!(forecast.max_vote_repair_prompts, 2);
        assert_eq!(forecast.first_context_prompts, 2);
        assert_eq!(forecast.project_context_payload_chars, 2_000);
        assert_eq!(forecast.local_change_payload_chars, 200);
        assert_eq!(forecast.total_prompts(), 7);
        assert_eq!(forecast.known_payload_chars, 2_228);
    }

    #[test]
    fn prompt_forecast_discuss_has_no_execution_payload() {
        let agents = vec![
            PreflightAgent {
                role: "drafter",
                key: "claude".into(),
                model: None,
                effort: None,
            },
            PreflightAgent {
                role: "critic",
                key: "codex".into(),
                model: None,
                effort: None,
            },
        ];
        let context = PreflightContextSummary {
            project_context_chars: None,
            project_context_cache_hit: None,
            local_change_handoff_chars: Some(200),
        };

        let forecast = prompt_forecast("task", &agents, 3, true, false, None, &context);

        assert_eq!(forecast.planning_prompts, 3);
        assert_eq!(forecast.recommendation_prompts, 0);
        assert_eq!(forecast.execution_prompts, 0);
        assert_eq!(forecast.max_vote_repair_prompts, 0);
        assert_eq!(forecast.first_context_prompts, 0);
        assert_eq!(forecast.local_change_payload_chars, 0);
        assert_eq!(forecast.total_prompts(), 3);
        assert_eq!(forecast.known_payload_chars, 12);
    }

    #[test]
    fn preflight_agent_json_exposes_tool_metadata() {
        let agent = PreflightAgent {
            role: "drafter",
            key: "claude".into(),
            model: Some("opus".into()),
            effort: None,
        };

        let value = preflight_agent_json(&agent, true);

        assert_eq!(value["role"], "drafter");
        assert_eq!(value["key"], "claude");
        assert_eq!(value["model"], "opus");
        assert_eq!(value["demo_mock"], true);
        assert!(value["caps"].is_object());
    }

    #[test]
    fn shared_prompt_forecast_json_reports_calls_and_payloads() {
        let forecast = tales_core::prompt_forecast::PromptForecast {
            planner_count: 2,
            planning_prompts: 4,
            recommendation_prompts: 2,
            execution_prompts: 1,
            max_vote_repair_prompts: 2,
            first_context_prompts: 2,
            project_context_payload_chars: 1_200,
            local_change_payload_chars: 400,
            known_payload_chars: 1_700,
        };

        let value = prompt_forecast_to_json(&forecast, 4, false);

        assert_eq!(value["planned_calls"]["total"], 7);
        assert_eq!(value["planned_calls"]["base"], 7);
        assert_eq!(value["planned_calls"]["worst_case"], 9);
        assert_eq!(value["planning_waits"]["mode"], "parallel");
        assert_eq!(
            value["payloads"]["project_context_chars_per_first_prompt"],
            600
        );
        assert_eq!(value["repair_risk"]["min_vote_repair_prompts"], 0);
        assert_eq!(value["repair_risk"]["max_vote_repair_prompts"], 2);
        assert_eq!(value["repair_risk"]["worst_case_total_prompts"], 9);
    }
}
