//! Local workspace metadata memory.
//!
//! Profiles intentionally store operational metadata only: commands, tool
//! preferences, warnings, report paths, and run telemetry. They do not store
//! source snippets by default.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{bin_on_path, KNOWN_TOOLS};
use crate::{Result, TalesError, TokenUsage};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceProfile {
    pub schema_version: u8,
    pub workspace: String,
    pub commands: Vec<String>,
    pub preferred_tools: Vec<String>,
    pub conventions: Vec<String>,
    pub last_successful_checks: Vec<String>,
    pub latency_cost: Vec<ToolRunStats>,
    pub known_warnings: Vec<String>,
    pub approved_report_paths: Vec<String>,
    pub runs: Vec<WorkspaceRunRecord>,
    pub updated_at_unix: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRunStats {
    pub tool_key: String,
    pub runs: u64,
    pub elapsed_ms_total: u64,
    pub prompt_chars_total: u64,
    pub reported_tokens_total: Option<u64>,
    pub cost_micros_total: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRunRecord {
    pub task_label: String,
    pub executor: Option<String>,
    pub approved: bool,
    pub elapsed_ms: Option<u64>,
    pub prompt_count: Option<u64>,
    pub prompt_chars: Option<u64>,
    pub reported_tokens: Option<u64>,
    pub cost_micros: Option<u64>,
    pub report_path: Option<String>,
    pub created_at_unix: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ProfileUpdate {
    pub command: Option<String>,
    pub successful_check: Option<String>,
    pub warning: Option<String>,
    pub report_path: Option<String>,
    pub run: Option<WorkspaceRunRecord>,
    pub tool_stats: Option<ToolRunStats>,
}

impl WorkspaceProfile {
    pub fn new(root: &Path) -> Self {
        Self {
            schema_version: 1,
            workspace: root.display().to_string(),
            updated_at_unix: now_unix(),
            ..Self::default()
        }
    }

    pub fn apply_update(&mut self, update: ProfileUpdate) {
        push_unique(&mut self.commands, update.command);
        push_unique(&mut self.last_successful_checks, update.successful_check);
        push_unique(&mut self.known_warnings, update.warning);
        push_unique(&mut self.approved_report_paths, update.report_path);
        if let Some(run) = update.run {
            self.runs.push(run);
            if self.runs.len() > 20 {
                let excess = self.runs.len() - 20;
                self.runs.drain(0..excess);
            }
        }
        if let Some(stats) = update.tool_stats {
            merge_tool_stats(&mut self.latency_cost, stats);
        }
        self.updated_at_unix = now_unix();
    }

    pub fn prompt_hints(&self) -> Vec<String> {
        let mut hints = Vec::new();
        if !self.commands.is_empty() {
            hints.push(format!("workspace_commands: {}", self.commands.join(" | ")));
        }
        if !self.preferred_tools.is_empty() {
            hints.push(format!(
                "preferred_tools: {}",
                self.preferred_tools.join(", ")
            ));
        }
        if !self.last_successful_checks.is_empty() {
            hints.push(format!(
                "last_successful_checks: {}",
                self.last_successful_checks.join(" | ")
            ));
        }
        if !self.known_warnings.is_empty() {
            hints.push(format!(
                "known_warnings: {}",
                self.known_warnings.join(" | ")
            ));
        }
        hints
    }
}

pub fn load_profile(root: &Path, cache_dir: Option<&Path>) -> Result<WorkspaceProfile> {
    let root = canonical_root(root);
    let path = profile_path_for(&root, cache_dir);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let mut profile: WorkspaceProfile = serde_json::from_str(&text)?;
            if profile.schema_version == 0 {
                profile.schema_version = 1;
            }
            Ok(profile)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(refresh_profile(&root, cache_dir)?)
        }
        Err(err) => Err(TalesError::Other(format!(
            "cannot read workspace profile {}: {err}",
            path.display()
        ))),
    }
}

pub fn load_profile_if_exists(
    root: &Path,
    cache_dir: Option<&Path>,
) -> Result<Option<WorkspaceProfile>> {
    let root = canonical_root(root);
    let path = profile_path_for(&root, cache_dir);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(TalesError::Other(format!(
            "cannot read workspace profile {}: {err}",
            path.display()
        ))),
    }
}

pub fn refresh_profile(root: &Path, cache_dir: Option<&Path>) -> Result<WorkspaceProfile> {
    let root = canonical_root(root);
    let mut profile = WorkspaceProfile::new(&root);
    profile.commands = detect_commands(&root);
    profile.preferred_tools = KNOWN_TOOLS
        .iter()
        .filter(|tool| bin_on_path(tool.bin))
        .map(|tool| tool.key.to_string())
        .collect();
    profile.conventions = detect_conventions(&root);
    save_profile(&root, cache_dir, &profile)?;
    Ok(profile)
}

pub fn save_profile(
    root: &Path,
    cache_dir: Option<&Path>,
    profile: &WorkspaceProfile,
) -> Result<PathBuf> {
    let root = canonical_root(root);
    let path = profile_path_for(&root, cache_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| TalesError::Other(e.to_string()))?;
    }
    let text = serde_json::to_string_pretty(profile)?;
    fs::write(&path, text).map_err(|e| TalesError::Other(e.to_string()))?;
    Ok(path)
}

pub fn clear_profile(root: &Path, cache_dir: Option<&Path>) -> Result<PathBuf> {
    let root = canonical_root(root);
    let path = profile_path_for(&root, cache_dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(err) => Err(TalesError::Other(format!(
            "cannot clear workspace profile {}: {err}",
            path.display()
        ))),
    }
}

