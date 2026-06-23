//! A local **LLM conductor** — routing by a fine-tuned model instead of keywords.
//!
//! [`LlmConductor`] asks a model served over an OpenAI-compatible endpoint
//! (llama.cpp's `server`, ollama, …) how to coordinate a task, using the exact
//! [`CONDUCTOR_SYSTEM`] prompt the model was trained on in [`crate::dataset`] and
//! parsing the same `{"shape":"…","difficulty":0.0-1.0}` reply into a
//! [`coordinator::Strategy`]. Everything stays local — no external API.
//!
//! It **never hard-fails**: an unreachable server or an unparseable reply falls
//! back to the keyword [`coordinator`], which is also Tales' zero-cost default.
//! So `--conductor llm` is strictly a quality upgrade when the model is up and a
//! no-op when it isn't.
//!
//! Behind the `llm-conductor` cargo feature so the lean default build pulls no
//! HTTP client.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::coordinator::{self, Shape, Strategy, Tier};
use crate::dataset::CONDUCTOR_SYSTEM;
use crate::{Result, TalesError};

/// Default OpenAI-compatible endpoint — llama.cpp `server`'s default `:8080/v1`.
pub const DEFAULT_URL: &str = "http://localhost:8080/v1";

/// Request timeout. Routing should be fast; if the local server stalls we'd
/// rather fall back to the keyword coordinator than hang the run.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// Routes a task by asking a local fine-tuned conductor model, falling back to
/// the keyword [`coordinator`] on any failure.
pub struct LlmConductor {
    base_url: String,
    model: String,
    client: reqwest::Client,
    /// Memoize deterministic (model+prompt+task) → decision to `.tales/`. On by
    /// default; the routing call is temperature 0, so a repeat is free + instant.
    cache: bool,
}

impl LlmConductor {
    /// Build a conductor pointed at `base_url` (e.g. `http://localhost:8080/v1`).
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .unwrap_or_default();
        LlmConductor {
            base_url: base_url.into(),
            // ponytail: llama.cpp ignores the `model` field, so a placeholder is
            // fine there; ollama needs the real model name — set it via
            // `with_model`. Ceiling: wrong/missing name on ollama → request
            // fails → keyword fallback. Upgrade path: a `--conductor-model` flag.
            model: "tales-conductor".to_string(),
            client,
            cache: true,
        }
    }

    /// Override the model name sent to the server (required for ollama).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Enable/disable the local decision cache (default: enabled). Disable for
    /// benchmarking the raw model or when you want every call to hit the server.
    pub fn with_cache(mut self, enabled: bool) -> Self {
        self.cache = enabled;
        self
    }

    /// Route `task` to a [`Strategy`]. Never fails: on an unreachable server or
    /// an unparseable reply it logs a warning and returns the keyword
    /// coordinator's call for `workspace`.
    pub async fn advise(&self, task: &str, workspace: &Path) -> Strategy {
        self.advise_traced(task, workspace).await.0
    }

    /// Like [`Self::advise`] but also reports the provenance: `true` if the model
    /// answered, `false` if the keyword coordinator fallback did. Lets a UI label
    /// the routing honestly instead of claiming the LLM spoke when it didn't.
    pub async fn advise_traced(&self, task: &str, workspace: &Path) -> (Strategy, bool) {
        // Cache hit: the model already routed this exact (model, prompt, task) —
        // return its memoized call without touching the server. Counts as the
        // model answering (`true`), because it did, once.
        if self.cache {
            if let Some((shape, difficulty)) = cache::lookup(workspace, &self.model, task) {
                if let Some(shape) = shape_from_str(&shape) {
                    return (strategy_from(shape, difficulty, &self.base_url), true);
                }
            }
        }
        match self.try_route(task).await {
            Ok(strategy) => {
                if self.cache {
                    // Best-effort: a cache write must never break routing.
                    cache::store(workspace, &self.model, task, &strategy);
                }
                (strategy, true)
            }
            Err(err) => {
                tracing::warn!(
                    "llm conductor unavailable ({err}); falling back to keyword coordinator"
                );
                (coordinator::advise(task, workspace), false)
            }
        }
    }

    /// The fallible inner route: POST the task, parse the decision, build a
    /// [`Strategy`]. Any error here triggers the keyword fallback in [`Self::advise`].
    async fn try_route(&self, task: &str) -> Result<Strategy> {
        let endpoint = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": CONDUCTOR_SYSTEM},
                {"role": "user", "content": task},
            ],
            "temperature": 0.0,
            "stream": false,
        });
        let response = self
            .client
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| TalesError::Other(format!("conductor request failed: {e}")))?;
        if !response.status().is_success() {
            return Err(TalesError::Other(format!(
                "conductor returned HTTP {}",
                response.status()
            )));
        }
        let text = response
            .text()
            .await
            .map_err(|e| TalesError::Other(format!("conductor body read failed: {e}")))?;
        let value: Value = serde_json::from_str(&text)?;
        let content = value["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| TalesError::Other("conductor reply missing message content".into()))?;
        let decision = parse_decision(content).ok_or_else(|| {
            TalesError::Other(format!("conductor reply was not a decision: {content}"))
        })?;
        let shape = shape_from_str(&decision.shape).ok_or_else(|| {
            TalesError::Other(format!(
                "conductor returned unknown shape '{}'",
                decision.shape
            ))
        })?;
        Ok(strategy_from(shape, decision.difficulty, &self.base_url))
    }
}

