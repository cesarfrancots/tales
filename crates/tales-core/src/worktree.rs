//! Per-agent git worktree isolation.
//!
//! Each agent codes in its own `git worktree` on its own branch, so two agents
//! editing the same project can never clobber each other. The orchestrator
//! surfaces each worktree's diff, and on hand-off the user-chosen worktree is
//! committed and merged into the base branch while the losers are pruned.
//!
//! M3 shells out to `git` for everything (worktree add/remove, diff, merge).
//! A later pass can swap the read/diff paths to `git2` for structured hunks;
//! the public API here stays the same.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::{AgentId, Result, TalesError};

/// A live worktree bound to one agent.
#[derive(Clone, Debug)]
pub struct WorktreeInfo {
    pub agent: AgentId,
    pub label: String,
    pub branch: String,
    pub path: PathBuf,
}

/// The diff of a worktree against the run's base commit.
#[derive(Clone, Debug, Default)]
pub struct DiffSummary {
    pub files_changed: usize,
    /// `git diff --stat` text.
    pub stat: String,
    /// Full unified patch.
    pub patch: String,
}

impl DiffSummary {
    pub fn is_empty(&self) -> bool {
        self.files_changed == 0
    }
}

/// Result of merging an agent's worktree branch into the base branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Merged cleanly.
    Clean,
    /// The agent produced no changes.
    NoChanges,
    /// Merge hit conflicts in these files; base branch left mid-merge for the
    /// user to resolve or abort.
    Conflict { files: Vec<String> },
}