pub fn profile_status_json(
    root: &Path,
    cache_dir: Option<&Path>,
    profile: &WorkspaceProfile,
) -> Value {
    let root = canonical_root(root);
    let path = profile_path_for(&root, cache_dir);
    json!({
        "schema_version": profile.schema_version,
        "workspace": profile.workspace,
        "path": path.display().to_string(),
        "commands": profile.commands,
        "preferred_tools": profile.preferred_tools,
        "conventions": profile.conventions,
        "last_successful_checks": profile.last_successful_checks,
        "known_warnings": profile.known_warnings,
        "approved_report_paths": profile.approved_report_paths,
        "runs": profile.runs,
        "latency_cost": profile.latency_cost,
        "prompt_hints": profile.prompt_hints(),
        "updated_at_unix": profile.updated_at_unix,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn run_record_from_report(
    task_label: impl Into<String>,
    executor: Option<String>,
    approved: bool,
    elapsed_ms: Option<u64>,
    prompt_count: Option<u64>,
    prompt_chars: Option<u64>,
    token_usage: Option<TokenUsage>,
    cost_usd: Option<f64>,
    report_path: Option<String>,
) -> WorkspaceRunRecord {
    WorkspaceRunRecord {
        task_label: task_label.into(),
        executor,
        approved,
        elapsed_ms,
        prompt_count,
        prompt_chars,
        reported_tokens: token_usage.and_then(|usage| usage.total_or_sum()),
        cost_micros: cost_usd.map(|cost| (cost.max(0.0) * 1_000_000.0).round() as u64),
        report_path,
        created_at_unix: now_unix(),
    }
}

pub fn profile_path_for(root: &Path, override_dir: Option<&Path>) -> PathBuf {
    let root = canonical_root(root);
    override_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_profile_root)
        .join(format!("{:016x}", stable_hash(&root.to_string_lossy())))
        .join("profile.json")
}

fn default_profile_root() -> PathBuf {
    if let Ok(dir) = std::env::var("TALES_CACHE_DIR") {
        return PathBuf::from(dir).join("workspaces");
    }
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("tales").join("workspaces");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("tales")
            .join("workspaces");
    }
    std::env::temp_dir().join("tales").join("workspaces")
}

fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn detect_commands(root: &Path) -> Vec<String> {
    let mut commands = Vec::new();
    if root.join("Cargo.toml").is_file() {
        commands.extend([
            "cargo fmt --check".to_string(),
            "cargo clippy --workspace --all-targets -- -D warnings".to_string(),
            "cargo test -q --workspace".to_string(),
        ]);
    }
    if let Ok(package) = fs::read_to_string(root.join("package.json")) {
        if let Ok(value) = serde_json::from_str::<Value>(&package) {
            if let Some(scripts) = value.get("scripts").and_then(Value::as_object) {
                for key in ["lint", "typecheck", "test", "build"] {
                    if scripts.contains_key(key) {
                        commands.push(format!("npm run {key}"));
                    }
                }
            }
        }
    }
    commands
}

fn detect_conventions(root: &Path) -> Vec<String> {
    let mut conventions = Vec::new();
    if root.join("Cargo.toml").is_file() {
        conventions.push("rust_workspace".into());
    }
    if root.join("package.json").is_file() {
        conventions.push("node_package".into());
    }
    if root.join(".github/workflows").is_dir() {
        conventions.push("github_actions".into());
    }
    if root.join("README.md").is_file() {
        conventions.push("readme_present".into());
    }
    conventions
}

fn push_unique(items: &mut Vec<String>, item: Option<String>) {
    let Some(item) = item.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return;
    };
    if !items.iter().any(|existing| existing == &item) {
        items.push(item);
    }
}

fn merge_tool_stats(items: &mut Vec<ToolRunStats>, next: ToolRunStats) {
    if let Some(existing) = items.iter_mut().find(|item| item.tool_key == next.tool_key) {
        existing.runs = existing.runs.saturating_add(next.runs);
        existing.elapsed_ms_total = existing
            .elapsed_ms_total
            .saturating_add(next.elapsed_ms_total);
        existing.prompt_chars_total = existing
            .prompt_chars_total
            .saturating_add(next.prompt_chars_total);
        existing.reported_tokens_total =
            add_optional(existing.reported_tokens_total, next.reported_tokens_total);
        existing.cost_micros_total =
            add_optional(existing.cost_micros_total, next.cost_micros_total);
    } else {
        items.push(next);
    }
}

fn add_optional(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn stable_hash(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip_stores_metadata_only() {
        let root = std::env::temp_dir().join(format!("tales-profile-test-{}", std::process::id()));
        let cache =
            std::env::temp_dir().join(format!("tales-profile-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&cache);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();

        let mut profile = refresh_profile(&root, Some(&cache)).unwrap();
        profile.apply_update(ProfileUpdate {
            command: Some("cargo test -q --workspace".into()),
            warning: Some("known flaky integration test".into()),
            ..ProfileUpdate::default()
        });
        save_profile(&root, Some(&cache), &profile).unwrap();

        let loaded = load_profile(&root, Some(&cache)).unwrap();
        assert!(loaded.commands.iter().any(|cmd| cmd.contains("cargo test")));
        assert!(loaded
            .prompt_hints()
            .join("\n")
            .contains("workspace_commands"));
        assert!(!serde_json::to_string(&loaded).unwrap().contains("fn main"));

        clear_profile(&root, Some(&cache)).unwrap();
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(cache);
    }

    #[test]
    fn profile_path_is_stable_per_workspace() {
        let root = PathBuf::from("/tmp/tales-workspace");
        assert_eq!(
            profile_path_for(&root, Some(Path::new("/tmp/cache"))),
            profile_path_for(&root, Some(Path::new("/tmp/cache")))
        );
    }
}