/// The model's routing reply — shape + difficulty, the [`CONDUCTOR_SYSTEM`] schema.
#[derive(Deserialize)]
struct Decision {
    shape: String,
    // ponytail: defaults to 0.0 (→ Cheap tier) if the model omits it; the trained
    // conductor always emits it. Upgrade: derive from shape when absent.
    #[serde(default)]
    difficulty: f32,
}

/// Parse a decision from raw model output, tolerating prose or ```json fences by
/// falling back to the first `{ … }` slice.
fn parse_decision(content: &str) -> Option<Decision> {
    if let Ok(d) = serde_json::from_str::<Decision>(content.trim()) {
        return Some(d);
    }
    // ponytail: greedy first-`{` .. last-`}` — fine for a fenced or prose-wrapped
    // object; ceiling: several JSON objects in one reply. Upgrade: brace-balance scan.
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end > start {
        serde_json::from_str::<Decision>(&content[start..=end]).ok()
    } else {
        None
    }
}

fn shape_from_str(s: &str) -> Option<Shape> {
    match s.trim().to_ascii_lowercase().as_str() {
        "solo" => Some(Shape::Solo),
        "debate" => Some(Shape::Debate),
        "tiered" => Some(Shape::Tiered),
        _ => None,
    }
}

/// Build a full [`Strategy`] from the model's (shape, difficulty). The model
/// makes a single definite call, so the chosen shape gets all the probability
/// mass; the tier comes from the shared [`Tier::from_difficulty`] mapping so the
/// LLM and keyword paths agree on tier policy.
fn strategy_from(shape: Shape, difficulty: f32, url: &str) -> Strategy {
    let difficulty = difficulty.clamp(0.0, 1.0);
    let mut shape_probs = [0.0f32; 3];
    shape_probs[shape.index()] = 1.0;
    Strategy {
        shape,
        shape_probs,
        difficulty,
        tier: Tier::from_difficulty(difficulty),
        confidence: 1.0,
        margin: 1.0,
        uncertain: false,
        rationale: format!("llm conductor @ {url}"),
    }
}

