//! Cached project context for cheaper agent orientation.
//!
//! This is intentionally deterministic and local: Tales builds a compact repo
//! snapshot from filenames plus a few manifest excerpts, caches it outside the
//! project directory, and injects it into first planning prompts.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

use crate::prompt_forecast::estimated_tokens;
use crate::Result;

const CACHE_VERSION: &str = "project-context-v7";
const DEFAULT_MAX_FILES: usize = 220;
const DEFAULT_MAX_MANIFEST_CHARS: usize = 1_200;
const MAX_GIT_STATUS_LINES: usize = 80;
pub const DEFAULT_LOCAL_CHANGE_LINES: usize = 40;

#[derive(Clone, Debug)]
pub struct ProjectContextOptions {
    pub max_files: usize,
    pub max_manifest_chars: usize,
    pub refresh: bool,
    pub cache_dir: Option<PathBuf>,
}

impl Default for ProjectContextOptions {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_manifest_chars: DEFAULT_MAX_MANIFEST_CHARS,
            refresh: false,
            cache_dir: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProjectContext {
    pub text: String,
    pub cache_hit: bool,
    pub cache_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct LocalChangeSummary {
    pub branch: Option<String>,
    pub changes: Vec<String>,
    pub truncated: bool,
    pub omitted: usize,
}

impl LocalChangeSummary {
    pub fn is_clean(&self) -> bool {
        self.changes.is_empty() && self.omitted == 0 && !self.truncated
    }

    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(branch) = &self.branch {
            parts.push(format!("branch {branch}"));
        }
        if self.changes.is_empty() {
            parts.push("clean".to_string());
        } else if self.truncated {
            parts.push(format!("at least {} visible change(s)", self.changes.len()));
        } else {
            parts.push(format!("{} visible change(s)", self.changes.len()));
        }
        if self.omitted > 0 {
            parts.push(format!(
                "{} generated/dependency entry(ies) omitted",
                self.omitted
            ));
        }
        parts.join("; ")
    }

    pub fn to_handoff_text(&self) -> String {
        let mut out = String::new();
        out.push_str("Local changes before execution (git status --short)\n");
        if let Some(branch) = &self.branch {
            out.push_str(&format!("- branch: {branch}\n"));
        }
        if self.changes.is_empty() {
            out.push_str("- changes: clean\n");
        } else {
            out.push_str("- changes:\n");
            for line in &self.changes {
                out.push_str(&format!("  - {line}\n"));
            }
            if self.truncated {
                out.push_str(&format!(
                    "  - ... (status truncated after {} visible entries)\n",
                    self.changes.len()
                ));
            }
        }
        if self.omitted > 0 {
            out.push_str(&format!(
                "- omitted_generated_or_dependency_entries: {}\n",
                self.omitted
            ));
        }
        out.push_str(
            "- guidance: preserve unrelated existing changes; inspect touched files before editing.\n",
        );
        out
    }
}

pub fn context_budget_fit_json(context_chars: usize, budget_chars: Option<usize>) -> Value {
    let context_tokens = estimated_tokens(context_chars);
    match budget_chars {
        Some(budget) => {
            let budget_tokens = estimated_tokens(budget);
            let percent = if budget == 0 {
                0
            } else {
                context_chars.saturating_mul(100).div_ceil(budget)
            };
            json!({
                "status": if context_chars > budget { "over_budget" } else { "fits" },
                "context_chars": context_chars,
                "budget_chars": budget,
                "percent": percent,
                "context_tokens_estimate": context_tokens,
                "budget_tokens_estimate": budget_tokens,
            })
        }
        None => json!({
            "status": "unlimited",
            "context_chars": context_chars,
            "budget_chars": null,
            "percent": null,
            "context_tokens_estimate": context_tokens,
            "budget_tokens_estimate": null,
        }),
    }
}

pub fn project_context_status_json(
    ctx: Option<&ProjectContext>,
    refresh: bool,
    max_files: usize,
    max_manifest_chars: usize,
    budget_chars: Option<usize>,
) -> Value {
    match ctx {
        Some(ctx) => {
            let chars = ctx.text.chars().count();
            let budget_fit = context_budget_fit_json(chars, budget_chars);
            json!({
                "enabled": true,
                "refresh": refresh,
                "cache_hit": ctx.cache_hit,
                "cache_path": ctx.cache_path.display().to_string(),
                "chars": chars,
                "tokens_estimate": estimated_tokens(chars),
                "budget_fit": budget_fit.clone(),
                "default_budget_fit": budget_fit,
                "max_files": max_files,
                "max_manifest_chars": max_manifest_chars,
                "max_manifest_tokens_estimate": estimated_tokens(max_manifest_chars),
                "budgets": {
                    "max_files": max_files,
                    "max_manifest_chars": max_manifest_chars,
                },
            })
        }
        None => json!({
            "enabled": false,
            "refresh": refresh,
            "cache_hit": null,
            "cache_path": null,
            "chars": null,
            "tokens_estimate": null,
            "budget_fit": null,
            "default_budget_fit": null,
            "max_files": max_files,
            "max_manifest_chars": max_manifest_chars,
            "max_manifest_tokens_estimate": estimated_tokens(max_manifest_chars),
            "budgets": {
                "max_files": max_files,
                "max_manifest_chars": max_manifest_chars,
            },
        }),
    }
}

pub fn local_change_summary_status_json(changes: Option<&LocalChangeSummary>) -> Value {
    match changes {
        Some(changes) => {
            let handoff_text = changes.to_handoff_text();
            let handoff_chars = handoff_text.chars().count();
            let visible_changes = changes.changes.clone();
            let preview = visible_changes.iter().take(8).cloned().collect::<Vec<_>>();
            json!({
                "available": true,
                "summary": changes.summary_line(),
                "branch": changes.branch.as_deref(),
                "clean": changes.is_clean(),
                "visible_count": visible_changes.len(),
                "visible_changes": visible_changes.len(),
                "changes": visible_changes,
                "preview": preview,
                "truncated": changes.truncated,
                "omitted": changes.omitted,
                "omitted_generated_or_dependency_entries": changes.omitted,
                "handoff_chars": handoff_chars,
                "handoff_tokens_estimate": estimated_tokens(handoff_chars),
            })
        }
        None => json!({
            "available": false,
            "summary": "unavailable",
            "reason": "not a git repository or git unavailable",
            "handoff_chars": null,
            "handoff_tokens_estimate": null,
        }),
    }
}

#[derive(Clone, Debug)]
struct FileInfo {
    rel: String,
    len: u64,
    modified_nanos: u128,
    manifest_excerpt_hash: Option<u64>,
}

#[derive(Clone, Debug)]
struct CollectedFiles {
    files: Vec<FileInfo>,
    manifest_files: Vec<FileInfo>,
    truncated: bool,
}

#[derive(Clone, Debug)]
struct GitStatus {
    branch: Option<String>,
    changes: Vec<String>,
    truncated: bool,
}

pub fn load_or_build(root: &Path, opts: ProjectContextOptions) -> Result<ProjectContext> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let collected = collect_files(&root, opts.max_files, opts.max_manifest_chars);
    let git_status = git_status_for(&root);
    let signature = signature_for(
        &root,
        &collected.files,
        &collected.manifest_files,
        opts.max_files,
        opts.max_manifest_chars,
    );
    let cache_path = cache_path_for(&root, opts.cache_dir.as_deref());

