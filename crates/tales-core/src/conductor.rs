//! Turn-taking — who speaks next in the discussion.
//!
//! The [`Conductor`] is the single authority on whose turn it is, which keeps
//! the relay deadlock-free (only ever one outstanding turn). M4 ships
//! [`RuleConductor`]: deterministic alternation for a fixed number of turns. An
//! LLM-driven conductor can later implement the same trait without touching the
//! orchestrator.

use crate::blackboard::Blackboard;
use crate::AgentId;

/// The part a participant plays in the discussion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Proposes and revises the plan/solution.
    Drafter,
    /// Critiques the draft and asks clarifying questions.
    Critic,
    /// The human-in-the-loop, interjecting into the conversation.
    Human,
}

/// An agent enrolled in the discussion.
#[derive(Clone, Debug)]
pub struct RosterEntry {
    pub agent: AgentId,
    pub label: String,
    pub role: Role,
}

/// The conductor's decision for the next step.
#[derive(Clone, Debug)]
pub struct TurnPlan {
    pub agent: AgentId,
    pub label: String,
    pub role: Role,
}

/// Decides who speaks next, or `None` to end the discussion.
pub trait Conductor {
    fn next_turn(&mut self, blackboard: &Blackboard) -> Option<TurnPlan>;
}

/// Deterministic round-robin over the roster for a fixed number of turns.
pub struct RuleConductor {
    roster: Vec<RosterEntry>,
    max_turns: usize,
    taken: usize,
}

impl RuleConductor {
    pub fn new(roster: Vec<RosterEntry>, max_turns: usize) -> Self {
        Self {
            roster,
            max_turns,
            taken: 0,
        }
    }
}

impl Conductor for RuleConductor {
    fn next_turn(&mut self, _blackboard: &Blackboard) -> Option<TurnPlan> {
        if self.roster.is_empty() || self.taken >= self.max_turns {
            return None;
        }
        let entry = &self.roster[self.taken % self.roster.len()];
        self.taken += 1;
        Some(TurnPlan {
            agent: entry.agent,
            label: entry.label.clone(),
            role: entry.role,
        })
    }
}
