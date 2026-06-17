//! Integration test for the worktree manager against a real temp git repo.

use std::path::{Path, PathBuf};
use std::process::Command;

use tales_core::worktree::{MergeOutcome, WorktreeManager};
use uuid::Uuid;

/// A throwaway git repo under the OS temp dir, cleaned up on drop.
struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tales-wt-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&path).unwrap();
        run(&path, &["init", "-q", "-b", "main"]);
        run(&path, &["config", "user.email", "test@tales.dev"]);
        run(&path, &["config", "user.name", "Tales Test"]);
        std::fs::write(path.join("README.md"), "base\n").unwrap();
        run(&path, &["add", "-A"]);
        run(&path, &["commit", "-q", "-m", "init"]);
        Self { path }
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn worktree_lifecycle_create_diff_merge_remove() {
    let repo = TempRepo::new();
    let mut mgr = WorktreeManager::init(&repo.path, "testrun")
        .await
        .expect("init");

    let agent = Uuid::new_v4();
    let wt = mgr.create(agent, "claude").await.expect("create worktree");
    assert!(wt.exists(), "worktree dir should exist");

    // Agent edits an existing file and creates a new one (untracked).
    std::fs::write(wt.join("README.md"), "base\nedited by claude\n").unwrap();
    std::fs::write(wt.join("feature.txt"), "new feature\n").unwrap();

    let diff = mgr.diff(agent).await.expect("diff");
    assert_eq!(diff.files_changed, 2, "modified + new file");
    assert!(diff.patch.contains("edited by claude"));
    assert!(diff.patch.contains("feature.txt"));

    // Merge the agent's branch into base.
    let outcome = mgr.commit_and_merge(agent).await.expect("merge");
    assert_eq!(outcome, MergeOutcome::Clean);

    // Base branch now contains the new file.
    assert!(
        repo.path.join("feature.txt").exists(),
        "merged feature.txt should be in base work tree"
    );

    mgr.remove(agent).await.expect("remove");
    assert!(!wt.exists(), "worktree should be pruned");
}

#[tokio::test]
async fn no_changes_reports_nochanges() {
    let repo = TempRepo::new();
    let mut mgr = WorktreeManager::init(&repo.path, "noop").await.unwrap();
    let agent = Uuid::new_v4();
    mgr.create(agent, "codex").await.unwrap();

    let diff = mgr.diff(agent).await.unwrap();
    assert!(diff.is_empty());

    let outcome = mgr.commit_and_merge(agent).await.unwrap();
    assert_eq!(outcome, MergeOutcome::NoChanges);

    mgr.remove(agent).await.unwrap();
}