    if !opts.refresh {
        if let Ok(cached) = fs::read_to_string(&cache_path) {
            if let Some(text) = read_cached_context(&cached, &signature) {
                return Ok(ProjectContext {
                    text: with_git_status_context(text, git_status.as_ref()),
                    cache_hit: true,
                    cache_path,
                });
            }
        }
    }

    let text = build_context_text(
        &root,
        &collected.files,
        &collected.manifest_files,
        collected.truncated,
        opts.max_files,
        opts.max_manifest_chars,
    );
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let payload = format!("{CACHE_VERSION}\nsignature:{signature}\n---\n{text}");
    let _ = fs::write(&cache_path, payload);

    Ok(ProjectContext {
        text: with_git_status_context(text, git_status.as_ref()),
        cache_hit: false,
        cache_path,
    })
}

pub fn local_change_summary(root: &Path, max_lines: usize) -> Option<LocalChangeSummary> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--short", "--branch", "--untracked-files=all"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut branch = None;
    let mut changes = Vec::new();
    let mut truncated = false;
    let mut omitted = 0usize;
    let max_lines = max_lines.max(1);

    for raw in stdout
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
    {
        if let Some(rest) = raw.strip_prefix("## ") {
            branch = Some(rest.trim().to_string());
            continue;
        }
        if local_change_line_should_skip(raw) {
            omitted += 1;
            continue;
        }
        if changes.len() >= max_lines {
            truncated = true;
            continue;
        }
        changes.push(raw.to_string());
    }

    Some(LocalChangeSummary {
        branch,
        changes,
        truncated,
        omitted,
    })
}

fn read_cached_context(payload: &str, signature: &str) -> Option<String> {
    let (header, body) = payload.split_once("\n---\n")?;
    if header.lines().next()? != CACHE_VERSION {
        return None;
    }
    let expected = format!("signature:{signature}");
    if !header.lines().any(|line| line == expected) {
        return None;
    }
    Some(body.to_string())
}

fn cache_path_for(root: &Path, override_dir: Option<&Path>) -> PathBuf {
    let name = format!("{:016x}.txt", stable_hash_parts(&[&root.to_string_lossy()]));
    override_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_cache_dir)
        .join(name)
}

fn default_cache_dir() -> PathBuf {
    if let Ok(dir) = env::var("TALES_CACHE_DIR") {
        return PathBuf::from(dir).join("project-context");
    }
    if let Ok(dir) = env::var("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("tales").join("project-context");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("tales")
            .join("project-context");
    }
    env::temp_dir().join("tales").join("project-context")
}