/// A tiny, local, best-effort decision cache. The conductor routes at
/// temperature 0, so `(model, system prompt, task)` maps to one decision
/// forever — memoizing it makes a repeat instant and free (no server call, no
/// tokens), which is the local analogue of provider prompt caching. The system
/// prompt is folded into the key, so editing [`CONDUCTOR_SYSTEM`] or swapping
/// the model silently invalidates stale entries. Persisted to
/// `<workspace>/.tales/conductor-cache.json`.
mod cache {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Serialize};

    use super::CONDUCTOR_SYSTEM;
    use crate::coordinator::Strategy;

    const SCHEMA_VERSION: u32 = 1;

    #[derive(Serialize, Deserialize)]
    struct CacheFile {
        schema_version: u32,
        entries: HashMap<String, Entry>,
    }

    #[derive(Serialize, Deserialize)]
    struct Entry {
        model: String,
        task: String,
        shape: String,
        difficulty: f32,
    }

    fn path(workspace: &Path) -> PathBuf {
        workspace.join(".tales").join("conductor-cache.json")
    }

    /// FNV-1a 64-bit over `model ⏿ prompt ⏿ task`. A *stable* hash on purpose —
    /// `DefaultHasher` isn't guaranteed stable across builds, so it can't key a
    /// persisted cache.
    fn key(model: &str, task: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in model
            .bytes()
            .chain([0x1f_u8])
            .chain(CONDUCTOR_SYSTEM.bytes())
            .chain([0x1f_u8])
            .chain(task.bytes())
        {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        format!("{h:016x}")
    }

    fn load(workspace: &Path) -> CacheFile {
        std::fs::read_to_string(path(workspace))
            .ok()
            .and_then(|s| serde_json::from_str::<CacheFile>(&s).ok())
            .filter(|c: &CacheFile| c.schema_version == SCHEMA_VERSION)
            .unwrap_or(CacheFile {
                schema_version: SCHEMA_VERSION,
                entries: HashMap::new(),
            })
    }

    /// The cached `(shape, difficulty)` for this exact request, if any. The
    /// stored `model`/`task` are re-checked so a hash collision can never serve a
    /// wrong decision.
    pub(super) fn lookup(workspace: &Path, model: &str, task: &str) -> Option<(String, f32)> {
        let file = load(workspace);
        let e = file.entries.get(&key(model, task))?;
        (e.model == model && e.task == task).then(|| (e.shape.clone(), e.difficulty))
    }

    /// Persist a decision. Best-effort: any IO/serialize error is swallowed so a
    /// cache problem never breaks routing.
    pub(super) fn store(workspace: &Path, model: &str, task: &str, strategy: &Strategy) {
        let mut file = load(workspace);
        file.entries.insert(
            key(model, task),
            Entry {
                model: model.to_string(),
                task: task.to_string(),
                shape: strategy.shape.as_str().to_string(),
                difficulty: strategy.difficulty,
            },
        );
        let p = path(workspace);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&file) {
            let _ = std::fs::write(p, json);
        }
    }
    // ponytail: unbounded growth, no eviction, last-writer-wins across concurrent
    // processes. Plans are tiny, so size is a non-issue for a local single-user
    // tool; the upgrade path is an LRU/size cap + a file lock if that changes.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn decision_cache_round_trips_and_is_keyed() {
        let ws = std::env::temp_dir().join("tales_cond_cache_test");
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        let task = "migrate all the tests from mocha to jest";
        let strat = strategy_from(Shape::Tiered, 0.2, "test");

        // Miss before anything is stored.
        assert!(cache::lookup(&ws, "talesml-v4", task).is_none());

        cache::store(&ws, "talesml-v4", task, &strat);

        // Hit on the exact (model, task).
        let (shape, diff) = cache::lookup(&ws, "talesml-v4", task).expect("cache hit");
        assert_eq!(shape, "tiered");
        assert!((diff - 0.2).abs() < 1e-6);

        // Miss on a different task, and on a different model (key includes both).
        assert!(cache::lookup(&ws, "talesml-v4", "implement a trie").is_none());
        assert!(cache::lookup(&ws, "other-model", task).is_none());

        // Persisted locally under .tales/.
        assert!(ws.join(".tales").join("conductor-cache.json").exists());
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// One-shot mock OpenAI-compatible server: answers a single connection with
    /// `decision` wrapped as the assistant message content, then closes. Runs on
    /// a std thread so the test needs no extra tokio features.
    fn spawn_mock_server(decision: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf); // best-effort drain of the small request
                let body = json!({
                    "choices": [{"message": {"role": "assistant", "content": decision}}]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn parses_canned_decision_from_local_server() {
        let url = spawn_mock_server("{\"shape\":\"tiered\",\"difficulty\":0.2}");
        let strategy = LlmConductor::new(url)
            .advise("anything at all", &std::env::temp_dir())
            .await;
        assert_eq!(strategy.shape, Shape::Tiered);
        assert!((strategy.difficulty - 0.2).abs() < 1e-3);
        assert_eq!(strategy.tier, Tier::Cheap);
        assert_eq!(strategy.shape_probs[Shape::Tiered.index()], 1.0);
    }

    #[tokio::test]
    async fn tolerates_fenced_json_reply() {
        // Real models often wrap JSON in a ```json fence or add a word of prose.
        let url = spawn_mock_server(
            "Here you go:\n```json\n{\"shape\":\"solo\",\"difficulty\":0.9}\n```",
        );
        let strategy = LlmConductor::new(url)
            .advise("implement a regex engine", &std::env::temp_dir())
            .await;
        assert_eq!(strategy.shape, Shape::Solo);
        assert_eq!(strategy.tier, Tier::Strong);
    }

    #[tokio::test]
    async fn falls_back_to_keyword_when_server_down() {
        // Nothing is listening on port 1 → connection refused → keyword fallback,
        // never an error. The keyword coordinator routes this algorithmic task to
        // Solo (same phrasing the coordinator's own tests assert on).
        let strategy = LlmConductor::new("http://127.0.0.1:1/v1")
            .advise(
                "implement a balanced binary search tree with rebalancing",
                &std::env::temp_dir(),
            )
            .await;
        assert_eq!(strategy.shape, Shape::Solo);
    }
}