/// Captured output of a `git` invocation.
struct GitOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn git(cwd: &Path, args: &[&str]) -> Result<GitOut> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    Ok(GitOut {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Manages every agent's worktree for a single run.
pub struct WorktreeManager {
    base: PathBuf,
    run_id: String,
    /// The branch the base work tree was on at init (the merge target). `None`
    /// if HEAD was detached.
    base_branch: Option<String>,
    trees: HashMap<AgentId, WorktreeInfo>,
}

impl WorktreeManager {
    /// Create a manager rooted at an existing git repo. Errors if `base` is not
    /// inside a git work tree.
    pub async fn init(base: impl Into<PathBuf>, run_id: impl Into<String>) -> Result<Self> {
        let base = base.into();
        let check = git(&base, &["rev-parse", "--is-inside-work-tree"]).await?;
        if !check.ok || check.stdout.trim() != "true" {
            return Err(TalesError::Other(format!(
                "{} is not a git work tree",
                base.display()
            )));
        }
        // Capture the merge target up front. A detached HEAD yields None — that's
        // allowed for isolated worktree work, but merging back will be refused.
        let head = git(&base, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
        let base_branch = if head.ok && !head.stdout.trim().is_empty() {
            Some(head.stdout.trim().to_string())
        } else {
            None
        };
        Ok(Self {
            base,
            run_id: run_id.into(),
            base_branch,
            trees: HashMap::new(),
        })
    }

    /// The branch worktrees will be merged back into, if not detached.
    pub fn base_branch(&self) -> Option<&str> {
        self.base_branch.as_deref()
    }

    /// Add a fresh worktree + branch for `agent`, forked from the current HEAD.
    /// Returns the worktree path the agent should run in.
    pub async fn create(&mut self, agent: AgentId, label: &str) -> Result<PathBuf> {
        let short: String = agent.simple().to_string().chars().take(8).collect();
        let dir = self
            .base
            .join(".tales")
            .join("wt")
            .join(format!("{label}-{short}"));
        // Include the per-agent short id so branches never collide across runs
        // or between same-label agents within a run.
        let branch = format!("tales/{label}/{}-{short}", self.run_id);
        let dir_str = dir.to_string_lossy().into_owned();

        let out = git(
            &self.base,
            &["worktree", "add", "-b", &branch, &dir_str, "HEAD"],
        )
        .await?;
        if !out.ok {
            return Err(TalesError::Other(format!(
                "git worktree add failed: {}",
                out.stderr.trim()
            )));
        }

        self.trees.insert(
            agent,
            WorktreeInfo {
                agent,
                label: label.to_string(),
                branch,
                path: dir.clone(),
            },
        );
        Ok(dir)
    }

    /// Look up a worktree.
    pub fn get(&self, agent: AgentId) -> Result<&WorktreeInfo> {
        self.trees
            .get(&agent)
            .ok_or_else(|| TalesError::Other(format!("no worktree for agent {agent}")))
    }

    /// Porcelain status of a worktree — used between turns to detect stray
    /// writes / dirtiness.
    pub async fn status(&self, agent: AgentId) -> Result<String> {
        let wt = self.get(agent)?;
        let out = git(&wt.path, &["status", "--porcelain"]).await?;
        Ok(out.stdout)
    }

    /// Diff the worktree against the base commit, including newly created
    /// (untracked) files. Staging is harmless inside the isolated worktree.
    pub async fn diff(&self, agent: AgentId) -> Result<DiffSummary> {
        let wt = self.get(agent)?;
        // Stage everything so untracked files appear in the diff.
        let add = git(&wt.path, &["add", "-A"]).await?;
        if !add.ok {
            return Err(TalesError::Other(format!(
                "git add -A failed: {}",
                add.stderr.trim()
            )));
        }

        let names = git(&wt.path, &["diff", "--cached", "--name-only", "HEAD"]).await?;
        let files_changed = names
            .stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        let stat = git(
            &wt.path,
            &["--no-pager", "diff", "--cached", "--stat", "HEAD"],
        )
        .await?;
        let patch = git(&wt.path, &["--no-pager", "diff", "--cached", "HEAD"]).await?;

        Ok(DiffSummary {
            files_changed,
            stat: stat.stdout,
            patch: patch.stdout,
        })
    }

    /// Commit the agent's work and merge its branch into the base branch.
    /// Returns [`MergeOutcome::NoChanges`] if the agent changed nothing.
    pub async fn commit_and_merge(&self, agent: AgentId) -> Result<MergeOutcome> {
        let wt = self.get(agent)?;

        // Refuse to merge into a detached base HEAD — the result would land on a
        // nameless commit and be silently stranded.
        let base_branch = self.base_branch.as_deref().ok_or_else(|| {
            TalesError::Other("base repo HEAD is detached; refusing to merge".to_string())
        })?;

        let add = git(&wt.path, &["add", "-A"]).await?;
        if !add.ok {
            return Err(TalesError::Other(format!(
                "git add -A failed: {}",
                add.stderr.trim()
            )));
        }
        let staged = git(&wt.path, &["diff", "--cached", "--name-only"]).await?;
        if staged.stdout.trim().is_empty() {
            return Ok(MergeOutcome::NoChanges);
        }

        let msg = format!("tales: {} result", wt.label);
        let commit = git(&wt.path, &["commit", "-m", &msg]).await?;
        if !commit.ok {
            return Err(TalesError::Other(format!(
                "git commit failed: {}",
                commit.stderr.trim()
            )));
        }

        // The base must still be on its branch, or the merge would land somewhere
        // unexpected.
        let cur = git(&self.base, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
        if cur.stdout.trim() != base_branch {
            return Err(TalesError::Other(format!(
                "base repo is not on '{base_branch}' (now '{}'); refusing to merge",
                cur.stdout.trim()
            )));
        }

        let merge_msg = format!("tales: merge {}", wt.label);
        let merge = git(
            &self.base,
            &["merge", "--no-ff", &wt.branch, "-m", &merge_msg],
        )
        .await?;
        if merge.ok {
            return Ok(MergeOutcome::Clean);
        }

        // A non-zero merge is a real *content conflict* only if a merge actually
        // started (MERGE_HEAD exists). Otherwise it failed to even begin (dirty
        // base, untracked overwrite, …) — that's a hard error, not a bogus
        // "conflict in zero files".
        let merge_head = git(&self.base, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).await?;
        if !merge_head.ok {
            return Err(TalesError::Other(format!(
                "git merge failed to start: {}",
                merge.stderr.trim()
            )));
        }

        let conflicts = git(&self.base, &["diff", "--name-only", "--diff-filter=U"]).await?;
        let files: Vec<String> = conflicts
            .stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        Ok(MergeOutcome::Conflict { files })
    }

    /// Remove an agent's worktree and delete its branch (e.g. a losing
    /// candidate after hand-off).
    pub async fn remove(&mut self, agent: AgentId) -> Result<()> {
        let Some(wt) = self.trees.get(&agent).cloned() else {
            return Ok(());
        };
        let path = wt.path.to_string_lossy().into_owned();

        let rm = git(&self.base, &["worktree", "remove", "--force", &path]).await?;
        if !rm.ok {
            // Reconcile any stale registration, but report the failure rather
            // than leaking it silently.
            let _ = git(&self.base, &["worktree", "prune"]).await;
            return Err(TalesError::Other(format!(
                "git worktree remove failed for {path}: {}",
                rm.stderr.trim()
            )));
        }

        let br = git(&self.base, &["branch", "-D", &wt.branch]).await?;
        if !br.ok {
            tracing::warn!(
                "failed to delete branch {}: {}",
                wt.branch,
                br.stderr.trim()
            );
        }
        // Only drop the registration once the worktree is actually gone.
        self.trees.remove(&agent);
        Ok(())
    }

    /// Every live worktree.
    pub fn worktrees(&self) -> impl Iterator<Item = &WorktreeInfo> {
        self.trees.values()
    }
}
