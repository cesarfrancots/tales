//! Shared session contracts used by CLI, TUI, web, and dry-run preflight.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{tool_info, validate_effort, validate_roster};
use crate::{Result, TalesError};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Discuss,
    Run,
    Solo,
    Eval,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CwdPolicy {
    Shared,
    SeparateWorktree,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPermission {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    AcceptEdits,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSeatConfig {
    pub role: String,
    pub tool_key: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub cwd_policy: CwdPolicy,
    pub execution_permission: ExecutionPermission,
}

impl AgentSeatConfig {
    pub fn new(role: impl Into<String>, tool_key: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            tool_key: tool_key.into(),
            model: None,
            effort: None,
            cwd_policy: CwdPolicy::Shared,
            execution_permission: ExecutionPermission::WorkspaceWrite,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptBudgetConfig {
    pub context_budget_chars: Option<usize>,
    pub project_context_enabled: bool,
    pub refresh_project_context: bool,
    pub project_context_max_files: usize,
    pub project_context_max_manifest_chars: usize,
    pub local_change_handoff_lines: usize,
}

impl Default for PromptBudgetConfig {
    fn default() -> Self {
        Self {
            context_budget_chars: Some(crate::orchestrator::DEFAULT_CONTEXT_BUDGET_CHARS),
            project_context_enabled: true,
            refresh_project_context: false,
            project_context_max_files: 220,
            project_context_max_manifest_chars: 1_200,
            local_change_handoff_lines: crate::project_context::DEFAULT_LOCAL_CHANGE_LINES,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub recommend_only: bool,
    pub approval_required: bool,
    pub allowed_executors: Vec<String>,
}

impl ExecutionPolicy {
    pub fn recommend_only(allowed_executors: Vec<String>) -> Self {
        Self {
            recommend_only: true,
            approval_required: true,
            allowed_executors,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportPaths {
    pub markdown: Option<PathBuf>,
    pub json: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionConfig {
    pub schema_version: u8,
    pub task: String,
    pub cwd: PathBuf,
    pub mode: SessionMode,
    pub turns: usize,
    pub sequential: bool,
    pub sandbox: String,
    pub seats: Vec<AgentSeatConfig>,
    pub prompt_budget: PromptBudgetConfig,
    pub execution_policy: ExecutionPolicy,
    pub report_paths: ReportPaths,
}

impl SessionConfig {
    pub fn validate(&self) -> Result<()> {
        if self.task.trim().is_empty() {
            return Err(TalesError::Other("session task cannot be empty".into()));
        }
        if self.turns == 0 {
            return Err(TalesError::Other(
                "session turns must be greater than zero".into(),
            ));
        }
        if !self.cwd.is_dir() {
            return Err(TalesError::Other(format!(
                "session cwd is not a directory: {}",
                self.cwd.display()
            )));
        }
        if self.seats.is_empty() {
            return Err(TalesError::Other(
                "session needs at least one agent seat".into(),
            ));
        }

        let tools: Vec<String> = self
            .seats
            .iter()
            .map(|seat| seat.tool_key.clone())
            .collect();
        validate_roster(&tools)?;
        for seat in &self.seats {
            if tool_info(&seat.tool_key).is_none() {
                return Err(TalesError::Other(format!(
                    "unknown tool: {}",
                    seat.tool_key
                )));
            }
            validate_effort(&seat.tool_key, seat.effort.as_deref())?;
        }
        for executor in &self.execution_policy.allowed_executors {
            if !tools.iter().any(|tool| tool.eq_ignore_ascii_case(executor))
                && tool_info(executor).is_none()
            {
                return Err(TalesError::Other(format!(
                    "unknown allowed executor: {executor}"
                )));
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        json!(self)
    }

    pub fn planner_tools(&self) -> Vec<String> {
        self.seats
            .iter()
            .filter(|seat| {
                matches!(
                    seat.role.as_str(),
                    "drafter" | "critic" | "planner" | "reviewer"
                )
            })
            .map(|seat| seat.tool_key.clone())
            .collect()
    }
}

pub fn canonical_cwd(raw: Option<&str>) -> Result<PathBuf> {
    let path = raw
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir().map_err(|e| TalesError::Other(e.to_string()))?);
    canonicalize_dir(&path)
}

pub fn canonicalize_dir(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .map_err(|e| TalesError::Other(format!("cannot open workspace: {e}")))?;
    if !path.is_dir() {
        return Err(TalesError::Other(format!(
            "workspace must be a directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoleArtifact {
    pub role: String,
    pub summary: String,
    pub risks: Vec<String>,
    pub files_or_areas: Vec<String>,
    pub recommended_next_action: String,
    pub confidence: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptCompact {
    pub task: String,
    pub decisions: Vec<String>,
    pub risks: Vec<String>,
    pub open_questions: Vec<String>,
    pub proposed_executor: Option<String>,
}

pub fn compact_transcript(
    task: &str,
    transcript: &str,
    candidates: &[String],
) -> TranscriptCompact {
    let mut out = TranscriptCompact {
        task: task.trim().to_string(),
        ..TranscriptCompact::default()
    };
    for line in transcript
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let lower = line.to_lowercase();
        if out.decisions.len() < 6
            && (lower.contains("decide")
                || lower.contains("recommend")
                || lower.contains("plan:")
                || lower.contains("approved"))
        {
            out.decisions.push(line.to_string());
        }
        if out.risks.len() < 6
            && (lower.contains("risk")
                || lower.contains("edge")
                || lower.contains("fail")
                || lower.contains("warning"))
        {
            out.risks.push(line.to_string());
        }
        if out.open_questions.len() < 4 && line.ends_with('?') {
            out.open_questions.push(line.to_string());
        }
        if out.proposed_executor.is_none() {
            for candidate in candidates {
                if lower.contains(&candidate.to_lowercase()) {
                    out.proposed_executor = Some(candidate.clone());
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_config_validates_roster_and_effort() {
        let cfg = SessionConfig {
            schema_version: 1,
            task: "fix lint".into(),
            cwd: std::env::current_dir().unwrap(),
            mode: SessionMode::Run,
            turns: 4,
            sequential: false,
            sandbox: "workspace-write".into(),
            seats: vec![
                AgentSeatConfig {
                    role: "drafter".into(),
                    tool_key: "claude".into(),
                    model: Some("opus".into()),
                    effort: Some("max".into()),
                    cwd_policy: CwdPolicy::Shared,
                    execution_permission: ExecutionPermission::WorkspaceWrite,
                },
                AgentSeatConfig {
                    role: "critic".into(),
                    tool_key: "codex".into(),
                    model: None,
                    effort: Some("high".into()),
                    cwd_policy: CwdPolicy::Shared,
                    execution_permission: ExecutionPermission::WorkspaceWrite,
                },
            ],
            prompt_budget: PromptBudgetConfig::default(),
            execution_policy: ExecutionPolicy::recommend_only(vec![
                "claude".into(),
                "codex".into(),
            ]),
            report_paths: ReportPaths {
                markdown: None,
                json: None,
            },
        };

        cfg.validate().unwrap();
        assert_eq!(cfg.to_json()["execution_policy"]["recommend_only"], true);
    }

    #[test]
    fn compact_transcript_keeps_decisions_risks_and_executor() {
        let compact = compact_transcript(
            "ship it",
            "Plan: use small patch\nRisk: tests may fail\nShould we use codex?\nRecommend codex",
            &["claude".into(), "codex".into()],
        );

        assert_eq!(compact.proposed_executor.as_deref(), Some("codex"));
        assert_eq!(compact.decisions.len(), 2);
        assert_eq!(compact.risks.len(), 1);
        assert_eq!(compact.open_questions.len(), 1);
    }
}
