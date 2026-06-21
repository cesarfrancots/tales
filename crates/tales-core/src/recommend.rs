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
    pub needs_plan: bool,
    pub rationale: String,
    pub parse_source: VoteParseSource,
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
    /// True only when every parsed planner vote says a formal plan is needed.
    pub needs_plan: bool,
    /// `true` only when a candidate has a positive, unique top score. When
    /// `false`, `executor` is still the label Tales would preselect, but the
    /// user should treat it as "please choose" rather than real consensus.
    pub confident: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoteParseSource {
    Json,
    CandidateMention,
}

impl VoteParseSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::CandidateMention => "candidate_mention",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedVote {
    pub recommended_executor: String,
    pub confidence: f32,
    pub needs_plan: bool,
    pub rationale: String,
    pub source: VoteParseSource,
}

/// Shape an agent is asked to emit as JSON.
#[derive(Debug, Deserialize)]
struct RawVote {
    #[serde(alias = "executor", alias = "agent", alias = "tool")]
    recommended_executor: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    needs_plan: bool,
    #[serde(default, alias = "why", alias = "reason")]
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
    parse_vote_json(text).map(|vote| (vote.recommended_executor, vote.confidence, vote.rationale))
}

pub fn parse_vote_lenient(text: &str, candidates: &[String]) -> Option<ParsedVote> {
    if let Some(vote) = parse_vote_json(text) {
        return Some(vote);
    }
    parse_single_candidate_mention(text, candidates)
}

fn parse_vote_json(text: &str) -> Option<ParsedVote> {
    let slice = first_json_object(text)?;
    let raw: RawVote = serde_json::from_str(slice).ok()?;
    let conf = raw.confidence.clamp(0.0, 1.0);
    Some(ParsedVote {
        recommended_executor: raw.recommended_executor.trim().to_string(),
        confidence: conf,
        needs_plan: raw.needs_plan,
        rationale: raw.rationale,
        source: VoteParseSource::Json,
    })
}

fn parse_single_candidate_mention(text: &str, candidates: &[String]) -> Option<ParsedVote> {
    let mentioned: Vec<&String> = candidates
        .iter()
        .filter(|candidate| mentions_candidate(text, candidate))
        .collect();
    if mentioned.len() != 1 {
        return None;
    }
    Some(ParsedVote {
        recommended_executor: mentioned[0].clone(),
        confidence: 0.5,
        needs_plan: false,
        rationale: format!("salvaged from prose vote: {}", compact_one_line(text, 180)),
        source: VoteParseSource::CandidateMention,
    })
}

fn mentions_candidate(text: &str, candidate: &str) -> bool {
    let needle = candidate.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return false;
    }
    let haystack = text.to_ascii_lowercase();
    let mut offset = 0usize;
    while let Some(found) = haystack[offset..].find(&needle) {
        let start = offset + found;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .map(label_boundary)
            .unwrap_or(true);
        let after_ok = haystack[end..]
            .chars()
            .next()
            .map(label_boundary)
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        offset = end;
    }
    false
}

fn label_boundary(ch: char) -> bool {
    !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-'
}

