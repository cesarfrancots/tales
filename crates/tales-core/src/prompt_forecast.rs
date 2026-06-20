//! Deterministic prompt-call and local-payload forecasts for preflight UIs.
//!
//! This deliberately estimates only what Tales controls before model calls:
//! prompt count shape, task text repeats, cached project context injection, and
//! local-change handoff payloads. Runtime reports remain authoritative for
//! actual prompt telemetry.

use serde_json::{json, Value};

const BUDGETED_PROJECT_CONTEXT_SECTIONS: usize = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreflightContextSummary {
    pub project_context_chars: Option<usize>,
    pub project_context_cache_hit: Option<bool>,
    pub local_change_handoff_chars: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptForecast {
    pub planner_count: usize,
    pub planning_prompts: usize,
    pub recommendation_prompts: usize,
    pub execution_prompts: usize,
    pub max_vote_repair_prompts: usize,
    pub first_context_prompts: usize,
    pub project_context_payload_chars: usize,
    pub local_change_payload_chars: usize,
    pub known_payload_chars: usize,
}

impl PromptForecast {
    pub fn total_prompts(&self) -> usize {
        self.planning_prompts + self.recommendation_prompts + self.execution_prompts
    }

    pub fn worst_case_total_prompts(&self) -> usize {
        self.total_prompts() + self.max_vote_repair_prompts
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdviceLevel {
    Good,
    Info,
    Warn,
}

impl AdviceLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Info => "info",
            Self::Warn => "warn",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunAdvice {
    pub key: &'static str,
    pub level: AdviceLevel,
    pub title: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug)]
pub struct RunAdviceInput<'a> {
    pub forecast: &'a PromptForecast,
    pub context: &'a PreflightContextSummary,
    /// Effective budget after CLI/web defaults are applied. `None` means
    /// unlimited context.
    pub effective_context_budget_chars: Option<usize>,
    pub turns: usize,
    pub sequential: bool,
    pub include_execution: bool,
    pub separate_executor: bool,
}

pub fn build_run_advice(input: RunAdviceInput<'_>) -> Vec<RunAdvice> {
    let mut advice = Vec::new();
    let forecast = input.forecast;

    if input.sequential {
        advice.push(RunAdvice {
            key: "planning_shape",
            level: AdviceLevel::Warn,
            title: "Sequential planning".into(),
            detail: format!(
                "{} planner waits; omit --sequential when latency matters.",
                input.turns
            ),
        });
    } else if forecast.planner_count > 1 {
        let rounds = forecast.planning_prompts.div_ceil(forecast.planner_count);
        advice.push(RunAdvice {
            key: "planning_shape",
            level: AdviceLevel::Good,
            title: "Parallel planning".into(),
            detail: format!(
                "{rounds} waits across {} planners instead of {} sequential turns.",
                forecast.planner_count, input.turns
            ),
        });
    }

    match input.context.project_context_chars {
        Some(chars) => {
            match input.context.project_context_cache_hit {
                Some(true) => advice.push(RunAdvice {
                    key: "project_context_cache",
                    level: AdviceLevel::Good,
                    title: "Context cache hit".into(),
                    detail: "Stable repo map reused; agents skip rediscovery work.".into(),
                }),
                Some(false) => advice.push(RunAdvice {
                    key: "project_context_cache",
                    level: AdviceLevel::Info,
                    title: "Context cache rebuilt".into(),
                    detail:
                        "This run pays the indexing cost; the next matching run should hit cache."
                            .into(),
                }),
                None => {}
            }

            match input.effective_context_budget_chars {
                Some(budget) if chars > budget => advice.push(RunAdvice {
                    key: "context_cap",
                    level: AdviceLevel::Good,
                    title: "Context capped".into(),
                    detail: format!(
                        "{chars} source chars are budgeted before first planner prompts."
                    ),
                }),
                Some(budget) if chars.saturating_mul(2) < budget => advice.push(RunAdvice {
                    key: "context_cap",
                    level: AdviceLevel::Info,
                    title: "Context comfortably fits".into(),
                    detail: format!(
                        "{chars}/{budget} chars leaves room for task-specific context."
                    ),
                }),
                None => advice.push(RunAdvice {
                    key: "context_cap",
                    level: AdviceLevel::Warn,
                    title: "Unlimited context".into(),
                    detail: "Prompt payload can grow without a deterministic cap.".into(),
                }),
                _ => {}
            }
        }
        None => advice.push(RunAdvice {
            key: "project_context_cache",
            level: AdviceLevel::Warn,
            title: "Project context off".into(),
            detail: "Agents may spend extra tokens rediscovering repo shape.".into(),
        }),
    }

    if input.include_execution {
        if input.separate_executor {
            advice.push(RunAdvice {
                key: "tiered_execution",
                level: AdviceLevel::Good,
                title: "Tiered execution".into(),
                detail:
                    "Planner models can spend judgment while a separate executor does the typing."
                        .into(),
            });
        } else {
            advice.push(RunAdvice {
                key: "tiered_execution",
                level: AdviceLevel::Info,
                title: "Participant executor".into(),
                detail: "Execution reuses a planning participant; simpler, but less cost-tiered."
                    .into(),
            });
        }

        if forecast.max_vote_repair_prompts > 0 {
            advice.push(RunAdvice {
                key: "vote_repair",
                level: AdviceLevel::Warn,
                title: "Vote repair risk".into(),
                detail: format!(
                    "Base call plan is {}; worst case is {} if all recommendation votes need repair. Common vote aliases and unambiguous candidate mentions are accepted without repair.",
                    forecast.total_prompts(),
                    forecast.worst_case_total_prompts()
                ),
            });
        }

        if let Some(chars) = input.context.local_change_handoff_chars {
            if chars > 8_000 {
                advice.push(RunAdvice {
                    key: "local_change_handoff",
                    level: AdviceLevel::Warn,
                    title: "Large dirty-worktree handoff".into(),
                    detail: format!(
                        "{chars} chars of local changes will be preserved in execution context."
                    ),
                });
            } else if chars > 0 {
                advice.push(RunAdvice {
                    key: "local_change_handoff",
                    level: AdviceLevel::Info,
                    title: "Dirty worktree protected".into(),
                    detail: "Executor prompt includes a compact local-change handoff.".into(),
                });
            }
        }
    }

    advice
}

pub fn estimated_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

pub fn prompt_forecast_to_json(forecast: &PromptForecast, turns: usize, sequential: bool) -> Value {
    let project_context_chars_per_prompt = forecast
        .project_context_payload_chars
        .checked_div(forecast.first_context_prompts)
        .unwrap_or(0);
    let waits = if sequential {
        turns
    } else {
        parallel_rounds(turns, forecast.planner_count)
    };
    json!({
        "planner_count": forecast.planner_count,
        "planned_calls": {
            "total": forecast.total_prompts(),
            "base": forecast.total_prompts(),
            "worst_case": forecast.worst_case_total_prompts(),
            "planning": forecast.planning_prompts,
            "recommendation": forecast.recommendation_prompts,
            "execution": forecast.execution_prompts,
        },
        "planning_waits": {
            "mode": if sequential { "sequential" } else { "parallel" },
            "waits": waits,
        },
        "repair_risk": {
            "min_vote_repair_prompts": 0,
            "max_vote_repair_prompts": forecast.max_vote_repair_prompts,
            "base_total_prompts": forecast.total_prompts(),
            "worst_case_total_prompts": forecast.worst_case_total_prompts(),
            "parser_shortcuts": [
                "recommended_executor",
                "executor",
                "agent",
                "tool",
                "rationale",
                "why",
                "reason",
                "single_candidate_mention",
            ],
        },
        "payloads": {
            "known_chars": forecast.known_payload_chars,
            "known_tokens_estimate": estimated_tokens(forecast.known_payload_chars),
            "project_context_chars_per_first_prompt": project_context_chars_per_prompt,
            "first_context_prompts": forecast.first_context_prompts,
            "project_context_total_chars": forecast.project_context_payload_chars,
            "project_context_total_tokens_estimate": estimated_tokens(forecast.project_context_payload_chars),
            "local_change_handoff_chars": forecast.local_change_payload_chars,
            "local_change_handoff_tokens_estimate": estimated_tokens(forecast.local_change_payload_chars),
        },
    })
}

pub fn run_advice_to_json(items: &[RunAdvice]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            json!({
                "key": item.key,
                "level": item.level.as_str(),
                "title": item.title.as_str(),
                "detail": item.detail.as_str(),
            })
        })
        .collect()
}

