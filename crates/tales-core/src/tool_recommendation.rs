//! Deterministic model/tool recommendation hints.
//!
//! This scorer never executes anything. It only ranks known tools so frontends
//! can explain smarter/faster/cheaper choices before the human approval gate.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{bin_on_path, tool_info, ToolInfo};
use crate::workspace_profile::WorkspaceProfile;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecommendationInput {
    pub task: String,
    pub candidates: Vec<String>,
    pub prefer_cheaper: bool,
    pub prefer_faster: bool,
    pub needs_execution: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRecommendation {
    pub tool_key: String,
    pub score: i32,
    pub installed: bool,
    pub reasons: Vec<String>,
    pub chips: Vec<String>,
}

pub fn recommend_tools(
    input: &RecommendationInput,
    profile: Option<&WorkspaceProfile>,
) -> Vec<ToolRecommendation> {
    let mut rows = input
        .candidates
        .iter()
        .filter_map(|candidate| tool_info(candidate).map(|info| score_tool(info, input, profile)))
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.tool_key.cmp(&b.tool_key))
    });
    rows
}

pub fn recommendation_chips_json(items: &[ToolRecommendation]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            json!({
                "tool": item.tool_key,
                "score": item.score,
                "installed": item.installed,
                "reasons": item.reasons,
                "chips": item.chips,
            })
        })
        .collect()
}

fn score_tool(
    info: &ToolInfo,
    input: &RecommendationInput,
    profile: Option<&WorkspaceProfile>,
) -> ToolRecommendation {
    let installed = bin_on_path(info.bin);
    let mut score = 0;
    let mut reasons = Vec::new();
    let mut chips = Vec::new();

    if installed {
        score += 25;
        reasons.push("installed locally".into());
    } else {
        score -= 50;
        reasons.push("not installed".into());
    }

    if info.supports_resume {
        score += 10;
        reasons.push("resumable sessions reduce repeated context".into());
        chips.push("cheaper".into());
    }
    if info.supports_structured_output {
        score += 8;
        reasons.push("structured output reduces vote repair prompts".into());
        chips.push("smarter".into());
    }
    if input.needs_execution && info.supports_headless {
        score += 8;
        reasons.push("headless execution ready".into());
    }
    if input.prefer_faster && info.startup_cost == "low" {
        score += 8;
        chips.push("faster".into());
    }
    if input.prefer_cheaper && matches!(info.default_price_tier, "low" | "medium") {
        score += 8;
        chips.push("cheaper".into());
    }
    if info.default_strength == "frontier" {
        score += 6;
        chips.push("smarter".into());
    }

    if let Some(profile) = profile {
        if profile
            .preferred_tools
            .iter()
            .any(|tool| tool.eq_ignore_ascii_case(info.key))
        {
            score += 6;
            reasons.push("seen in this workspace profile".into());
        }
        if profile
            .latency_cost
            .iter()
            .any(|stats| stats.tool_key == info.key)
        {
            score += 4;
            reasons.push("has prior local telemetry".into());
        }
    }

    chips.sort();
    chips.dedup();

    ToolRecommendation {
        tool_key: info.key.into(),
        score,
        installed,
        reasons,
        chips,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendation_scores_known_tools_and_chips() {
        let input = RecommendationInput {
            task: "fix lint".into(),
            candidates: vec!["claude".into(), "codex".into()],
            prefer_cheaper: true,
            prefer_faster: true,
            needs_execution: true,
        };

        let rows = recommend_tools(&input, None);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.tool_key == "codex"));
        assert!(rows
            .iter()
            .flat_map(|row| row.chips.iter())
            .any(|chip| chip == "smarter" || chip == "cheaper"));
    }
}
