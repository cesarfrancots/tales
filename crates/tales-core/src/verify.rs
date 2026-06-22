//! Verify-and-iterate — the closed loop that turns a one-shot executor into one
//! that iterates to green.
//!
//! After the executor produces a diff, Tales runs a project check (tests, build,
//! lint — whatever command you name). If it fails, the failing output is fed back
//! to the executor and it tries again, up to a cap. This is the single change
//! that, in Tales' own benchmark, moved a cheap executor from 43.8% (blind) to
//! 100% (iterating against tests), and it is the verifier role at the heart of
//! how Fugu coordinates.
//!
//! The check is a *command* — deterministic ground truth — not an LLM critic: the
//! project's own tests are a better oracle than a second opinion, and a command
//! is trivially testable. An LLM-critic verifier can be added later as another
//! policy without touching the loop.
//!
//! Safety: the command runs *verbatim through the platform shell*, unsandboxed,
//! in the executor's working directory — it is the caller's responsibility to
//! scope it (it is not constrained by the agent's `--sandbox` policy, and without
//! a git worktree it runs against the live tree). Its combined stdout+stderr is
//! fed back into the executor's prompt, so a check that prints secrets would
//! surface them to the model; don't point it at one that does.

use std::path::{Path, PathBuf};

/// What to run to verify the executor's work, and how many retries to allow.
#[derive(Clone, Debug)]
pub struct VerificationPolicy {
    /// Shell command whose exit code decides pass/fail (e.g. `cargo test`).
    pub command: String,
    /// Directory to run the command in — the executor's working tree.
    pub cwd: PathBuf,
    /// Max executor retries after the first failure. `0` = verify once, no retry.
    pub max_iterations: u8,
}

impl VerificationPolicy {
    pub fn new(command: impl Into<String>, cwd: impl Into<PathBuf>, max_iterations: u8) -> Self {
        Self {
            command: command.into(),
            cwd: cwd.into(),
            max_iterations,
        }
    }
}

/// The result of running the check once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub passed: bool,
    /// Combined stdout+stderr, tail-bounded for prompting.
    pub output: String,
}

/// Cap on captured check output fed back to the executor — failures surface at
/// the end of test output, so we keep the tail.
const MAX_CHECK_OUTPUT_CHARS: usize = 6_000;

/// Run the check command in `cwd` via the platform shell. A non-zero exit (or a
/// spawn failure) is a fail; combined stdout+stderr is captured (tail-bounded).
pub async fn run_check(command: &str, cwd: &Path) -> CheckResult {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    match tokio::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .output()
        .await
    {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }
            CheckResult {
                passed: out.status.success(),
                output: tail_bounded(&combined, MAX_CHECK_OUTPUT_CHARS),
            }
        }
        Err(e) => CheckResult {
            passed: false,
            output: format!("failed to run check `{command}`: {e}"),
        },
    }
}

/// Build the feedback prompt sent back to the executor when the check fails.
/// Pure and bounded, so it's cheap to test and never blows the context budget.
pub fn feedback_prompt(
    task: &str,
    command: &str,
    check_output: &str,
    attempt: u8,
    max: u8,
) -> String {
    format!(
        "The verification check failed. Fix the code so it passes, then stop.\n\n\
         Original task: {task}\n\
         Check command: `{command}` (attempt {attempt} of {max})\n\n\
         Failing output:\n```\n{out}\n```\n\n\
         Make the minimal change that makes `{command}` pass. Do not change the \
         task's intent. If the failure is genuinely unrelated to your change, say \
         so explicitly instead of guessing.",
        out = check_output.trim(),
    )
}

/// Keep the last `max` chars of a string (test output's signal is at the end).
fn tail_bounded(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let tail: String = trimmed
        .chars()
        .rev()
        .take(max)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("(earlier output truncated; last {max} chars)\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_check_detects_pass_and_fail() {
        let cwd = std::env::temp_dir();
        let (pass, fail) = if cfg!(windows) {
            ("exit 0", "exit 1")
        } else {
            ("true", "false")
        };
        assert!(run_check(pass, &cwd).await.passed);
        assert!(!run_check(fail, &cwd).await.passed);
    }

    #[tokio::test]
    async fn run_check_captures_output() {
        let cwd = std::env::temp_dir();
        let result = run_check("echo verify-marker", &cwd).await;
        assert!(result.passed);
        assert!(result.output.contains("verify-marker"), "{}", result.output);
    }

    #[test]
    fn feedback_prompt_includes_task_command_and_output() {
        let p = feedback_prompt(
            "implement X",
            "cargo test",
            "assertion failed at line 5",
            1,
            3,
        );
        assert!(p.contains("implement X"));
        assert!(p.contains("cargo test"));
        assert!(p.contains("assertion failed"));
        assert!(p.contains("attempt 1 of 3"));
    }

    #[test]
    fn tail_bounded_keeps_the_end() {
        let s = format!("{}TAIL", "a".repeat(10_000));
        let b = tail_bounded(&s, 100);
        assert!(b.contains("truncated"), "{b}");
        assert!(b.ends_with("TAIL"), "{b}");
        assert!(b.chars().count() < 200);
    }
}