fn compact_one_line(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (idx, ch) in normalized.chars().enumerate() {
        if idx == max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
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

const SCORE_TIE_EPSILON: f32 = 1e-6;

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
    let needs_plan = votes.len() >= 2 && votes.iter().all(|vote| vote.needs_plan);

    // Highest score first; stable so ties keep candidate order (roster order).
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let top = scores
        .first()
        .cloned()
        .unwrap_or((candidates[0].clone(), 0.0));
    let second_score = scores.get(1).map(|(_, score)| *score).unwrap_or(0.0);
    let has_positive_leader = top.1 > 0.0;
    let has_unique_leader = top.1 > second_score + SCORE_TIE_EPSILON;
    let confident = has_positive_leader && has_unique_leader;
    // When no vote scored a candidate, or the top score is tied, still return a
    // deterministic executor but flag it as not-confident so callers/UI don't
    // present roster-order fallback as consensus.
    let executor = top.0;

    let mut rationale = String::new();
    if !has_positive_leader {
        rationale.push_str("(no scored vote — defaulting to the first candidate; please choose)\n");
    } else if !has_unique_leader {
        rationale.push_str(&format!(
            "(tied executor vote at {score:.2} — preselecting {executor} by roster order; please choose)\n",
            score = top.1
        ));
    }
    if needs_plan {
        rationale.push_str(
            "(both planner votes requested a formal plan; ask before turning this discussion into one)\n",
        );
    }
    for vote in &votes {
        rationale.push_str(&format!(
            "- {} → {} (conf {:.2}, needs_plan={}): {}\n",
            vote.voter_label,
            vote.recommended_executor,
            vote.confidence,
            vote.needs_plan,
            vote.rationale.trim()
        ));
    }

    Some(Recommendation {
        executor,
        scores,
        votes,
        rationale,
        needs_plan,
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
    fn parse_vote_accepts_common_executor_aliases_without_repair() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        let vote = parse_vote_lenient(
            r#"{"executor":"codex","confidence":0.7,"why":"cheap"}"#,
            &candidates,
        )
        .unwrap();

        assert_eq!(vote.recommended_executor, "codex");
        assert!((vote.confidence - 0.7).abs() < f32::EPSILON);
        assert_eq!(vote.rationale, "cheap");
        assert!(!vote.needs_plan);
        assert_eq!(vote.source, VoteParseSource::Json);

        let vote = parse_vote_lenient(
            r#"{"agent":"claude","reason":"best code","needs_plan":true}"#,
            &candidates,
        )
        .unwrap();
        assert_eq!(vote.recommended_executor, "claude");
        assert_eq!(vote.rationale, "best code");
        assert!(vote.needs_plan);
        assert_eq!(vote.source, VoteParseSource::Json);
    }

    #[test]
    fn parse_vote_lenient_salvages_single_candidate_prose() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        let vote = parse_vote_lenient(
            "I would have Claude execute this because the patch is file-heavy.",
            &candidates,
        )
        .unwrap();

        assert_eq!(vote.recommended_executor, "claude");
        assert_eq!(vote.source, VoteParseSource::CandidateMention);
        assert!((vote.confidence - 0.5).abs() < f32::EPSILON);
        assert!(!vote.needs_plan);
        assert!(vote.rationale.contains("salvaged from prose vote"));
    }

    #[test]
    fn parse_vote_lenient_refuses_ambiguous_or_partial_mentions() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        assert!(
            parse_vote_lenient("Claude can do it, but Codex is cheaper.", &candidates).is_none()
        );
        assert!(
            parse_vote_lenient("The claudean path is not a candidate mention.", &candidates)
                .is_none()
        );
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
                needs_plan: false,
                rationale: "x".into(),
                parse_source: VoteParseSource::Json,
            },
            ExecutionVote {
                voter: Uuid::new_v4(),
                voter_label: "codex".into(),
                recommended_executor: "claude".into(),
                confidence: 0.6,
                needs_plan: false,
                rationale: "y".into(),
                parse_source: VoteParseSource::Json,
            },
        ];
        let rec = aggregate(votes, &candidates).unwrap();
        assert_eq!(rec.executor, "claude");
        assert_eq!(rec.scores[0].0, "claude");
        assert!((rec.scores[0].1 - 1.4).abs() < 1e-6);
        assert!(rec.confident);
        assert!(!rec.needs_plan);
    }

    #[test]
    fn aggregate_requires_all_planner_votes_to_need_plan() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        let mk_vote = |needs_plan| ExecutionVote {
            voter: Uuid::new_v4(),
            voter_label: "agent".into(),
            recommended_executor: "claude".into(),
            confidence: 0.7,
            needs_plan,
            rationale: "x".into(),
            parse_source: VoteParseSource::Json,
        };

        let rec = aggregate(vec![mk_vote(true), mk_vote(true)], &candidates).unwrap();
        assert!(rec.needs_plan);
        assert!(rec.rationale.contains("formal plan"));

        let rec = aggregate(vec![mk_vote(true), mk_vote(false)], &candidates).unwrap();
        assert!(!rec.needs_plan);
    }

    #[test]
    fn aggregate_marks_tied_positive_votes_not_confident() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        let votes = vec![
            ExecutionVote {
                voter: Uuid::new_v4(),
                voter_label: "claude".into(),
                recommended_executor: "claude".into(),
                confidence: 0.7,
                needs_plan: false,
                rationale: "strong local context".into(),
                parse_source: VoteParseSource::Json,
            },
            ExecutionVote {
                voter: Uuid::new_v4(),
                voter_label: "codex".into(),
                recommended_executor: "codex".into(),
                confidence: 0.7,
                needs_plan: false,
                rationale: "cheaper executor".into(),
                parse_source: VoteParseSource::Json,
            },
        ];

        let rec = aggregate(votes, &candidates).unwrap();
        assert_eq!(rec.executor, "claude");
        assert_eq!(rec.scores[0], ("claude".to_string(), 0.7));
        assert_eq!(rec.scores[1], ("codex".to_string(), 0.7));
        assert!(!rec.confident);
        assert!(
            rec.rationale.contains("tied executor vote"),
            "{}",
            rec.rationale
        );
        assert!(rec.rationale.contains("please choose"), "{}", rec.rationale);
    }

    #[test]
    fn aggregate_marks_unmatched_votes_not_confident() {
        let candidates = vec!["claude".to_string(), "codex".to_string()];
        let votes = vec![ExecutionVote {
            voter: Uuid::new_v4(),
            voter_label: "claude".into(),
            recommended_executor: "nonexistent".into(),
            confidence: 0.9,
            needs_plan: false,
            rationale: "typo".into(),
            parse_source: VoteParseSource::Json,
        }];

        let rec = aggregate(votes, &candidates).unwrap();
        assert_eq!(rec.executor, "claude");
        assert!(!rec.confident);
        assert!(
            rec.rationale.contains("no scored vote"),
            "{}",
            rec.rationale
        );
    }
}
