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
        }
    }

    /// Override the model name sent to the server (required for ollama).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
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
        match self.try_route(task).await {
            Ok(strategy) => (strategy, true),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

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
