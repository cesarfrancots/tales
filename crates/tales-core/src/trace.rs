//! Run traces — the coordinator's training flywheel.
//!
//! Each completed run appends one [`RunTrace`] to a local, append-only
//! `.tales/traces.jsonl`. The [`crate::coordinator::Coordinator`] retrains on the
//! *successful* ones so routing improves from real outcomes, not just the seed
//! priors. This is the only "memory" Tales keeps across runs, and it stays on
//! your machine: no trace file, no signal — never any telemetry.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::coordinator::Shape;
use crate::{Result, TalesError};

/// On-disk schema for a [`RunTrace`] line. Bump on a breaking field change.
pub const TRACE_SCHEMA_VERSION: u8 = 1;

/// One completed run, reduced to what the coordinator can learn from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RunTrace {
    pub schema_version: u8,
    pub created_at_unix: u64,
    /// The task as the user phrased it — the model's input.
    pub task: String,
    /// The collaboration shape actually used, when known.
    pub shape: Option<Shape>,
    /// Which tool executed.
    pub executor: String,
    /// Whether the run produced an accepted/verified result. Only successful
    /// runs become positive training examples.
    pub success: bool,
    pub cost_usd: Option<f64>,
    pub latency_secs: Option<u64>,
}

impl RunTrace {
    /// Build a trace stamped with the current time and the current schema.
    pub fn now(
        task: impl Into<String>,
        shape: Option<Shape>,
        executor: impl Into<String>,
        success: bool,
    ) -> RunTrace {
        RunTrace {
            schema_version: TRACE_SCHEMA_VERSION,
            created_at_unix: now_unix(),
            task: task.into(),
            shape,
            executor: executor.into(),
            success,
            cost_usd: None,
            latency_secs: None,
        }
    }

    pub fn with_cost(mut self, cost_usd: Option<f64>) -> RunTrace {
        self.cost_usd = cost_usd;
        self
    }

    pub fn with_latency(mut self, latency_secs: Option<u64>) -> RunTrace {
        self.latency_secs = latency_secs;
        self
    }
}

/// The append-only trace log for a workspace.
pub fn trace_log_path(workspace: &Path) -> PathBuf {
    workspace.join(".tales").join("traces.jsonl")
}

/// Append one trace as a JSON line. Best-effort durability: creates `.tales/` if
/// missing. Failures are returned so callers can log, but a write error must
/// never break a run.
pub fn append(workspace: &Path, trace: &RunTrace) -> Result<()> {
    let path = trace_log_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TalesError::Other(e.to_string()))?;
    }
    let line = serde_json::to_string(trace)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| TalesError::Other(e.to_string()))?;
    writeln!(file, "{line}").map_err(|e| TalesError::Other(e.to_string()))
}

/// Load all parseable traces, skipping blank or malformed lines so one corrupt
/// entry never blocks training.
pub fn load(workspace: &Path) -> Vec<RunTrace> {
    let path = trace_log_path(workspace);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<RunTrace>(l).ok())
        .filter(|t| t.schema_version == TRACE_SCHEMA_VERSION)
        .collect()
}

/// Turn traces into positive training examples: a *successful* run with a known
/// shape is evidence that shape suited that task. Failures and shape-less traces
/// carry no positive signal, so they're dropped.
pub fn training_examples(traces: &[RunTrace]) -> Vec<(String, Shape)> {
    traces
        .iter()
        .filter(|t| t.success)
        .filter_map(|t| t.shape.map(|shape| (t.task.clone(), shape)))
        .collect()
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
    fn append_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("tales-trace-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let t1 = RunTrace::now(
            "implement a regex engine",
            Some(Shape::Solo),
            "claude",
            true,
        )
        .with_cost(Some(1.53))
        .with_latency(Some(355));
        let t2 = RunTrace::now("add CRUD endpoints", Some(Shape::Tiered), "opencode", false);
        append(&dir, &t1).unwrap();
        append(&dir, &t2).unwrap();

        let loaded = load(&dir);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0], t1);

        // Only the successful, shaped trace becomes a training example.
        let examples = training_examples(&loaded);
        assert_eq!(examples.len(), 1);
        assert_eq!(examples[0].1, Shape::Solo);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_is_empty_when_no_log() {
        let dir = std::env::temp_dir().join(format!("tales-trace-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load(&dir).is_empty());
    }
}
