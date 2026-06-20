//! The agent abstraction — the heart of Tales.
//!
//! Every AI tool (Claude Code, Codex, future adapters) is an [`AgentAdapter`]
//! that produces a stream of normalized [`AgentEvent`]s and consumes
//! [`AgentCommand`]s. The differences between tools — Claude's bidirectional
//! stream-json vs Codex's turn-based exec/resume — are expressed only through
//! [`AgentCaps`], never by branching on the concrete type upstream.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::{AgentId, Result, TokenUsage};

pub mod claude;
pub mod codex;
pub mod generic;
pub mod mock;
pub mod opencode;

/// Monotonic per-agent turn counter.
pub type TurnId = u64;

/// What an adapter can and cannot do. The engine schedules around these flags
/// instead of asking "is this Claude or Codex".
#[derive(Clone, Copy, Debug)]
pub struct AgentCaps {
    /// Can accept a new message *during* an in-flight turn (Claude: yes,
    /// Codex: no — injected messages are folded into the next turn).
    pub midturn_injection: bool,
    /// Supports schema-constrained structured output.
    pub structured_output: bool,
    /// Supports resuming a prior session by id.
    pub resumable: bool,
    /// Supports attaching MCP servers.
    pub mcp: bool,
}

/// Normalized inbound events: agent → orchestrator. The engine and every
/// frontend see only this enum.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// The underlying CLI is up; `native_session_id` is its own session handle
    /// (used for resume).
    SessionReady {
        agent: AgentId,
        native_session_id: String,
    },
    /// The skills / slash-commands this tool exposes (discovered at startup).
    Skills { agent: AgentId, skills: Vec<String> },
    /// A new turn began.
    TurnStarted { agent: AgentId, turn: TurnId },
    /// A streamed token chunk.
    TokenDelta {
        agent: AgentId,
        turn: TurnId,
        text: String,
    },
    /// The full text of a completed assistant message.
    MessageFinal {
        agent: AgentId,
        turn: TurnId,
        text: String,
    },
    /// The agent invoked a tool.
    ToolCall {
        agent: AgentId,
        turn: TurnId,
        name: String,
        input: Value,
    },
    /// The turn finished; `cost_usd` is cumulative reported cost if available.
    /// `token_usage` is provider-reported token usage for this completed turn,
    /// when the adapter exposes it.
    TurnComplete {
        agent: AgentId,
        turn: TurnId,
        cost_usd: Option<f64>,
        token_usage: Option<TokenUsage>,
    },
    /// A recoverable or fatal error from the agent.
    Error {
        agent: AgentId,
        message: String,
        fatal: bool,
    },
    /// The process exited.
    Exited { agent: AgentId, code: Option<i32> },
}

/// A media file (image or PDF) to attach to a turn so the agent can see it.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub path: PathBuf,
}

impl Attachment {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    /// Lowercased file extension (no dot), if any.
    pub fn ext(&self) -> String {
        self.path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
    }
    pub fn is_image(&self) -> bool {
        matches!(self.ext().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
    }
    pub fn is_pdf(&self) -> bool {
        self.ext() == "pdf"
    }
    /// File name for display.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string()
    }
}

/// Normalized outbound commands: orchestrator → agent.
#[derive(Clone, Debug)]
pub enum AgentCommand {
    /// Begin a new turn with `prompt` and any media attachments to share.
    StartTurn {
        prompt: String,
        attachments: Vec<Attachment>,
    },
    /// Inject a message. Honored live only if [`AgentCaps::midturn_injection`];
    /// otherwise the adapter queues it for the next turn.
    InjectMessage { text: String },
    /// Gracefully end the session (closes stdin → CLI exits).
    Shutdown,
}

/// Everything an adapter needs to launch a session.
#[derive(Clone, Debug)]
pub struct SpawnCtx {
    pub agent: AgentId,
    pub label: String,
    /// The working directory — in v1 this is the agent's own git worktree.
    pub cwd: PathBuf,
    /// Model alias/id, or `None` for the CLI default.
    pub model: Option<String>,
    /// Reasoning-effort level (e.g. Codex `low` | `medium` | `high`), or `None`
    /// for the tool default. Only adapters whose tool advertises efforts use it.
    pub effort: Option<String>,
    /// Pre-authorized permission mode (Claude) so headless runs never hang on a
    /// prompt. Ignored by adapters that don't use it.
    pub permission_mode: String,
    /// Sandbox policy (Codex): `read-only` | `workspace-write` |
    /// `danger-full-access`. Ignored by adapters that don't use it.
    pub sandbox: String,
    /// If set, restricts the agent to these tools (Claude: `--allowedTools`).
    /// Used to keep an executor on non-blocking file tools (Write/Edit) so it
    /// can't hang on an unapproved Bash call in headless mode.
    pub allowed_tools: Option<Vec<String>>,
}