fn signature_for(
    root: &Path,
    files: &[FileInfo],
    manifest_files: &[FileInfo],
    max_files: usize,
    max_manifest_chars: usize,
) -> String {
    let mut hash = stable_hash_parts(&[CACHE_VERSION, &root.to_string_lossy()]);
    hash = stable_hash_extend(hash, &max_files.to_le_bytes());
    hash = stable_hash_extend(hash, &max_manifest_chars.to_le_bytes());
    for file in files {
        hash = stable_hash_extend(hash, file.rel.as_bytes());
        hash = stable_hash_extend(hash, &file.len.to_le_bytes());
        if is_manifest(&file.rel) {
            hash = stable_hash_extend(hash, &[0x02]);
        } else {
            hash = stable_hash_extend(hash, &[0x03]);
        }
    }
    hash = stable_hash_extend(hash, &[0xfe]);
    for file in manifest_files {
        hash = stable_hash_extend(hash, file.rel.as_bytes());
        hash = stable_hash_extend(hash, &file.len.to_le_bytes());
        if let Some(excerpt_hash) = file.manifest_excerpt_hash {
            hash = stable_hash_extend(hash, &[0x01]);
            hash = stable_hash_extend(hash, &excerpt_hash.to_le_bytes());
        } else {
            hash = stable_hash_extend(hash, &[0x00]);
            hash = stable_hash_extend(hash, &file.modified_nanos.to_le_bytes());
        }
    }
    format!("{hash:016x}")
}

fn stable_hash_parts(parts: &[&str]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for part in parts {
        hash = stable_hash_extend(hash, part.as_bytes());
        hash = stable_hash_extend(hash, &[0xff]);
    }
    hash
}

fn stable_hash_extend(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn collect_files(root: &Path, max_files: usize, max_manifest_chars: usize) -> CollectedFiles {
    let mut out = Vec::new();
    let collection_cap = max_files.saturating_mul(4).max(512);
    collect_files_inner(root, root, collection_cap, &mut out);
    out.sort_by(|a, b| context_rank(&a.rel).cmp(&context_rank(&b.rel)));
    let manifest_files = manifest_files(root, &out, max_manifest_chars);
    let truncated = out.len() > max_files;
    if out.len() > max_files {
        out.truncate(max_files);
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    CollectedFiles {
        files: out,
        manifest_files,
        truncated,
    }
}

fn collect_files_inner(root: &Path, dir: &Path, collection_cap: usize, out: &mut Vec<FileInfo>) {
    if out.len() >= collection_cap {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|entry| entry.ok()).collect();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let rel = match path.strip_prefix(root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if should_skip(rel) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_files_inner(root, &path, collection_cap, out);
        } else if meta.is_file() {
            if out.len() >= collection_cap {
                return;
            }
            let modified_nanos = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            out.push(FileInfo {
                rel: rel.to_string_lossy().replace('\\', "/"),
                len: meta.len(),
                modified_nanos,
                manifest_excerpt_hash: None,
            });
        }
    }
}

fn context_rank(rel: &str) -> (u8, &str) {
    if is_manifest(rel) {
        return (0, rel);
    }
    if is_hidden_or_harness_file(rel) {
        return (5, rel);
    }
    if is_source_entry(rel) {
        return (1, rel);
    }
    if is_source_file(rel) {
        return (2, rel);
    }
    if is_doc_or_config(rel) {
        return (3, rel);
    }
    if is_binary_or_media(rel) {
        return (6, rel);
    }
    (4, rel)
}

fn should_skip(rel: &Path) -> bool {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if is_generated_output_path(&rel_str) {
        return true;
    }
    rel.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            ".git"
                | ".hg"
                | ".svn"
                | ".tales"
                | ".playwright-cli"
                | "target"
                | "node_modules"
                | ".next"
                | "dist"
                | "build"
                | ".cache"
                | ".turbo"
                | "coverage"
                | ".DS_Store"
        )
    })
}

fn is_source_entry(rel: &str) -> bool {
    rel.ends_with("/src/main.rs")
        || rel.ends_with("/src/lib.rs")
        || rel.ends_with("/src/main.ts")
        || rel.ends_with("/src/index.ts")
        || rel.ends_with("/src/main.tsx")
        || rel.ends_with("/src/index.tsx")
        || rel.ends_with("/main.py")
        || rel.ends_with("/app.py")
        || rel == "src/main.rs"
        || rel == "src/lib.rs"
        || rel == "main.py"
        || rel == "app.py"
}

fn is_source_file(rel: &str) -> bool {
    let Some(ext) = rel.rsplit('.').next() else {
        return false;
    };
    matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "py"
            | "go"
            | "swift"
            | "kt"
            | "java"
            | "c"
            | "cc"
            | "cpp"
            | "h"
            | "hpp"
            | "toml"
            | "json"
            | "yaml"
            | "yml"
            | "html"
            | "css"
    )
}

fn is_doc_or_config(rel: &str) -> bool {
    rel == "README.md"
        || rel == "LICENSE"
        || rel == ".gitignore"
        || rel.ends_with(".md")
        || rel.ends_with(".toml")
        || rel.ends_with(".json")
        || rel.ends_with(".yaml")
        || rel.ends_with(".yml")
}

fn is_hidden_or_harness_file(rel: &str) -> bool {
    rel.starts_with('.') || rel.starts_with("codex/") || rel.starts_with(".claude/")
}

fn is_binary_or_media(rel: &str) -> bool {
    let Some(ext) = rel.rsplit('.').next() else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "mp4"
            | "mov"
            | "webm"
            | "pdf"
            | "zip"
            | "gz"
            | "tar"
            | "wasm"
    )
}