pub fn parallel_rounds(max_turns: usize, planners: usize) -> usize {
    let planners = planners.max(1);
    (max_turns / planners).clamp(2, 6)
}

pub fn forecast_prompt_usage(
    task_chars: usize,
    planner_count: usize,
    turns: usize,
    sequential: bool,
    include_execution: bool,
    context: &PreflightContextSummary,
    context_budget_chars: Option<usize>,
) -> PromptForecast {
    let planner_count = planner_count.max(1);
    let planning_prompts = if sequential {
        turns
    } else {
        parallel_rounds(turns, planner_count) * planner_count
    };
    let recommendation_prompts = if include_execution { planner_count } else { 0 };
    let execution_prompts = usize::from(include_execution);
    let max_vote_repair_prompts = if include_execution { planner_count } else { 0 };
    let first_context_prompts = match context.project_context_chars {
        Some(_) if sequential => turns.min(planner_count),
        Some(_) => planner_count,
        None => 0,
    };
    let project_context_payload_chars = context
        .project_context_chars
        .map(|chars| budgeted_context_payload_chars(chars, context_budget_chars))
        .unwrap_or(0)
        * first_context_prompts;
    let local_change_payload_chars = if include_execution {
        context.local_change_handoff_chars.unwrap_or(0)
    } else {
        0
    };
    let total_prompts = planning_prompts + recommendation_prompts + execution_prompts;
    let known_payload_chars =
        task_chars * total_prompts + project_context_payload_chars + local_change_payload_chars;

    PromptForecast {
        planner_count,
        planning_prompts,
        recommendation_prompts,
        execution_prompts,
        max_vote_repair_prompts,
        first_context_prompts,
        project_context_payload_chars,
        local_change_payload_chars,
        known_payload_chars,
    }
}

