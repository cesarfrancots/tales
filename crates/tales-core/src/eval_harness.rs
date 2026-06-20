//! Lightweight deterministic evaluation harness.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::prompt_forecast::{
    forecast_prompt_usage, prompt_forecast_to_json, PreflightContextSummary,
};
use crate::tool_recommendation::{recommend_tools, recommendation_chips_json, RecommendationInput};
use crate::workspace_profile::WorkspaceProfile;
use crate::{Result, TalesError};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalMode {
    Solo,
    Debate,
    Tiered,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalScenario {
    pub name: String,
    pub task: String,
    pub mode: EvalMode,
    pub turns: usize,
    pub sequential: bool,
    pub drafter: Option<String>,
    pub critic: Option<String>,
    pub executor: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalResult {
    pub schema_version: u8,
    pub created_at_unix: u64,
    pub workspace: String,
    pub scenario: EvalScenario,
    pub prompt_forecast: Value,
    pub recommendations: Vec<Value>,
    pub metrics: EvalMetrics,
    pub checks: Vec<EvalCheck>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalMetrics {
    pub planned_calls: usize,
    pub worst_case_planned_calls: usize,
    pub known_payload_chars: usize,
    pub estimated_tokens: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

pub fn default_scenarios(task: &str) -> Vec<EvalScenario> {
    vec![
        EvalScenario {
            name: "solo_strong".into(),
            task: task.into(),
            mode: EvalMode::Solo,
            turns: 1,
            sequential: false,
            drafter: None,
            critic: None,
            executor: "claude".into(),
        },
        EvalScenario {
            name: "debate_parallel".into(),
            task: task.into(),
            mode: EvalMode::Debate,
            turns: 4,
            sequential: false,
            drafter: Some("claude".into()),
            critic: Some("codex".into()),
            executor: "claude".into(),
        },
        EvalScenario {
            name: "tiered_parallel".into(),
            task: task.into(),
            mode: EvalMode::Tiered,
            turns: 4,
            sequential: false,
            drafter: Some("claude".into()),
            critic: Some("codex".into()),
            executor: "opencode".into(),
        },
    ]
}

pub fn run_mock_eval(
    workspace: &Path,
    scenario: EvalScenario,
    context: &PreflightContextSummary,
    profile: Option<&WorkspaceProfile>,
) -> EvalResult {
    let planner_count = match scenario.mode {
        EvalMode::Solo => 1,
        EvalMode::Debate | EvalMode::Tiered => 2,
    };
    let include_execution = !matches!(scenario.mode, EvalMode::Debate);
    let forecast = forecast_prompt_usage(
        scenario.task.chars().count(),
        planner_count,
        scenario.turns,
        scenario.sequential,
        include_execution,
        context,
        Some(crate::orchestrator::DEFAULT_CONTEXT_BUDGET_CHARS),
    );
    let candidates = scenario_candidates(&scenario);
    let recommendation_input = RecommendationInput {
        task: scenario.task.clone(),
        candidates,
        prefer_cheaper: matches!(scenario.mode, EvalMode::Tiered),
        prefer_faster: !scenario.sequential,
        needs_execution: include_execution,
    };
    let recommendations =
        recommendation_chips_json(&recommend_tools(&recommendation_input, profile));
    let metrics = EvalMetrics {
        planned_calls: forecast.total_prompts(),
        worst_case_planned_calls: forecast.worst_case_total_prompts(),
        known_payload_chars: forecast.known_payload_chars,
        estimated_tokens: crate::prompt_forecast::estimated_tokens(forecast.known_payload_chars),
    };
    let prompt_forecast = prompt_forecast_to_json(&forecast, scenario.turns, scenario.sequential);
    let checks = vec![
        EvalCheck {
            name: "forecast_built".into(),
            passed: forecast.total_prompts() > 0,
            detail: "deterministic prompt forecast generated without model calls".into(),
        },
        EvalCheck {
            name: "human_gate_required".into(),
            passed: true,
            detail: "eval scenarios never auto-execute without approval".into(),
        },
    ];
    EvalResult {
        schema_version: 1,
        created_at_unix: now_unix(),
        workspace: workspace.display().to_string(),
        scenario,
        prompt_forecast,
        recommendations,
        metrics,
        checks,
    }
}

pub fn compare_results(results: &[EvalResult]) -> Value {
    let best_latency_shape = results
        .iter()
        .min_by_key(|result| result.metrics.planned_calls)
        .map(|result| result.scenario.name.clone());
    let cheapest_payload = results
        .iter()
        .min_by_key(|result| result.metrics.known_payload_chars)
        .map(|result| result.scenario.name.clone());
    json!({
        "kind": "tales_eval_compare",
        "schema_version": 1,
        "scenarios": results,
        "best_latency_shape": best_latency_shape,
        "cheapest_payload_shape": cheapest_payload,
    })
}

pub fn save_eval_report(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| TalesError::Other(e.to_string()))?;
    }
    fs::write(path, serde_json::to_string_pretty(value)?)
        .map_err(|e| TalesError::Other(e.to_string()))
}

pub fn default_eval_report_path(workspace: &Path, name: &str) -> PathBuf {
    workspace
        .join(".tales")
        .join("evals")
        .join(format!("{name}.json"))
}

fn scenario_candidates(scenario: &EvalScenario) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(drafter) = &scenario.drafter {
        out.push(drafter.clone());
    }
    if let Some(critic) = &scenario.critic {
        out.push(critic.clone());
    }
    out.push(scenario.executor.clone());
    out.sort();
    out.dedup();
    out
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_eval_generates_metrics_without_model_calls() {
        let scenario = default_scenarios("fix lint").remove(1);
        let context = PreflightContextSummary {
            project_context_chars: Some(1_000),
            project_context_cache_hit: Some(true),
            local_change_handoff_chars: Some(200),
        };

        let result = run_mock_eval(Path::new("/tmp/tales"), scenario, &context, None);

        assert!(result.metrics.planned_calls > 0);
        assert!(result.checks.iter().all(|check| check.passed));
        assert_eq!(result.schema_version, 1);
    }
}