/// Which built-in adapter drives a tool. Tools with a bespoke adapter name it
/// here; everything else rides the stateless [`generic`] adapter, configured
/// from the registry row — so hooking a new CLI is a data change, not new code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterKind {
    /// Claude Code — persistent bidirectional stream-json.
    Claude,
    /// Codex — turn-based `codex exec --json`.
    Codex,
    /// Open Code — turn-based `opencode run`, cwd-keyed session continuity.
    OpenCode,
    /// Any other turn-based CLI that prints its reply to stdout, driven by the
    /// [`generic`] adapter via the row's `run_args` / `model_flag` / `prompt_flag`.
    Generic,
}

/// A tool Tales knows how to drive: its key, display name, CLI binary, install
/// hint, which adapter runs it, and the knobs a picker can offer (models,
/// efforts). Frontends use this to present a "connect your tools" picker without
/// hard-coding the roster. Adding a turn-based CLI is just a new row here.
#[derive(Clone, Copy, Debug)]
pub struct ToolInfo {
    /// Lowercase key used everywhere as the agent label (e.g. `"claude"`).
    pub key: &'static str,
    /// Human-facing name (e.g. `"Claude Code"`).
    pub pretty: &'static str,
    /// The CLI executable that must be on `PATH`.
    pub bin: &'static str,
    /// How to install it, shown when it isn't found.
    pub install: &'static str,
    /// Which built-in adapter drives this tool.
    pub kind: AdapterKind,
    /// Suggested model ids/aliases a picker can cycle through. Advisory — a user
    /// can still pass any model string; empty means "no suggestions, type one".
    pub models: &'static [&'static str],
    /// Reasoning-effort levels this tool understands (e.g. Codex
    /// `low`/`medium`/`high`), or empty if it has no effort knob.
    pub efforts: &'static [&'static str],
    /// Whether the tool can resume or retain a session across turns.
    pub supports_resume: bool,
    /// Whether the tool can be asked for schema-constrained output.
    pub supports_structured_output: bool,
    /// Whether Tales can pass a model override to the tool.
    pub supports_model_override: bool,
    /// Whether Tales can pass a reasoning effort override to the tool.
    pub supports_effort: bool,
    /// Whether Tales can pass a sandbox policy to the tool.
    pub supports_sandbox: bool,
    /// Whether Tales can start the tool in a specific working directory.
    pub supports_cwd: bool,
    /// Whether the adapter can run without interactive terminal focus.
    pub supports_headless: bool,
    /// Startup cost class used by deterministic recommendations.
    pub startup_cost: &'static str,
    /// Default quality/strength class used by deterministic recommendations.
    pub default_strength: &'static str,
    /// Default price class used by deterministic recommendations.
    pub default_price_tier: &'static str,
    /// `Generic` only: fixed args before the prompt (e.g. `["run"]` for a
    /// `<bin> run <prompt>` CLI). Ignored by bespoke adapters.
    pub run_args: &'static [&'static str],
    /// `Generic` only: the flag that selects a model (e.g. `"--model"` / `"-m"`),
    /// or `""` if the tool takes no model flag.
    pub model_flag: &'static str,
    /// `Generic` only: the flag the prompt is passed with (e.g. `"-p"`), or `""`
    /// when the prompt is the trailing positional argument.
    pub prompt_flag: &'static str,
}