fn git_status_for(root: &Path) -> Option<GitStatus> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--short", "--branch", "--untracked-files=all"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut branch = None;
    let mut changes = Vec::new();
    let mut truncated = false;

    for raw in stdout
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
    {
        if let Some(rest) = raw.strip_prefix("## ") {
            branch = Some(rest.trim().to_string());
            continue;
        }
        if git_status_line_should_skip(raw) {
            continue;
        }
        if changes.len() >= MAX_GIT_STATUS_LINES {
            truncated = true;
            continue;
        }
        changes.push(raw.to_string());
    }

    Some(GitStatus {
        branch,
        changes,
        truncated,
    })
}

fn git_status_line_should_skip(line: &str) -> bool {
    let path_part = line.get(3..).unwrap_or(line).trim();
    if path_part.is_empty() {
        return false;
    }
    let paths: Vec<&str> = path_part.split(" -> ").collect();
    !paths.is_empty()
        && paths.iter().all(|path| {
            let normalized = path.trim().trim_matches('"');
            normalized.is_empty() || git_status_path_should_skip(normalized)
        })
}

fn git_status_path_should_skip(path: &str) -> bool {
    should_skip(Path::new(path)) || is_binary_or_media(path) || is_generated_output_path(path)
}

fn local_change_line_should_skip(line: &str) -> bool {
    let path_part = line.get(3..).unwrap_or(line).trim();
    if path_part.is_empty() {
        return false;
    }
    let paths: Vec<&str> = path_part.split(" -> ").collect();
    !paths.is_empty()
        && paths.iter().all(|path| {
            let normalized = path.trim().trim_matches('"');
            normalized.is_empty()
                || should_skip(Path::new(normalized))
                || is_generated_output_path(normalized)
        })
}

fn is_generated_output_path(path: &str) -> bool {
    path.starts_with("output/")
        || path.starts_with("tmp/")
        || path.starts_with("video/out/")
        || path.contains("/output/")
        || path.contains("/tmp/")
}

fn build_context_text(
    root: &Path,
    files: &[FileInfo],
    manifest_files: &[FileInfo],
    truncated: bool,
    max_files: usize,
    max_manifest_chars: usize,
) -> String {
    let mut out = String::new();
    out.push_str("Project context (cached by Tales)\n");
    out.push_str(&format!("Root: {}\n", root.display()));
    out.push_str(&format!("Indexed files: {}\n", files.len()));
    if truncated {
        out.push_str(&format!(
            "Truncated: yes (max_files={max_files}; increase --max-files for a broader map)\n"
        ));
    } else {
        out.push_str("Truncated: no\n");
    }

    let manifest_paths = manifest_paths(manifest_files);
    if !manifest_paths.is_empty() {
        out.push_str("\nImportant manifests:\n");
        for rel in &manifest_paths {
            out.push_str(&format!("- {rel}\n"));
        }
    }

    out.push_str("\nFile map:\n");
    for file in files {
        out.push_str(&format!("- {} ({} bytes)\n", file.rel, file.len));
    }

    if !manifest_paths.is_empty() {
        out.push_str("\nManifest excerpts:\n");
        for rel in manifest_paths {
            let path = root.join(&rel);
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            out.push_str(&format!("--- {rel} ---\n"));
            out.push_str(&excerpt(&text, max_manifest_chars));
            out.push('\n');
        }
    }

    out
}

fn with_git_status_context(mut text: String, git_status: Option<&GitStatus>) -> String {
    let Some(status) = git_status else {
        return text;
    };
    let section = git_status_section(status);
    if let Some(idx) = text.find("\nFile map:\n") {
        text.insert_str(idx, &section);
    } else {
        text.push_str(&section);
    }
    text
}

fn git_status_section(status: &GitStatus) -> String {
    let mut out = String::new();
    out.push_str("\nGit working tree:\n");
    if let Some(branch) = &status.branch {
        out.push_str(&format!("- branch: {branch}\n"));
    }
    if status.changes.is_empty() {
        out.push_str("- changes: clean\n");
    } else {
        out.push_str("- changes:\n");
        for line in &status.changes {
            out.push_str(&format!("  - {line}\n"));
        }
        if status.truncated {
            out.push_str(&format!(
                "  - ... (status truncated after {MAX_GIT_STATUS_LINES} entries)\n"
            ));
        }
    }
    out
}

fn manifest_paths(files: &[FileInfo]) -> Vec<String> {
    files
        .iter()
        .filter(|file| is_manifest(&file.rel))
        .take(16)
        .map(|file| file.rel.clone())
        .collect()
}

fn manifest_files(root: &Path, files: &[FileInfo], max_manifest_chars: usize) -> Vec<FileInfo> {
    files
        .iter()
        .filter(|file| is_manifest(&file.rel))
        .take(16)
        .map(|file| {
            let mut file = file.clone();
            file.manifest_excerpt_hash =
                manifest_excerpt_hash(&root.join(&file.rel), max_manifest_chars);
            file
        })
        .collect()
}

fn manifest_excerpt_hash(path: &Path, max_chars: usize) -> Option<u64> {
    fs::read_to_string(path)
        .ok()
        .map(|text| stable_hash_extend(0xcbf29ce484222325, excerpt(&text, max_chars).as_bytes()))
}

