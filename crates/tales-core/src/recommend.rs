//! Recommendation stage — the agents vote on who should execute the plan.
//!
//! After the discussion, each agent is asked to nominate an executor with a
//! confidence. [`aggregate`] tallies confidence-weighted votes per candidate
//! and produces a [`Recommendation`]. The orchestrator surfaces this to the
//! user, who must confirm (or override) before any execution happens — the
//! recommendation is advisory, the gate is the user's.

use serde::Deserialize;

use crate::AgentId;

/// One agent's nomination.
#[derive(Clone, Debug)]
pub struct ExecutionVote {
    pub voter: AgentId,
    pub voter_label: String,
    pub recommended_executor: String,
    pub confidence: f32,
    pub rationale: String,
}

/// The aggregated recommendation across all votes.
#[derive(Clone, Debug)]
pub struct Recommendation {
    /// The candidate label with the highest confidence-weighted score.
    pub executor: String,
    /// Score per candidate label (sum of confidences), highest first.
    pub scores: Vec<(String, f32)>,
    pub votes: Vec<ExecutionVote>,
    /// Human-readable summary of why.
    pub rationale: String,
    /// `true` if at least one vote scored a candidate. When `false`, `executor`
    /// is a fallback (first candidate) and the user should treat it as "no
    /// clear recommendation" rather than a real consensus.
    pub confident: bool,
}

/// Shape an agent is asked to emit as JSON.
#[derive(Debug, Deserialize)]
struct RawVote {
    recommended_executor: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    rationale: String,
}

fn default_confidence() -> f32 {
    0.5
}

/// Extract a `RawVote` from free-form agent text by isolating the first
/// *balanced* top-level JSON object. Tolerates prose/code fences around it and
/// braces appearing later in the prose (which a naive first-`{`/last-`}` span
/// would wrongly include).
pub fn parse_vote(text: &str) -> Option<(String, f32, String)> {
    let slice = first_json_object(text)?;
    let raw: RawVote = serde_json::from_str(slice).ok()?;
    let conf = raw.confidence.clamp(0.0, 1.0);
    Some((
        raw.recommended_executor.trim().to_string(),
        conf,
        raw.rationale,
    ))
}

/// Return the substring of the first balanced `{...}` object, tracking string
/// literals and escapes so braces inside strings don't throw off the depth.
fn first_json_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Normalize a label for matching (case/space-insensitive).
fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Tally confidence-weighted votes. `candidates` are the valid executor labels;
/// a vote whose nomination doesn't match a candidate is matched by normalized
/// label, and otherwise ignored for scoring (but kept in `votes`).
pub fn aggregate(votes: Vec<ExecutionVote>, candidates: &[String]) -> Option<Recommendation> {
    if candidates.is_empty() {
        return None;
    }

    let mut scores: Vec<(String, f32)> = candidates.iter().map(|c| (c.clone(), 0.0)).collect();
    for vote in &votes {
        let target = norm(&vote.recommended_executor);
        if let Some(slot) = scores.iter_mut().find(|(c, _)| norm(c) == target) {
            slot.1 += vote.confidence;
        }
    }

    // Highest score first; stable so ties keep candidate order (roster order).
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let top = scores
        .first()
        .cloned()
        .unwrap_or((candidates[0].clone(), 0.0));
    let confident = top.1 > 0.0;
    // When no vote scored a candidate, fall back to the first candidate but flag
    // it as not-confident so callers/UI don't present it as a real consensus.
    let executor = top.0;

    let mut rationale = String::new();
    if !confident {
        rationale.push_str(
            "(no parseable/decisive vote — defaulting to the first candidate; please choose)\n",
        );
    }
    for vote in &votes {
        rationale.push_str(&format!(
            "- {} → {} (conf {:.2}): {}\n",
            vote.voter_label,
            vote.recommended_executor,
            vote.confidence,
            vote.rationale.trim()
        ));
    }

    Some(Recommendation {
        executor,
        scores,
        votes,
        rationale,
        confident,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn parse_vote_tolerates_prose_and_fences() {
        let text = "Sure! Here is my vote:\n```json\n{\"recommended_executor\": \"claude\", \
                    \"confidence\": 0.9, \"rationale\": \"best at code\"}\n```\nThanks";
        let (exec, conf, why) = parse_vote(text).unwrap();
        assert_eq!(exec, "claude");
        assert!((conf - 0.9).abs() < 1e-6);
        assert!(why.contains("best at code"));
    }

    #[test]
    fn aggregate_picks_highest_confidence_candidate() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        let votes = vec![
            ExecutionVote {
                voter: Uuid::new_v4(),
                voter_label: "claude".into(),
                recommended_executor: "Claude".into(), // case-insensitive match
                confidence: 0.8,
                rationale: "x".into(),
            },
            ExecutionVote {
                voter: Uuid::new_v4(),
                voter_label: "codex".into(),
                recommended_executor: "claude".into(),
                confidence: 0.6,
                rationale: "y".into(),
            },
        ];
        let rec = aggregate(votes, &candidates).unwrap();
        assert_eq!(rec.executor, "claude");
        assert_eq!(rec.scores[0].0, "claude");
        assert!((rec.scores[0].1 - 1.4).abs() < 1e-6);
    }
}