/// Every tool Tales can connect, in display order. Tools with a bespoke adapter
/// (`Claude` / `Codex` / `OpenCode`) name it via [`AdapterKind`]; the rest ride
/// the stateless `Generic` adapter, so adding a turn-based CLI that prints to
/// stdout is just a new row — the picker, the CLI, and the orchestrator all read
/// from this list. The `Generic` rows below encode a best-effort non-interactive
/// invocation; tweak `run_args`/`model_flag`/`prompt_flag` if a CLI differs.
pub const KNOWN_TOOLS: &[ToolInfo] = &[
    ToolInfo {
        key: "claude",
        pretty: "Claude Code",
        bin: "claude",
        install:
            "https://docs.anthropic.com/en/docs/claude-code  (npm i -g @anthropic-ai/claude-code)",
        kind: AdapterKind::Claude,
        models: &["opus", "sonnet", "haiku"],
        efforts: &["low", "medium", "high", "xhigh", "max"],
        supports_resume: true,
        supports_structured_output: false,
        supports_model_override: true,
        supports_effort: true,
        supports_sandbox: false,
        supports_cwd: true,
        supports_headless: true,
        startup_cost: "medium",
        default_strength: "frontier",
        default_price_tier: "high",
        run_args: &[],
        model_flag: "",
        prompt_flag: "",
    },
    ToolInfo {
        key: "codex",
        pretty: "Codex",
        bin: "codex",
        install: "npm i -g @openai/codex",
        kind: AdapterKind::Codex,
        models: &["gpt-5-codex", "gpt-5"],
        efforts: &["low", "medium", "high"],
        supports_resume: false,
        supports_structured_output: true,
        supports_model_override: true,
        supports_effort: true,
        supports_sandbox: true,
        supports_cwd: true,
        supports_headless: true,
        startup_cost: "low",
        default_strength: "frontier",
        default_price_tier: "medium",
        run_args: &[],
        model_flag: "",
        prompt_flag: "",
    },
    ToolInfo {
        key: "opencode",
        pretty: "Open Code",
        bin: "opencode",
        install: "npm i -g opencode-ai   (or: brew install sst/tap/opencode)",
        kind: AdapterKind::OpenCode,
        models: &[],
        efforts: &[],
        supports_resume: true,
        supports_structured_output: false,
        supports_model_override: true,
        supports_effort: false,
        supports_sandbox: false,
        supports_cwd: true,
        supports_headless: true,
        startup_cost: "low",
        default_strength: "general",
        default_price_tier: "medium",
        run_args: &["run"],
        model_flag: "--model",
        prompt_flag: "",
    },
    ToolInfo {
        key: "gemini",
        pretty: "Gemini CLI",
        bin: "gemini",
        install: "npm i -g @google/gemini-cli",
        kind: AdapterKind::Generic,
        models: &["gemini-2.5-pro", "gemini-2.5-flash"],
        efforts: &[],
        supports_resume: false,
        supports_structured_output: false,
        supports_model_override: true,
        supports_effort: false,
        supports_sandbox: false,
        supports_cwd: true,
        supports_headless: true,
        startup_cost: "low",
        default_strength: "frontier",
        default_price_tier: "medium",
        run_args: &[],
        model_flag: "-m",
        prompt_flag: "-p",
    },
    ToolInfo {
        key: "glm",
        pretty: "GLM",
        bin: "glm",
        install: "see your GLM/Zhipu CLI vendor for install",
        kind: AdapterKind::Generic,
        models: &[],
        efforts: &[],
        supports_resume: false,
        supports_structured_output: false,
        supports_model_override: true,
        supports_effort: false,
        supports_sandbox: false,
        supports_cwd: true,
        supports_headless: true,
        startup_cost: "low",
        default_strength: "general",
        default_price_tier: "low",
        run_args: &[],
        model_flag: "-m",
        prompt_flag: "",
    },
    ToolInfo {
        key: "kimi",
        pretty: "Kimi",
        bin: "kimi",
        install: "see Moonshot Kimi CLI for install",
        kind: AdapterKind::Generic,
        models: &[],
        efforts: &[],
        supports_resume: false,
        supports_structured_output: false,
        supports_model_override: true,
        supports_effort: false,
        supports_sandbox: false,
        supports_cwd: true,
        supports_headless: true,
        startup_cost: "low",
        default_strength: "general",
        default_price_tier: "low",
        run_args: &[],
        model_flag: "-m",
        prompt_flag: "",
    },
];

/// Look up a known tool by its (case-insensitive) key.
pub fn tool_info(key: &str) -> Option<&'static ToolInfo> {
    KNOWN_TOOLS.iter().find(|t| t.key.eq_ignore_ascii_case(key))
}