fn is_manifest(rel: &str) -> bool {
    rel == "README.md"
        || rel == "Cargo.toml"
        || rel == "package.json"
        || rel.ends_with("/Cargo.toml")
        || rel.ends_with("/package.json")
        || rel.ends_with("/pyproject.toml")
        || rel.ends_with("/go.mod")
        || rel.ends_with("/deno.json")
}

fn excerpt(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max_chars).collect();
    format!("{head}\n... (excerpt truncated)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_root(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "tales-project-context-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn project_context_caches_until_file_map_changes() {
        let root = test_root("cache");
        let cache = env::temp_dir().join(format!(
            "tales-project-context-cache-store-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&cache);
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn demo() {}\n").unwrap();

        let opts = ProjectContextOptions {
            cache_dir: Some(cache.clone()),
            ..ProjectContextOptions::default()
        };
        let first = load_or_build(&root, opts.clone()).unwrap();
        assert!(!first.cache_hit);
        assert!(first.text.contains("Cargo.toml"));
        assert!(first.cache_path.starts_with(&cache));

        let second = load_or_build(&root, opts.clone()).unwrap();
        assert!(second.cache_hit);
        assert_eq!(first.text, second.text);

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(root.join("src/lib.rs"))
            .unwrap();
        writeln!(file, "pub fn changed() {{}}").unwrap();
        file.flush().unwrap();
        drop(file);
        let third = load_or_build(&root, opts).unwrap();
        assert!(!third.cache_hit);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(cache);
    }

    #[test]
    fn project_context_cache_respects_manifest_excerpt_size() {
        let root = test_root("manifest-size");
        let cache = env::temp_dir().join(format!(
            "tales-project-context-manifest-size-store-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&cache);
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        let small = load_or_build(
            &root,
            ProjectContextOptions {
                max_manifest_chars: 8,
                cache_dir: Some(cache.clone()),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();
        assert!(!small.cache_hit);
        assert!(small.text.contains("excerpt truncated"), "{}", small.text);

        let large = load_or_build(
            &root,
            ProjectContextOptions {
                max_manifest_chars: 1200,
                cache_dir: Some(cache.clone()),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();
        assert!(
            !large.cache_hit,
            "changing excerpt size must rebuild the cached context"
        );
        assert!(large.text.contains("edition = \"2021\""), "{}", large.text);

        let large_again = load_or_build(
            &root,
            ProjectContextOptions {
                max_manifest_chars: 1200,
                cache_dir: Some(cache.clone()),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();
        assert!(large_again.cache_hit);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(cache);
    }

    #[test]
    fn project_context_status_json_keeps_budget_and_legacy_aliases() {
        let ctx = ProjectContext {
            text: "abcd".repeat(300),
            cache_hit: true,
            cache_path: PathBuf::from("/tmp/tales-context.txt"),
        };

        let value = project_context_status_json(Some(&ctx), false, 180, 900, Some(1_000));

        assert_eq!(value["enabled"], true);
        assert_eq!(value["cache_hit"], true);
        assert_eq!(value["chars"], 1_200);
        assert_eq!(value["tokens_estimate"], 300);
        assert_eq!(value["max_manifest_tokens_estimate"], 225);
        assert_eq!(value["budgets"]["max_files"], 180);
        assert_eq!(value["budget_fit"]["status"], "over_budget");
        assert_eq!(value["default_budget_fit"]["status"], "over_budget");
    }

    #[test]
    fn local_change_summary_status_json_keeps_count_and_preview_aliases() {
        let changes = LocalChangeSummary {
            branch: Some("main".into()),
            changes: vec![" M src/lib.rs".into(), "?? README.md".into()],
            truncated: false,
            omitted: 1,
        };

        let value = local_change_summary_status_json(Some(&changes));

        assert_eq!(value["available"], true);
        assert_eq!(value["branch"], "main");
        assert_eq!(value["visible_count"], 2);
        assert_eq!(value["visible_changes"], 2);
        assert_eq!(value["changes"].as_array().unwrap().len(), 2);
        assert_eq!(value["preview"].as_array().unwrap().len(), 2);
        assert_eq!(value["omitted"], 1);
        assert_eq!(value["omitted_generated_or_dependency_entries"], 1);
        assert!(value["handoff_chars"].as_u64().unwrap() > 0);
    }

    #[test]
    fn signature_ignores_source_mtime_when_file_map_is_unchanged() {
        let root = PathBuf::from("/tmp/tales-project-context-root");
        let a = vec![FileInfo {
            rel: "src/lib.rs".into(),
            len: 10,
            modified_nanos: 1_700_000_000_000_000_001,
            manifest_excerpt_hash: None,
        }];
        let b = vec![FileInfo {
            rel: "src/lib.rs".into(),
            len: 10,
            modified_nanos: 1_700_000_000_000_000_999,
            manifest_excerpt_hash: None,
        }];

        assert_eq!(
            signature_for(&root, &a, &[], 220, 1200),
            signature_for(&root, &b, &[], 220, 1200)
        );
    }

    #[test]
    fn signature_changes_when_source_file_size_changes() {
        let root = PathBuf::from("/tmp/tales-project-context-root");
        let a = vec![FileInfo {
            rel: "src/lib.rs".into(),
            len: 10,
            modified_nanos: 1_700_000_000_000_000_001,
            manifest_excerpt_hash: None,
        }];
        let b = vec![FileInfo {
            rel: "src/lib.rs".into(),
            len: 11,
            modified_nanos: 1_700_000_000_000_000_001,
            manifest_excerpt_hash: None,
        }];

        assert_ne!(
            signature_for(&root, &a, &[], 220, 1200),
            signature_for(&root, &b, &[], 220, 1200)
        );
    }

    #[test]
    fn project_context_reuses_cache_for_same_size_source_edits() {
        let root = test_root("source-same-size");
        let cache = env::temp_dir().join(format!(
            "tales-project-context-source-same-size-store-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&cache);
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        let first_source = "pub fn a() {}\n";
        let second_source = "pub fn b() {}\n";
        assert_eq!(first_source.len(), second_source.len());
        fs::write(root.join("src/lib.rs"), first_source).unwrap();

        let opts = ProjectContextOptions {
            cache_dir: Some(cache.clone()),
            ..ProjectContextOptions::default()
        };
        let first = load_or_build(&root, opts.clone()).unwrap();
        assert!(!first.cache_hit);

        fs::write(root.join("src/lib.rs"), second_source).unwrap();
        let second = load_or_build(&root, opts).unwrap();
        assert!(
            second.cache_hit,
            "source edits that do not change the injected file map should reuse cache"
        );
        assert_eq!(first.text, second.text);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(cache);
    }

    #[test]
    fn signature_uses_manifest_content_hash() {
        let root = PathBuf::from("/tmp/tales-project-context-root");
        let files = vec![FileInfo {
            rel: "Cargo.toml".into(),
            len: 24,
            modified_nanos: 1_700_000_000_000_000_001,
            manifest_excerpt_hash: None,
        }];
        let a = vec![FileInfo {
            rel: "Cargo.toml".into(),
            len: 24,
            modified_nanos: 1_700_000_000_000_000_001,
            manifest_excerpt_hash: Some(111),
        }];
        let b = vec![FileInfo {
            rel: "Cargo.toml".into(),
            len: 24,
            modified_nanos: 1_700_000_000_000_000_001,
            manifest_excerpt_hash: Some(222),
        }];

        assert_ne!(
            signature_for(&root, &files, &a, 220, 1200),
            signature_for(&root, &files, &b, 220, 1200)
        );
    }

    #[test]
    fn signature_ignores_manifest_mtime_when_excerpt_hash_matches() {
        let root = PathBuf::from("/tmp/tales-project-context-root");
        let files_a = vec![FileInfo {
            rel: "Cargo.toml".into(),
            len: 24,
            modified_nanos: 1_700_000_000_000_000_001,
            manifest_excerpt_hash: None,
        }];
        let files_b = vec![FileInfo {
            rel: "Cargo.toml".into(),
            len: 24,
            modified_nanos: 1_700_000_000_000_000_999,
            manifest_excerpt_hash: None,
        }];
        let manifest = vec![FileInfo {
            rel: "Cargo.toml".into(),
            len: 24,
            modified_nanos: 1_700_000_000_000_000_001,
            manifest_excerpt_hash: Some(111),
        }];

        assert_eq!(
            signature_for(&root, &files_a, &manifest, 220, 1200),
            signature_for(&root, &files_b, &manifest, 220, 1200)
        );
    }

    #[test]
    fn project_context_reuses_cache_for_uninjected_manifest_tail_edits() {
        let root = test_root("manifest-tail");
        let cache = env::temp_dir().join(format!(
            "tales-project-context-manifest-tail-store-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&cache);
        let first_manifest = "[package]\nname = \"demo\"\n# tail aaaa\n";
        let second_manifest = "[package]\nname = \"demo\"\n# tail bbbb\n";
        assert_eq!(first_manifest.len(), second_manifest.len());
        assert_eq!(excerpt(first_manifest, 10), excerpt(second_manifest, 10));
        fs::write(root.join("Cargo.toml"), first_manifest).unwrap();

        let opts = ProjectContextOptions {
            max_manifest_chars: 10,
            cache_dir: Some(cache.clone()),
            ..ProjectContextOptions::default()
        };
        let first = load_or_build(&root, opts.clone()).unwrap();
        assert!(!first.cache_hit);

        fs::write(root.join("Cargo.toml"), second_manifest).unwrap();
        let second = load_or_build(&root, opts).unwrap();
        assert!(
            second.cache_hit,
            "edits outside the injected manifest excerpt should not rebuild"
        );
        assert_eq!(first.text, second.text);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(cache);
    }

    #[test]
    fn project_context_includes_live_git_status_without_status_cache_misses() {
        if Command::new("git")
            .arg("--version")
            .output()
            .map(|output| !output.status.success())
            .unwrap_or(true)
        {
            return;
        }

        let root = test_root("git-status");
        let cache = env::temp_dir().join(format!(
            "tales-project-context-git-status-store-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&cache);
        Command::new("git").arg("init").arg(&root).output().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".tales/reports")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("landing")).unwrap();
        fs::create_dir_all(root.join("output/playwright")).unwrap();
        fs::create_dir_all(root.join("video/out")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
        fs::write(root.join(".tales/reports/last.md"), "# report\n").unwrap();
        fs::write(root.join("target/debug/nope"), "artifact\n").unwrap();
        fs::write(root.join("landing/demo.mp4"), "media\n").unwrap();
        fs::write(root.join("output/playwright/page.png"), "screenshot\n").unwrap();
        fs::write(root.join("video/out/render.mp4"), "render\n").unwrap();

        let opts = ProjectContextOptions {
            cache_dir: Some(cache.clone()),
            ..ProjectContextOptions::default()
        };
        let first = load_or_build(&root, opts.clone()).unwrap();
        assert!(!first.cache_hit);
        assert!(first.text.contains("Git working tree:"), "{}", first.text);
        assert!(first.text.contains("- branch:"), "{}", first.text);
        assert!(first.text.contains("?? Cargo.toml"), "{}", first.text);
        assert!(first.text.contains("?? src/lib.rs"), "{}", first.text);
        assert!(
            !first.text.contains(".tales/reports/last.md"),
            "{}",
            first.text
        );
        assert!(!first.text.contains("target/debug/nope"), "{}", first.text);
        let status_section = first
            .text
            .split("\nFile map:\n")
            .next()
            .unwrap_or(&first.text);
        assert!(
            !status_section.contains("landing/demo.mp4"),
            "{}",
            first.text
        );
        assert!(
            !status_section.contains("output/playwright/page.png"),
            "{}",
            first.text
        );
        assert!(
            !status_section.contains("video/out/render.mp4"),
            "{}",
            first.text
        );

        let second = load_or_build(&root, opts.clone()).unwrap();
        assert!(second.cache_hit);

        let checkout = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["checkout", "-b", "feature/cache-status"])
            .output()
            .unwrap();
        assert!(
            checkout.status.success(),
            "git checkout failed: {}",
            String::from_utf8_lossy(&checkout.stderr)
        );
        let branch_changed = load_or_build(&root, opts.clone()).unwrap();
        assert!(
            branch_changed.cache_hit,
            "branch-only status changes should reuse static project context"
        );
        assert!(
            branch_changed.text.contains("feature/cache-status"),
            "{}",
            branch_changed.text
        );

        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        let third = load_or_build(&root, opts).unwrap();
        assert!(!third.cache_hit);
        assert!(third.text.contains("?? src/main.rs"), "{}", third.text);

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(cache);
    }

    #[test]
    fn local_change_summary_keeps_media_paths_but_skips_generated_outputs() {
        if Command::new("git")
            .arg("--version")
            .output()
            .map(|output| !output.status.success())
            .unwrap_or(true)
        {
            return;
        }

        let root = test_root("local-changes");
        Command::new("git").arg("init").arg(&root).output().unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("landing")).unwrap();
        fs::create_dir_all(root.join("video/out")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
        fs::write(root.join("landing/demo.mp4"), "media\n").unwrap();
        fs::write(root.join("video/out/render.mp4"), "render\n").unwrap();
        fs::write(root.join("target/debug/nope"), "artifact\n").unwrap();

        let summary = local_change_summary(&root, 10).unwrap();
        let text = summary.to_handoff_text();
        assert!(text.contains("?? src/lib.rs"), "{text}");
        assert!(text.contains("?? landing/demo.mp4"), "{text}");
        assert!(!text.contains("video/out/render.mp4"), "{text}");
        assert!(!text.contains("target/debug/nope"), "{text}");
        assert_eq!(summary.omitted, 2);
        assert!(!summary.truncated);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn local_change_summary_truncates_visible_entries() {
        if Command::new("git")
            .arg("--version")
            .output()
            .map(|output| !output.status.success())
            .unwrap_or(true)
        {
            return;
        }

        let root = test_root("local-changes-truncated");
        Command::new("git").arg("init").arg(&root).output().unwrap();
        fs::write(root.join("a.txt"), "a\n").unwrap();
        fs::write(root.join("b.txt"), "b\n").unwrap();

        let summary = local_change_summary(&root, 1).unwrap();
        let text = summary.to_handoff_text();
        assert_eq!(summary.changes.len(), 1);
        assert!(summary.truncated);
        assert!(
            text.contains("status truncated after 1 visible entries"),
            "{text}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_context_skips_heavy_directories() {
        let root = test_root("skip");
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("target/debug/nope"), "x").unwrap();
        fs::write(root.join("node_modules/pkg/nope.js"), "x").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        let ctx = load_or_build(
            &root,
            ProjectContextOptions {
                cache_dir: Some(env::temp_dir().join(format!(
                    "tales-project-context-skip-store-{}",
                    std::process::id()
                ))),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();
        assert!(ctx.text.contains("src/main.rs"));
        assert!(!ctx.text.contains("target/debug/nope"));
        assert!(!ctx.text.contains("node_modules/pkg/nope.js"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_context_skips_generated_session_artifacts() {
        let root = test_root("artifacts");
        fs::create_dir_all(root.join(".tales/reports")).unwrap();
        fs::create_dir_all(root.join(".playwright-cli")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".tales/reports/last.md"), "# stale run\n").unwrap();
        fs::write(root.join(".playwright-cli/page.yml"), "snapshot\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        let ctx = load_or_build(
            &root,
            ProjectContextOptions {
                cache_dir: Some(env::temp_dir().join(format!(
                    "tales-project-context-artifacts-store-{}",
                    std::process::id()
                ))),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();
        assert!(ctx.text.contains("src/main.rs"));
        assert!(!ctx.text.contains(".tales/reports/last.md"));
        assert!(!ctx.text.contains(".playwright-cli/page.yml"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_context_skips_generated_outputs_from_file_map_and_signature() {
        let root = test_root("generated-output");
        let cache = env::temp_dir().join(format!(
            "tales-project-context-generated-output-store-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&cache);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("output/playwright")).unwrap();
        fs::create_dir_all(root.join("video/out")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("output/playwright/page.png"), "screenshot\n").unwrap();
        fs::write(root.join("video/out/demo.mp4"), "render\n").unwrap();

        let opts = ProjectContextOptions {
            cache_dir: Some(cache.clone()),
            ..ProjectContextOptions::default()
        };
        let first = load_or_build(&root, opts.clone()).unwrap();
        assert!(!first.cache_hit);
        assert!(first.text.contains("src/main.rs"), "{}", first.text);
        assert!(
            !first.text.contains("output/playwright/page.png"),
            "{}",
            first.text
        );
        assert!(!first.text.contains("video/out/demo.mp4"), "{}", first.text);

        fs::write(
            root.join("output/playwright/page.png"),
            "screenshot changed\n",
        )
        .unwrap();
        fs::write(root.join("video/out/demo.mp4"), "render changed\n").unwrap();
        let second = load_or_build(&root, opts).unwrap();
        assert!(
            second.cache_hit,
            "generated output changes should not invalidate project context"
        );

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(cache);
    }

    #[test]
    fn project_context_discloses_when_file_map_is_truncated() {
        let root = test_root("truncated");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        for idx in 0..6 {
            fs::write(
                root.join("src").join(format!("file_{idx}.rs")),
                "pub fn f() {}\n",
            )
            .unwrap();
        }

        let truncated = load_or_build(
            &root,
            ProjectContextOptions {
                max_files: 3,
                cache_dir: Some(env::temp_dir().join(format!(
                    "tales-project-context-truncated-store-{}",
                    std::process::id()
                ))),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();
        assert!(truncated.text.contains("Indexed files: 3"));
        assert!(
            truncated.text.contains("Truncated: yes"),
            "{}",
            truncated.text
        );
        assert!(truncated.text.contains("max_files=3"), "{}", truncated.text);

        let complete = load_or_build(
            &root,
            ProjectContextOptions {
                max_files: 20,
                cache_dir: Some(env::temp_dir().join(format!(
                    "tales-project-context-complete-store-{}",
                    std::process::id()
                ))),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();
        assert!(complete.text.contains("Truncated: no"), "{}", complete.text);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_context_prioritizes_manifests_and_source_when_truncated() {
        let root = test_root("priority");
        fs::create_dir_all(root.join(".claude/commands")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("video/out")).unwrap();
        fs::write(root.join(".claude/commands/tales.md"), "harness notes\n").unwrap();
        fs::write(root.join(".gitignore"), "/target\n").unwrap();
        fs::write(root.join("Cargo.lock"), "large lock\n").unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(root.join("README.md"), "# Demo\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        fs::write(root.join("video/out/demo.mp4"), "not really media\n").unwrap();

        let ctx = load_or_build(
            &root,
            ProjectContextOptions {
                max_files: 4,
                cache_dir: Some(env::temp_dir().join(format!(
                    "tales-project-context-priority-store-{}",
                    std::process::id()
                ))),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();

        assert!(ctx.text.contains("Cargo.toml"), "{}", ctx.text);
        assert!(ctx.text.contains("README.md"), "{}", ctx.text);
        assert!(ctx.text.contains("src/lib.rs"), "{}", ctx.text);
        assert!(ctx.text.contains("src/main.rs"), "{}", ctx.text);
        assert!(
            !ctx.text.contains(".claude/commands/tales.md"),
            "{}",
            ctx.text
        );
        assert!(!ctx.text.contains("video/out/demo.mp4"), "{}", ctx.text);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_context_preserves_manifest_excerpts_outside_truncated_file_map() {
        let root = test_root("manifest-excerpts");
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )
        .unwrap();
        fs::write(
            root.join("app/package.json"),
            "{\"scripts\":{\"test\":\"vitest\"}}\n",
        )
        .unwrap();
        fs::write(root.join("app/main.ts"), "export const x = 1;\n").unwrap();

        let ctx = load_or_build(
            &root,
            ProjectContextOptions {
                max_files: 1,
                cache_dir: Some(env::temp_dir().join(format!(
                    "tales-project-context-manifest-excerpts-store-{}",
                    std::process::id()
                ))),
                ..ProjectContextOptions::default()
            },
        )
        .unwrap();

        assert!(ctx.text.contains("Indexed files: 1"), "{}", ctx.text);
        assert!(ctx.text.contains("- Cargo.toml"), "{}", ctx.text);
        assert!(ctx.text.contains("- app/package.json"), "{}", ctx.text);
        assert!(ctx.text.contains("--- Cargo.toml ---"), "{}", ctx.text);
        assert!(
            ctx.text.contains("--- app/package.json ---"),
            "{}",
            ctx.text
        );

        let file_map = ctx
            .text
            .split("\nManifest excerpts:\n")
            .next()
            .unwrap_or("");
        assert!(!file_map.contains("app/main.ts"), "{}", ctx.text);

        let _ = fs::remove_dir_all(root);
    }
}
