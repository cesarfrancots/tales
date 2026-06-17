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
    /// Implements the agreed plan. Does NOT take planning turns or vote — it can
    /// be a separate, cheaper/faster tool than the planners (tiered execution:
    /// strong models plan, a cheap model executes).
    Executor,
    /// The human-in-the-loop, interjecting into the conversation.
    Human,
}

impl Role {
    /// Planners take discussion turns and vote on who executes; executors and
    /// humans do neither.
    pub fn is_planner(self) -> bool {
        matches!(self, Role::Drafter | Role::Critic)
    }
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
        // Only planners take discussion turns — a separate executor is enrolled
        // for the build step, not the debate.
        let roster = roster.into_iter().filter(|r| r.role.is_planner()).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn entry(role: Role) -> RosterEntry {
        RosterEntry {
            agent: Uuid::new_v4(),
            label: "x".to_string(),
            role,
        }
    }

    #[test]
    fn is_planner_excludes_executor_and_human() {
        assert!(Role::Drafter.is_planner());
        assert!(Role::Critic.is_planner());
        assert!(!Role::Executor.is_planner());
        assert!(!Role::Human.is_planner());
    }

    #[test]
    fn conductor_only_schedules_planners() {
        // An executor sits between the planners but must never be scheduled.
        let roster = vec![
            entry(Role::Drafter),
            entry(Role::Executor),
            entry(Role::Critic),
        ];
        let mut c = RuleConductor::new(roster, 4);
        let bb = Blackboard::default();
        let mut roles = Vec::new();
        while let Some(p) = c.next_turn(&bb) {
            roles.push(p.role);
        }
        assert_eq!(
            roles,
            vec![Role::Drafter, Role::Critic, Role::Drafter, Role::Critic]
        );
    }

    #[test]
    fn conductor_with_no_planners_schedules_nothing() {
        let mut c = RuleConductor::new(vec![entry(Role::Executor)], 4);
        assert!(c.next_turn(&Blackboard::default()).is_none());
    }
}