/// Whether `bin` resolves to an executable on `PATH` — a dependency-free `which`
/// so a frontend can show which tools are actually installed. On unix it also
/// checks the execute bit; elsewhere it accepts any matching file (incl. `.exe`).
pub fn bin_path(bin: &str) -> Option<PathBuf> {
    let direct = Path::new(bin);
    if direct.components().count() > 1 || direct.is_absolute() {
        return is_executable(direct).then(|| direct.to_path_buf());
    }

    let path = std::env::var_os("PATH")?;
    let candidates = |dir: &std::path::Path| -> Vec<PathBuf> {
        if cfg!(windows) {
            vec![dir.join(bin), dir.join(format!("{bin}.exe"))]
        } else {
            vec![dir.join(bin)]
        }
    };
    for dir in std::env::split_paths(&path) {
        for cand in candidates(&dir) {
            if is_executable(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

pub fn bin_on_path(bin: &str) -> bool {
    bin_path(bin).is_some()
}

pub fn adapter_kind_label(kind: AdapterKind) -> &'static str {
    match kind {
        AdapterKind::Claude => "claude-stream",
        AdapterKind::Codex => "codex-exec",
        AdapterKind::OpenCode => "opencode-run",
        AdapterKind::Generic => "generic-cli",
    }
}

pub fn agent_caps_json(caps: AgentCaps) -> Value {
    json!({
        "resumable": caps.resumable,
        "resume": caps.resumable,
        "midturn_injection": caps.midturn_injection,
        "midturn": caps.midturn_injection,
        "structured_output": caps.structured_output,
        "structured": caps.structured_output,
        "mcp": caps.mcp,
    })
}

pub fn tool_info_status_json(info: &ToolInfo) -> Value {
    let path = bin_path(info.bin);
    let caps = make_adapter(info.key)
        .ok()
        .map(|adapter| agent_caps_json(adapter.caps()));
    json!({
        "key": info.key,
        "known": true,
        "pretty": info.pretty,
        "kind": adapter_kind_label(info.kind),
        "bin": info.bin,
        "path": path.as_ref().map(|path| path.display().to_string()),
        "bin_path": path.as_ref().map(|path| path.display().to_string()),
        "installed": path.is_some(),
        "install": info.install,
        "models": info.models,
        "suggested_models": info.models,
        "efforts": info.efforts,
        "supported_efforts": info.efforts,
        "capabilities": {
            "supports_resume": info.supports_resume,
            "supports_structured_output": info.supports_structured_output,
            "supports_model_override": info.supports_model_override,
            "supports_effort": info.supports_effort,
            "supports_sandbox": info.supports_sandbox,
            "supports_cwd": info.supports_cwd,
            "supports_headless": info.supports_headless,
            "startup_cost": info.startup_cost,
            "default_strength": info.default_strength,
            "default_price_tier": info.default_price_tier,
        },
        "caps": caps,
    })
}

pub fn known_tools_status_json() -> Value {
    json!({
        "kind": "tales_tools",
        "schema_version": 1,
        "tools": KNOWN_TOOLS
            .iter()
            .map(tool_info_status_json)
            .collect::<Vec<_>>(),
    })
}

pub fn tool_roster_advice_json(roster: &[(&str, &str)]) -> Vec<Value> {
    let mut advice = Vec::new();
    let planners: Vec<(&str, &str)> = roster
        .iter()
        .copied()
        .filter(|(role, _)| matches!(*role, "drafter" | "critic"))
        .collect();
    let executor = roster.iter().copied().find(|(role, _)| *role == "executor");

    let mut unknown = Vec::new();
    for (role, key) in roster {
        if tool_info(key).is_none() {
            unknown.push((*role, *key));
        }
    }
    for (role, key) in unknown {
        advice.push(roster_advice_item(
            "unknown_tool",
            "warn",
            "Unknown tool",
            format!("{role} uses '{key}', which is not in the Tales tool registry."),
            Some(role),
            Some(key),
        ));
    }

    if planners.len() >= 2 {
        let first = planners[0].1;
        if planners
            .iter()
            .all(|(_, key)| key.eq_ignore_ascii_case(first))
        {
            advice.push(roster_advice_item(
                "planner_diversity",
                "warn",
                "Single-tool debate",
                format!(
                    "Drafter and critic both use {first}; this can still self-review, but cross-model disagreement is weaker."
                ),
                None,
                Some(first),
            ));
        } else {
            advice.push(roster_advice_item(
                "planner_diversity",
                "good",
                "Diverse planners",
                "Drafter and critic use different tools, so Tales gets a stronger second opinion."
                    .to_string(),
                None,
                None,
            ));
        }

        let turn_based = planners
            .iter()
            .filter_map(|(role, key)| {
                let caps = caps_for_tool(key)?;
                (!caps.midturn_injection).then_some((*role, *key))
            })
            .collect::<Vec<_>>();
        if turn_based.len() == planners.len() {
            advice.push(roster_advice_item(
                "live_interjection",
                "info",
                "Turn-based planners",
                "Human notes are folded into the next turn for every planner; keep turns short when you expect frequent steering."
                    .to_string(),
                None,
                None,
            ));
        } else {
            advice.push(roster_advice_item(
                "live_interjection",
                "good",
                "Live steering available",
                "At least one planner can receive mid-turn human notes.".to_string(),
                None,
                None,
            ));
        }

        let plain_voters = planners
            .iter()
            .filter_map(|(role, key)| {
                let caps = caps_for_tool(key)?;
                (!caps.structured_output).then_some((*role, *key))
            })
            .collect::<Vec<_>>();
        if !plain_voters.is_empty() {
            let labels = plain_voters
                .iter()
                .map(|(role, key)| format!("{role}:{key}"))
                .collect::<Vec<_>>()
                .join(", ");
            advice.push(roster_advice_item(
                "structured_votes",
                "warn",
                "Plain-text vote risk",
                format!(
                    "{labels} cannot guarantee schema output; Tales will salvage common vote aliases and single-candidate prose, but repair prompts are more likely."
                ),
                None,
                None,
            ));
        } else {
            advice.push(roster_advice_item(
                "structured_votes",
                "good",
                "Structured votes",
                "Planner tools advertise structured output, reducing recommendation repair turns."
                    .to_string(),
                None,
                None,
            ));
        }
    }

    if let Some((_, executor_key)) = executor {
        let planner_reuse = planners
            .iter()
            .any(|(_, key)| key.eq_ignore_ascii_case(executor_key));
        if planner_reuse {
            advice.push(roster_advice_item(
                "executor_shape",
                "info",
                "Participant executor",
                format!(
                    "{executor_key} also participates in planning; execution can reuse context, but planning and typing are not cost-tiered."
                ),
                Some("executor"),
                Some(executor_key),
            ));
        } else {
            advice.push(roster_advice_item(
                "executor_shape",
                "good",
                "Separate executor",
                format!(
                    "{executor_key} is separate from the planners, so high-judgment planning can hand off to a cheaper or faster typing tool."
                ),
                Some("executor"),
                Some(executor_key),
            ));
        }

        if let Some(caps) = caps_for_tool(executor_key) {
            if caps.resumable {
                advice.push(roster_advice_item(
                    "executor_context",
                    "good",
                    "Resumable executor",
                    format!(
                        "{executor_key} can maintain native session context; Tales can keep execution handoffs compact."
                    ),
                    Some("executor"),
                    Some(executor_key),
                ));
            } else {
                advice.push(roster_advice_item(
                    "executor_context",
                    "warn",
                    "Stateless executor",
                    format!(
                        "{executor_key} starts fresh each turn; keep project context enabled and handoffs compact to avoid repeated tokens."
                    ),
                    Some("executor"),
                    Some(executor_key),
                ));
            }
        }
    }

    advice
}

fn caps_for_tool(key: &str) -> Option<AgentCaps> {
    make_adapter(key).ok().map(|adapter| adapter.caps())
}

fn roster_advice_item(
    key: &'static str,
    level: &'static str,
    title: &'static str,
    detail: String,
    role: Option<&str>,
    tool: Option<&str>,
) -> Value {
    json!({
        "key": key,
        "level": level,
        "title": title,
        "detail": detail,
        "role": role,
        "tool": tool,
    })
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
}

/// Whether two instances of this tool can safely share one working directory.
/// Claude (per-session `--session-id`) and Codex (captured `thread_id`) isolate
/// co-located agents; Open Code keys session continuity on the cwd, so two of
/// them would resume each other's `--continue` session.
fn coresident_safe(key: &str) -> bool {
    !key.eq_ignore_ascii_case("opencode")
}

/// Reject a roster that would put two cwd-session tools (currently Open Code) in
/// the same run — they share a directory and would cross-contaminate sessions.
/// Called by every frontend before enrolling agents.
pub fn validate_roster(tool_keys: &[String]) -> Result<()> {
    for tool in tool_keys {
        if coresident_safe(tool) {
            continue;
        }
        let count = tool_keys
            .iter()
            .filter(|k| k.eq_ignore_ascii_case(tool))
            .count();
        if count > 1 {
            let pretty = tool_info(tool).map(|t| t.pretty).unwrap_or(tool.as_str());
            return Err(crate::TalesError::Other(format!(
                "can't connect {pretty} twice in one run — its session continuity is \
                 per-directory, so two would cross-contaminate. Use it as a single participant."
            )));
        }
    }
    Ok(())
}

/// Fail before any model process is spawned if the requested roster cannot run on
/// this machine. This is stricter than [`validate_roster`]: it also catches
/// unknown tool keys and missing CLI binaries.
pub fn validate_tool_readiness(tool_keys: &[String]) -> Result<()> {
    validate_roster(tool_keys)?;

    let mut seen = Vec::<String>::new();
    let mut issues = Vec::<String>::new();
    for key in tool_keys {
        if seen.iter().any(|seen| seen.eq_ignore_ascii_case(key)) {
            continue;
        }
        seen.push(key.clone());

        let Some(info) = tool_info(key) else {
            let known = KNOWN_TOOLS
                .iter()
                .map(|tool| tool.key)
                .collect::<Vec<_>>()
                .join(" | ");
            issues.push(format!("unknown agent '{key}' (known: {known})"));
            continue;
        };
        if bin_path(info.bin).is_none() {
            issues.push(format!(
                "{} CLI '{}' not found on PATH; install: {}",
                info.pretty, info.bin, info.install
            ));
        }
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(crate::TalesError::Other(format!(
            "tool preflight failed: {}",
            issues.join("; ")
        )))
    }
}

/// Validate a requested reasoning effort for one tool. Model strings remain
/// advisory/free-form, but effort levels are finite tool-specific CLI knobs.
pub fn validate_effort(tool_key: &str, effort: Option<&str>) -> Result<()> {
    let Some(effort) = effort.filter(|effort| !effort.trim().is_empty()) else {
        return Ok(());
    };
    let Some(info) = tool_info(tool_key) else {
        let known = KNOWN_TOOLS
            .iter()
            .map(|tool| tool.key)
            .collect::<Vec<_>>()
            .join(" | ");
        return Err(crate::TalesError::Other(format!(
            "unknown agent '{tool_key}' (known: {known})"
        )));
    };
    if info.efforts.is_empty() {
        return Err(crate::TalesError::Other(format!(
            "{} has no Tales effort knob; remove the effort flag for '{}'",
            info.pretty, info.key
        )));
    }
    if info.efforts.contains(&effort) {
        return Ok(());
    }
    Err(crate::TalesError::Other(format!(
        "invalid effort '{effort}' for {}; valid: {}",
        info.pretty,
        info.efforts.join(" | ")
    )))
}

/// Construct the adapter for a known tool key. Dispatches on the row's
/// [`AdapterKind`]: bespoke adapters for Claude/Codex/Open Code, and the
/// stateless [`generic`] adapter — configured from the row — for everything
/// else. The single home for this mapping; frontends and the CLI all call it.
pub fn make_adapter(name: &str) -> Result<Box<dyn AgentAdapter>> {
    let Some(info) = tool_info(name) else {
        let known = KNOWN_TOOLS
            .iter()
            .map(|t| t.key)
            .collect::<Vec<_>>()
            .join(" | ");
        return Err(crate::TalesError::Other(format!(
            "unknown agent '{name}' (known: {known})"
        )));
    };
    Ok(match info.kind {
        AdapterKind::Claude => Box::new(claude::ClaudeAdapter::new()),
        AdapterKind::Codex => Box::new(codex::CodexAdapter::new()),
        AdapterKind::OpenCode => Box::new(opencode::OpenCodeAdapter::new()),
        AdapterKind::Generic => Box::new(generic::GenericAdapter::from_info(info)),
    })
}

/// The uniform interface over every AI tool.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Static capabilities of this adapter.
    fn caps(&self) -> AgentCaps;

    /// Launch the underlying CLI. Spawns the supervision tasks that pump
    /// [`AgentEvent`]s into `events_tx`, and returns the command sender the
    /// orchestrator drives the agent with.
    async fn spawn(
        &mut self,
        ctx: SpawnCtx,
        events_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<mpsc::Sender<AgentCommand>>;
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn validate_roster_allows_distinct_and_duplicate_session_tools() {
        assert!(validate_roster(&["claude".into(), "codex".into()]).is_ok());
        // Claude/Codex isolate co-located agents, so duplicates are fine.
        assert!(validate_roster(&["claude".into(), "claude".into()]).is_ok());
        assert!(validate_roster(&["codex".into(), "opencode".into()]).is_ok());
    }

    #[test]
    fn validate_roster_rejects_two_opencode() {
        let err = validate_roster(&["opencode".into(), "OpenCode".into()]).unwrap_err();
        assert!(err.to_string().contains("Open Code"), "{err}");
    }

    #[test]
    fn make_adapter_knows_opencode() {
        assert!(make_adapter("opencode").is_ok());
        assert!(make_adapter("nope").is_err());
    }

    #[test]
    fn make_adapter_builds_generic_tools() {
        // Generic-kind rows (e.g. gemini) construct via the generic adapter.
        assert!(make_adapter("gemini").is_ok());
        assert!(make_adapter("glm").is_ok());
    }

    #[test]
    fn validate_tool_readiness_reports_unknown_tools_before_spawn() {
        let err = validate_tool_readiness(&["claude".into(), "nope".into()]).unwrap_err();
        assert!(err.to_string().contains("unknown agent 'nope'"), "{err}");
        assert!(err.to_string().contains("tool preflight failed"), "{err}");
    }

    #[test]
    fn validate_tool_readiness_keeps_roster_safety() {
        let err = validate_tool_readiness(&["opencode".into(), "OpenCode".into()]).unwrap_err();
        assert!(err.to_string().contains("cross-contaminate"), "{err}");
    }

    #[test]
    fn validate_effort_accepts_known_tool_efforts() {
        assert!(validate_effort("codex", Some("high")).is_ok());
        assert!(validate_effort("claude", Some("max")).is_ok());
        assert!(validate_effort("codex", None).is_ok());
    }

    #[test]
    fn tool_status_json_exposes_shared_and_legacy_cap_aliases() {
        let claude = tool_info("claude").unwrap();
        let status = tool_info_status_json(claude);

        assert_eq!(status["key"], "claude");
        assert_eq!(status["kind"], "claude-stream");
        assert_eq!(status["known"], true);
        assert_eq!(status["models"][0], "opus");
        assert_eq!(status["suggested_models"][0], "opus");
        assert_eq!(status["caps"]["resumable"], true);
        assert_eq!(status["caps"]["resume"], true);
        assert_eq!(status["caps"]["midturn_injection"], true);
        assert_eq!(status["caps"]["midturn"], true);
    }

    #[test]
    fn known_tools_status_json_lists_registry_once() {
        let status = known_tools_status_json();
        let tools = status["tools"].as_array().unwrap();

        assert_eq!(status["kind"], "tales_tools");
        assert_eq!(status["schema_version"], 1);
        assert_eq!(tools.len(), KNOWN_TOOLS.len());
        assert!(tools.iter().any(|tool| tool["key"] == "codex"));
    }

    #[test]
    fn validate_effort_rejects_invalid_or_unsupported_efforts() {
        let err = validate_effort("codex", Some("max")).unwrap_err();
        assert!(
            err.to_string().contains("valid: low | medium | high"),
            "{err}"
        );

        let err = validate_effort("opencode", Some("high")).unwrap_err();
        assert!(err.to_string().contains("no Tales effort knob"), "{err}");
    }

    #[test]
    fn every_known_tool_constructs() {
        for t in KNOWN_TOOLS {
            assert!(make_adapter(t.key).is_ok(), "{} failed to build", t.key);
        }
    }

    #[test]
    fn generic_rows_have_an_adapter_kind_and_bin() {
        for t in KNOWN_TOOLS {
            assert!(!t.bin.is_empty(), "{} has no bin", t.key);
            if t.kind == AdapterKind::Generic {
                // Generic rows must encode an invocation: a prompt is either the
                // trailing positional (empty prompt_flag) or passed via a flag.
                assert!(
                    t.efforts.is_empty(),
                    "{}: generic + efforts unsupported",
                    t.key
                );
            }
        }
    }

    #[test]
    fn bin_path_accepts_direct_executable_path() {
        let dir = std::env::temp_dir().join(format!("tales-bin-path-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("tool");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert_eq!(
            bin_path(bin.to_str().unwrap()).as_deref(),
            Some(bin.as_path())
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