fn budgeted_context_payload_chars(chars: usize, context_budget_chars: Option<usize>) -> usize {
    match context_budget_chars {
        Some(max_chars) if max_chars > 0 && chars > max_chars => {
            max_chars + project_context_cap_overhead_chars(max_chars)
        }
        _ => chars,
    }
}

fn project_context_cap_overhead_chars(max_chars: usize) -> usize {
    let section_budget = (max_chars / BUDGETED_PROJECT_CONTEXT_SECTIONS).max(1);
    format!("(project context capped; preserving {BUDGETED_PROJECT_CONTEXT_SECTIONS} sections)\n")
        .chars()
        .count()
        + BUDGETED_PROJECT_CONTEXT_SECTIONS * edge_marker_chars(section_budget)
}

fn edge_marker_chars(max_chars: usize) -> usize {
    let head_chars = (max_chars / 2).max(1);
    let tail_chars = max_chars.saturating_sub(head_chars).max(1);
    format!(
        "(middle context omitted; showing first {head_chars} and last {tail_chars} chars)\n...\n"
    )
    .chars()
    .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_forecast_counts_parallel_run_payload() {
        let context = PreflightContextSummary {
            project_context_chars: Some(1_000),
            project_context_cache_hit: Some(true),
            local_change_handoff_chars: Some(200),
        };

        let forecast = forecast_prompt_usage(4, 2, 4, false, true, &context, None);

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
        let context = PreflightContextSummary {
            project_context_chars: None,
            project_context_cache_hit: None,
            local_change_handoff_chars: Some(200),
        };

        let forecast = forecast_prompt_usage(4, 2, 3, true, false, &context, None);

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
    fn prompt_forecast_caps_first_project_context_payload() {
        let context = PreflightContextSummary {
            project_context_chars: Some(1_000),
            project_context_cache_hit: Some(false),
            local_change_handoff_chars: None,
        };

        let forecast = forecast_prompt_usage(4, 2, 4, false, true, &context, Some(100));
        let capped_payload = 100 + project_context_cap_overhead_chars(100);

        assert_eq!(forecast.first_context_prompts, 2);
        assert_eq!(forecast.project_context_payload_chars, capped_payload * 2);
        assert_eq!(
            forecast.known_payload_chars,
            4 * forecast.total_prompts() + capped_payload * 2
        );
    }

    #[test]
    fn run_advice_flags_parallel_cache_cap_and_tiered_execution() {
        let context = PreflightContextSummary {
            project_context_chars: Some(50_000),
            project_context_cache_hit: Some(true),
            local_change_handoff_chars: Some(400),
        };
        let forecast = forecast_prompt_usage(20, 2, 4, false, true, &context, Some(24_000));

        let advice = build_run_advice(RunAdviceInput {
            forecast: &forecast,
            context: &context,
            effective_context_budget_chars: Some(24_000),
            turns: 4,
            sequential: false,
            include_execution: true,
            separate_executor: true,
        });

        let keys = advice
            .iter()
            .map(|item| (item.key, item.level))
            .collect::<Vec<_>>();
        assert!(keys.contains(&("planning_shape", AdviceLevel::Good)));
        assert!(keys.contains(&("project_context_cache", AdviceLevel::Good)));
        assert!(keys.contains(&("context_cap", AdviceLevel::Good)));
        assert!(keys.contains(&("tiered_execution", AdviceLevel::Good)));
        assert!(keys.contains(&("vote_repair", AdviceLevel::Warn)));
        assert!(keys.contains(&("local_change_handoff", AdviceLevel::Info)));
    }

    #[test]
    fn run_advice_warns_for_sequential_unlimited_and_disabled_context() {
        let context = PreflightContextSummary {
            project_context_chars: None,
            project_context_cache_hit: None,
            local_change_handoff_chars: None,
        };
        let forecast = forecast_prompt_usage(20, 2, 4, true, false, &context, None);

        let advice = build_run_advice(RunAdviceInput {
            forecast: &forecast,
            context: &context,
            effective_context_budget_chars: None,
            turns: 4,
            sequential: true,
            include_execution: false,
            separate_executor: false,
        });

        assert!(advice
            .iter()
            .any(|item| item.key == "planning_shape" && item.level == AdviceLevel::Warn));
        assert!(advice
            .iter()
            .any(|item| item.key == "project_context_cache" && item.level == AdviceLevel::Warn));
    }

    #[test]
    fn prompt_forecast_json_reports_shared_preflight_shape() {
        let forecast = PromptForecast {
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
        assert_eq!(value["planning_waits"]["waits"], 2);
        assert_eq!(
            value["payloads"]["project_context_chars_per_first_prompt"],
            600
        );
        assert_eq!(value["repair_risk"]["min_vote_repair_prompts"], 0);
        assert_eq!(value["repair_risk"]["max_vote_repair_prompts"], 2);
        assert_eq!(value["repair_risk"]["worst_case_total_prompts"], 9);
        assert!(value["repair_risk"]["parser_shortcuts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|shortcut| shortcut == "single_candidate_mention"));
    }

    #[test]
    fn run_advice_json_keeps_machine_readable_levels() {
        let advice = vec![RunAdvice {
            key: "tiered_execution",
            level: AdviceLevel::Good,
            title: "Tiered execution".into(),
            detail: "Separate executor keeps planning and typing costs apart.".into(),
        }];

        let value = run_advice_to_json(&advice);

        assert_eq!(value.len(), 1);
        assert_eq!(value[0]["key"], "tiered_execution");
        assert_eq!(value[0]["level"], "good");
        assert_eq!(value[0]["title"], "Tiered execution");
    }
}
