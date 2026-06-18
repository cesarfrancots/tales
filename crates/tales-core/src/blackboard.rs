//! Shared discussion state — the transcript the agents build together.

use crate::conductor::Role;

/// One contribution to the discussion.
#[derive(Clone, Debug)]
pub struct Utterance {
    pub label: String,
    pub role: Role,
    pub text: String,
}

/// The collaborative scratchpad: the task plus every utterance so far.
#[derive(Default, Debug)]
pub struct Blackboard {
    pub task: String,
    pub transcript: Vec<Utterance>,
}

impl Blackboard {
    /// Append an utterance.
    pub fn record(&mut self, label: String, role: Role, text: String) {
        self.transcript.push(Utterance { label, role, text });
    }

    /// The text of the most recent utterance, if any.
    pub fn last_text(&self) -> Option<&str> {
        self.transcript.last().map(|u| u.text.as_str())
    }

    /// The whole discussion, formatted for feeding back to an agent.
    pub fn transcript_text(&self) -> String {
        self.transcript_text_from(0)
    }

    /// The discussion from utterance `from` onward, formatted for an agent — the
    /// "delta" a resumable agent hasn't seen yet (its own earlier turns already
    /// live in its server-side session, so only the tail need be re-sent).
    pub fn transcript_text_from(&self, from: usize) -> String {
        let mut out = String::new();
        for u in self.transcript.iter().skip(from) {
            out.push_str(&format!("[{} — {:?}]\n{}\n\n", u.label, u.role, u.text));
        }
        out
    }
}
