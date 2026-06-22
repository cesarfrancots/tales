//! The Tales orchestration **coordinator** — a tiny, dependency-free model that
//! decides *how* to coordinate a task, the way Fugu routes across strategies.
//!
//! Given a task description the coordinator predicts the best collaboration
//! **shape** — [`Shape::Solo`] (one strong model plans + executes),
//! [`Shape::Debate`] (drafter/critic argue, then execute), or [`Shape::Tiered`]
//! (strong models plan, a cheap model executes) — plus a **difficulty** estimate
//! that drives cheap-first escalation (Fugu's "everyday vs Ultra" as a routing
//! decision).
//!
//! It is a small multilayer perceptron (features → hidden → 3 shapes) trained by
//! deterministic gradient descent on an embedded **seed corpus** that encodes the
//! structural priors from Tales' own benchmark — *algorithmically hard,
//! self-contained* work wants Solo; *voluminous boilerplate* wants Tiered;
//! *ambiguous, architecture-defining* work wants Debate. The model sharpens over
//! time by retraining on real [`crate::session`] run outcomes via
//! [`Coordinator::trained_from`].
//!
//! Inference and training are pure `std`: no ML runtime, a few-KB JSON artifact,
//! trivial to deploy. The whole thing exists so a learned policy — not an
//! `if/else` — chooses the orchestration strategy, while the human still holds
//! the execution gate.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Result, TalesError};

/// Current on-disk schema for a serialized coordinator. Bump on a breaking change
/// to the [`Mlp`] shape or feature layout so stale artifacts are rejected.
pub const SCHEMA_VERSION: u8 = 1;

/// Number of input features. Keep in sync with [`FEATURE_NAMES`] and
/// [`extract_features`].
pub const FEATURE_DIM: usize = 10;

/// Default hidden width. Small on purpose — the decision boundary between three
/// well-separated shapes does not need capacity, and a tiny net keeps the
/// artifact a few KB and training instant.
pub const HIDDEN_DIM: usize = 8;

/// Human-readable feature names, index-aligned with [`extract_features`].
pub const FEATURE_NAMES: [&str; FEATURE_DIM] = [
    "length",
    "word_count",
    "requirement_count",
    "algorithmic",
    "volume",
    "ambiguity",
    "correctness",
    "breadth",
    "verifiability",
    "bias",
];

/// The collaboration shape the coordinator routes a task to. Mirrors the eval
/// harness's `EvalMode`, but carries routing semantics (a decision) rather than
/// a cost-forecast mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// One strong model plans and executes. Best for algorithmically hard,
    /// self-contained work where correctness *is* the implementation.
    Solo,
    /// Drafter and critic argue the approach, then execute. Best for ambiguous,
    /// architecture-defining work with no known-correct answer.
    Debate,
    /// Strong models plan; a cheap, fast model executes the agreed plan. Best for
    /// mechanically voluminous work that amortizes the planning cost.
    Tiered,
}

impl Shape {
    /// All shapes in stable index order (matches model output neurons).
    pub const ALL: [Shape; 3] = [Shape::Solo, Shape::Debate, Shape::Tiered];

    /// Output-neuron index for this shape.
    pub fn index(self) -> usize {
        match self {
            Shape::Solo => 0,
            Shape::Debate => 1,
            Shape::Tiered => 2,
        }
    }

    /// Inverse of [`Shape::index`]. `None` for out-of-range indices.
    pub fn from_index(i: usize) -> Option<Shape> {
        Shape::ALL.get(i).copied()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Shape::Solo => "solo",
            Shape::Debate => "debate",
            Shape::Tiered => "tiered",
        }
    }

    /// One-line description of why/when this shape wins.
    pub fn blurb(self) -> &'static str {
        match self {
            Shape::Solo => {
                "one strong model plans + executes (algorithmically hard, self-contained)"
            }
            Shape::Debate => {
                "drafter/critic argue, then execute (ambiguous, architecture-defining)"
            }
            Shape::Tiered => "strong models plan, a cheap model executes (voluminous, mechanical)",
        }
    }
}

/// The cost/capability tier the coordinator advises the executor start at. Pairs
/// with [`Strategy::difficulty`] to drive cheap-first escalation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A cheap, fast executor is enough; escalate only on verifier failure.
    Cheap,
    /// Mid capability; escalate readily if the first attempt stumbles.
    Balanced,
    /// Start with a strong model — correctness risk is high.
    Strong,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Cheap => "cheap",
            Tier::Balanced => "balanced",
            Tier::Strong => "strong",
        }
    }
}

/// A fixed-width feature vector extracted from a task description.
pub type Features = [f32; FEATURE_DIM];

// --- feature extraction ----------------------------------------------------

/// Keywords that signal *algorithmic, self-contained* difficulty (favor Solo).
const ALGORITHMIC: &[&str] = &[
    "algorithm",
    "parser",
    "regex",
    "regular expression",
    "solver",
    "solve",
    "optimi",
    "complexity",
    "recursi",
    "data structure",
    "tree",
    "graph",
    "heap",
    "sort",
    "search",
    "dynamic programming",
    "np-",
    "compiler",
    "interpreter",
    "tokeniz",
    "encode",
    "decode",
    "hash",
    "cipher",
    "matrix",
    "numeric",
];

/// Keywords that signal *mechanical volume* (favor Tiered cheap execution).
const VOLUME: &[&str] = &[
    "crud",
    "endpoint",
    "boilerplate",
    "scaffold",
    "migrate",
    "migration",
    "rename",
    "across the",
    "all files",
    "every file",
    "repetitive",
    "bulk",
    "wire up",
    "stub",
    "generate the",
    "for each",
    "throughout",
    "port the",
    "convert all",
    "add tests for",
];

/// Keywords that signal *ambiguity / architecture* (favor Debate).
const AMBIGUITY: &[&str] = &[
    "design",
    "architect",
    "approach",
    "strategy",
    "tradeoff",
    "trade-off",
    "decide",
    "should we",
    "which",
    "vs",
    "versus",
    "propose",
    "evaluate",
    "options",
    "best way",
    "refactor",
    "rethink",
    "plan the",
    "structure the",
];

/// Keywords that raise *correctness criticality* (raise difficulty).
const CORRECTNESS: &[&str] = &[
    "correct",
    "exact",
    "edge case",
    "edge-case",
    "must not",
    "invariant",
    "race",
    "concurren",
    "deadlock",
    "security",
    "auth",
    "crypto",
    "payment",
    "money",
    "precise",
    "spec",
    "guarantee",
    "atomic",
];

/// Keywords that signal *breadth* across the codebase (raise volume + breadth).
const BREADTH: &[&str] = &[
    "across",
    "every",
    "all ",
    "entire",
    "whole",
    "codebase",
    "project-wide",
    "multiple files",
    "everywhere",
    "globally",
];

/// Keywords that signal *verifiability* (tests/oracle exist → safer to tier).
const VERIFIABILITY: &[&str] = &[
    "test",
    "tests",
    "oracle",
    "assert",
    "verify",
    "ci",
    "lint",
    "type check",
    "compile",
    "passes",
];

fn keyword_score(haystack: &str, needles: &[&str], cap: f32) -> f32 {
    let hits = needles.iter().filter(|n| haystack.contains(**n)).count() as f32;
    (hits / cap).min(1.0)
}

/// Count distinct requirement-ish clauses (commas, conjunctions, list markers).
fn requirement_count(task: &str) -> f32 {
    let lower = task.to_ascii_lowercase();
    let commas = lower.matches(',').count();
    let ands = lower.matches(" and ").count();
    let bullets = lower.matches('\n').count() + lower.matches(" then ").count();
    let total = (commas + ands + bullets) as f32;
    (total / 6.0).min(1.0)
}

/// Extract the index-aligned [`Features`] for a task. Pure and deterministic so
/// the same task always trains/predicts identically.
pub fn extract_features(task: &str) -> Features {
    let lower = task.to_ascii_lowercase();
    let chars = task.chars().count() as f32;
    let words = task.split_whitespace().count() as f32;

    // Length features on a log scale so a 2000-char task isn't 40x a 50-char one.
    let length = (chars.max(1.0).ln() / 2000f32.ln()).clamp(0.0, 1.0);
    let word_count = (words.max(1.0).ln() / 400f32.ln()).clamp(0.0, 1.0);

    [
        length,
        word_count,
        requirement_count(task),
        keyword_score(&lower, ALGORITHMIC, 3.0),
        keyword_score(&lower, VOLUME, 3.0),
        keyword_score(&lower, AMBIGUITY, 3.0),
        keyword_score(&lower, CORRECTNESS, 3.0),
        keyword_score(&lower, BREADTH, 2.0),
        keyword_score(&lower, VERIFIABILITY, 2.0),
        1.0, // bias
    ]
}

// --- the model -------------------------------------------------------------

/// A minimal two-layer perceptron with a ReLU hidden layer and a softmax head.
/// Weights are plain nested vectors so the serialized artifact is human-readable
/// JSON and forward-compatible to inspect.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mlp {
    pub input_dim: usize,
    pub hidden_dim: usize,
    pub output_dim: usize,
    /// `[hidden_dim][input_dim]`
    w1: Vec<Vec<f32>>,
    /// `[hidden_dim]`
    b1: Vec<f32>,
    /// `[output_dim][hidden_dim]`
    w2: Vec<Vec<f32>>,
    /// `[output_dim]`
    b2: Vec<f32>,
}

/// Deterministic splitmix64 — a tiny PRNG so weight init (and therefore the whole
/// model) is reproducible without pulling in the `rand` crate.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Symmetric small init in roughly `[-0.5, 0.5)`.
    fn next_init(&mut self) -> f32 {
        let unit = (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32; // [0,1)
        unit - 0.5
    }
}

fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|l| (l - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

/// Dot product of two equal-length slices.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl Mlp {
    fn new_seeded(input_dim: usize, hidden_dim: usize, output_dim: usize, seed: u64) -> Mlp {
        let mut rng = SplitMix64(seed);
        let w1 = (0..hidden_dim)
            .map(|_| (0..input_dim).map(|_| rng.next_init()).collect())
            .collect();
        let b1 = vec![0.0; hidden_dim];
        let w2 = (0..output_dim)
            .map(|_| (0..hidden_dim).map(|_| rng.next_init()).collect())
            .collect();
        let b2 = vec![0.0; output_dim];
        Mlp {
            input_dim,
            hidden_dim,
            output_dim,
            w1,
            b1,
            w2,
            b2,
        }
    }

    /// Forward pass. Returns `(hidden_activations, class_probabilities)`.
    fn forward(&self, x: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let a1: Vec<f32> = self
            .w1
            .iter()
            .zip(&self.b1)
            .map(|(row, b)| (b + dot(row, x)).max(0.0)) // ReLU
            .collect();
        let z2: Vec<f32> = self
            .w2
            .iter()
            .zip(&self.b2)
            .map(|(row, b)| b + dot(row, &a1))
            .collect();
        (a1, softmax(&z2))
    }

    /// Class probabilities for an input.
    pub fn predict_probs(&self, x: &[f32]) -> Vec<f32> {
        self.forward(x).1
    }
}

/// Hyperparameters for [`train`]. Defaults fit the seed corpus in well under a
/// millisecond and generalize to held-out task phrasings.
#[derive(Clone, Copy, Debug)]
pub struct TrainConfig {
    pub epochs: usize,
    pub lr: f32,
    pub l2: f32,
    pub hidden_dim: usize,
    pub seed: u64,
}

impl Default for TrainConfig {
    fn default() -> Self {
        TrainConfig {
            epochs: 600,
            lr: 0.2,
            l2: 1e-4,
            hidden_dim: HIDDEN_DIM,
            seed: 0x7A1E_5C0D,
        }
    }
}

/// Full-batch gradient descent with cross-entropy loss and L2 regularization.
/// Deterministic given `cfg.seed`, so the seed model is reproducible across
/// machines (no checked-in binary blob required).
pub fn train(samples: &[(Features, Shape)], cfg: TrainConfig) -> Mlp {
    let mut net = Mlp::new_seeded(FEATURE_DIM, cfg.hidden_dim, Shape::ALL.len(), cfg.seed);
    if samples.is_empty() {
        return net;
    }
    let n = samples.len() as f32;

    for _ in 0..cfg.epochs {
        // Gradient accumulators.
        let mut gw1 = vec![vec![0.0f32; net.input_dim]; net.hidden_dim];
        let mut gb1 = vec![0.0f32; net.hidden_dim];
        let mut gw2 = vec![vec![0.0f32; net.hidden_dim]; net.output_dim];
        let mut gb2 = vec![0.0f32; net.output_dim];

        for (x, shape) in samples {
            let (a1, probs) = net.forward(x);
            let target = shape.index();

            // Output-layer error: softmax + cross-entropy → (p - y).
            let dz2: Vec<f32> = probs
                .iter()
                .enumerate()
                .map(|(o, p)| p - if o == target { 1.0 } else { 0.0 })
                .collect();
            for ((grow, gb), dz) in gw2.iter_mut().zip(gb2.iter_mut()).zip(&dz2) {
                for (g, a) in grow.iter_mut().zip(&a1) {
                    *g += dz * a;
                }
                *gb += dz;
            }

            // Backprop into the hidden layer through the ReLU.
            for (h, ((grow, gb), a)) in gw1.iter_mut().zip(gb1.iter_mut()).zip(&a1).enumerate() {
                if *a <= 0.0 {
                    continue; // ReLU'(z) = 0
                }
                // da1 = Σ_o w2[o][h] · dz2[o]; ReLU'(z) = 1 where a1 > 0.
                let dz1: f32 = net.w2.iter().zip(&dz2).map(|(row, dz)| row[h] * dz).sum();
                for (g, xi) in grow.iter_mut().zip(x.iter()) {
                    *g += dz1 * xi;
                }
                *gb += dz1;
            }
        }

        // Apply averaged gradients + L2 decay.
        apply_grads(&mut net.w1, &mut net.b1, &gw1, &gb1, cfg.lr, cfg.l2, n);
        apply_grads(&mut net.w2, &mut net.b2, &gw2, &gb2, cfg.lr, cfg.l2, n);
    }
    net
}

/// Apply one averaged-gradient + L2-decay step to a weight matrix and its biases.
#[allow(clippy::too_many_arguments)]
fn apply_grads(
    weights: &mut [Vec<f32>],
    biases: &mut [f32],
    gw: &[Vec<f32>],
    gb: &[f32],
    lr: f32,
    l2: f32,
    n: f32,
) {
    for ((wrow, b), (grow, g)) in weights
        .iter_mut()
        .zip(biases.iter_mut())
        .zip(gw.iter().zip(gb))
    {
        for (w, gv) in wrow.iter_mut().zip(grow) {
            *w -= lr * (gv / n + l2 * *w);
        }
        *b -= lr * (g / n);
    }
}

// --- the coordinator -------------------------------------------------------

/// A trained coordinator plus provenance metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Coordinator {
    pub schema_version: u8,
    /// How many examples (seed + traces) the model was trained on.
    pub sample_count: usize,
    /// How many of those came from real run traces (vs the embedded seed).
    pub trace_count: usize,
    model: Mlp,
}

/// The coordinator's routing decision for a task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Strategy {
    pub shape: Shape,
    /// Probability per shape, index-aligned with [`Shape::ALL`].
    pub shape_probs: [f32; 3],
    /// `[0,1]` — higher means more correctness risk / less safe to run cheap.
    pub difficulty: f32,
    pub tier: Tier,
    /// Confidence in the top shape (its probability).
    pub confidence: f32,
    /// Human-readable explanation naming the dominant signals.
    pub rationale: String,
}

impl Strategy {
    /// A compact one-liner for an advisory chip in the pipeline UI.
    pub fn summary_line(&self) -> String {
        format!(
            "coordinator: {} ({:.0}% conf) · difficulty {:.2} · start {} — {}",
            self.shape.as_str(),
            self.confidence * 100.0,
            self.difficulty,
            self.tier.as_str(),
            self.shape.blurb(),
        )
    }
}

impl Coordinator {
    /// Train a coordinator from the embedded seed corpus alone — the default
    /// model, reproducible on any machine.
    pub fn seed() -> Coordinator {
        Coordinator::trained_from(&[])
    }

    /// Train from the seed corpus plus extra `(task, shape)` examples harvested
    /// from real run traces. Later examples don't overwrite the priors; they're
    /// concatenated so the seed keeps the model sane during cold-start.
    pub fn trained_from(extra: &[(String, Shape)]) -> Coordinator {
        let mut samples: Vec<(Features, Shape)> = seed_corpus()
            .iter()
            .map(|(task, shape)| (extract_features(task), *shape))
            .collect();
        let seed_len = samples.len();
        samples.extend(
            extra
                .iter()
                .map(|(task, shape)| (extract_features(task), *shape)),
        );
        let model = train(&samples, TrainConfig::default());
        Coordinator {
            schema_version: SCHEMA_VERSION,
            sample_count: samples.len(),
            trace_count: samples.len().saturating_sub(seed_len),
            model,
        }
    }

    /// Predict the routing strategy for a task.
    pub fn predict(&self, task: &str) -> Strategy {
        let features = extract_features(task);
        let probs = self.model.predict_probs(&features);
        let (best_idx, &confidence) = probs
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or((0, &0.0));
        let shape = Shape::from_index(best_idx).unwrap_or(Shape::Solo);
        let shape_probs = [probs[0], probs[1], probs[2]];

        // Difficulty: Solo work is hardest/least-safe-to-cheap, Tiered is safest.
        // Correctness keyword signal nudges it up regardless of shape.
        let correctness = features[6];
        let difficulty = (shape_probs[0] * 1.0
            + shape_probs[1] * 0.6
            + shape_probs[2] * 0.15
            + correctness * 0.2)
            .clamp(0.0, 1.0);
        let tier = if difficulty >= 0.6 {
            Tier::Strong
        } else if difficulty >= 0.35 {
            Tier::Balanced
        } else {
            Tier::Cheap
        };

        Strategy {
            shape,
            shape_probs,
            difficulty,
            tier,
            confidence,
            rationale: rationale(&features, shape),
        }
    }

    /// Persist to JSON. The artifact is a few KB and safe to commit or cache.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TalesError::Other(e.to_string()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json).map_err(|e| TalesError::Other(e.to_string()))
    }

    /// Load from JSON, rejecting artifacts from an incompatible schema.
    pub fn load(path: &Path) -> Result<Coordinator> {
        let text = std::fs::read_to_string(path).map_err(|e| TalesError::Other(e.to_string()))?;
        let coord: Coordinator = serde_json::from_str(&text)?;
        if coord.schema_version != SCHEMA_VERSION {
            return Err(TalesError::Other(format!(
                "coordinator schema {} != supported {} — retrain with `tales coordinator train`",
                coord.schema_version, SCHEMA_VERSION
            )));
        }
        Ok(coord)
    }

    /// Load the cached model if present and compatible, otherwise train the seed
    /// model and cache it. The zero-config path that makes the model "just work".
    pub fn load_or_seed(path: &Path) -> Coordinator {
        if let Ok(coord) = Coordinator::load(path) {
            return coord;
        }
        let coord = Coordinator::seed();
        let _ = coord.save(path); // caching is best-effort; predicting still works
        coord
    }
}

/// Default on-disk location for a workspace's cached coordinator.
pub fn default_model_path(workspace: &Path) -> PathBuf {
    workspace.join(".tales").join("coordinator.json")
}

/// Convenience: load-or-seed the workspace model and predict in one call. The
/// entry point pipeline code uses to get routing advice.
pub fn advise(task: &str, workspace: &Path) -> Strategy {
    Coordinator::load_or_seed(&default_model_path(workspace)).predict(task)
}

/// Name the one or two dominant signal features behind a routing call, for a
/// human-readable rationale.
fn rationale(features: &Features, shape: Shape) -> String {
    // Indices of the "signal" features worth naming (skip length/word/bias).
    const SIGNALS: [(usize, &str); 6] = [
        (3, "algorithmic difficulty"),
        (4, "mechanical volume"),
        (5, "ambiguity/architecture"),
        (6, "correctness criticality"),
        (7, "codebase breadth"),
        (2, "many requirements"),
    ];
    let mut scored: Vec<(f32, &str)> = SIGNALS
        .iter()
        .map(|(i, name)| (features[*i], *name))
        .filter(|(v, _)| *v > 0.15)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    if scored.is_empty() {
        return format!(
            "{} by default — no strong task signals detected",
            shape.as_str()
        );
    }
    let drivers: Vec<&str> = scored.iter().take(2).map(|(_, n)| *n).collect();
    format!("{} — driven by {}", shape.as_str(), drivers.join(" + "))
}

/// The embedded seed corpus: labeled example tasks encoding the structural priors
/// from Tales' benchmark. Kept legible so the priors are auditable and editable.
fn seed_corpus() -> Vec<(&'static str, Shape)> {
    vec![
        // --- Solo: algorithmically hard, self-contained ---
        (
            "implement a regex engine supporting groups, alternation, and character classes",
            Shape::Solo,
        ),
        (
            "write a balanced AVL tree with insert, delete, and rebalancing",
            Shape::Solo,
        ),
        (
            "implement Dijkstra shortest path with a binary heap",
            Shape::Solo,
        ),
        (
            "solve the 0/1 knapsack with dynamic programming and reconstruct the chosen items",
            Shape::Solo,
        ),
        (
            "write a recursive descent parser for arithmetic expressions with precedence",
            Shape::Solo,
        ),
        ("implement a LRU cache with O(1) get and put", Shape::Solo),
        ("build a topological sort that detects cycles", Shape::Solo),
        (
            "implement run-length encoding and decoding with exact round-trip correctness",
            Shape::Solo,
        ),
        (
            "write a function to compute edit distance between two strings",
            Shape::Solo,
        ),
        (
            "implement a thread-safe lock-free queue avoiding the ABA race",
            Shape::Solo,
        ),
        (
            "write a JSON tokenizer handling escapes and nested structures",
            Shape::Solo,
        ),
        (
            "implement a red-black tree deletion with correct fixups",
            Shape::Solo,
        ),
        // --- Tiered: mechanically voluminous, cheap executor amortizes ---
        (
            "add CRUD endpoints for the users, posts, and comments resources",
            Shape::Tiered,
        ),
        (
            "migrate all React components from class syntax to function components",
            Shape::Tiered,
        ),
        (
            "rename getUser to fetchUser across the entire codebase",
            Shape::Tiered,
        ),
        (
            "scaffold REST handlers and route stubs for twelve resources",
            Shape::Tiered,
        ),
        (
            "add unit tests for each utility function in the helpers module",
            Shape::Tiered,
        ),
        (
            "wire up boilerplate serializers for every model in the schema",
            Shape::Tiered,
        ),
        (
            "convert all var declarations to const or let throughout the project",
            Shape::Tiered,
        ),
        (
            "generate the TypeScript types for each API endpoint response",
            Shape::Tiered,
        ),
        (
            "port the config files from JSON to YAML across all services",
            Shape::Tiered,
        ),
        (
            "add logging statements to every public handler in the controllers",
            Shape::Tiered,
        ),
        // --- Debate: ambiguous, architecture-defining ---
        (
            "design the authentication architecture for a multi-tenant SSO system",
            Shape::Debate,
        ),
        (
            "should we use event sourcing or a CRUD model for the orders service",
            Shape::Debate,
        ),
        (
            "propose an approach to refactor the payments module for testability",
            Shape::Debate,
        ),
        (
            "decide the caching strategy and invalidation approach for the public API",
            Shape::Debate,
        ),
        (
            "evaluate the tradeoffs between REST and GraphQL for our gateway",
            Shape::Debate,
        ),
        (
            "plan the migration strategy from the monolith to services",
            Shape::Debate,
        ),
        (
            "rethink the data model to support soft deletes and audit history",
            Shape::Debate,
        ),
        (
            "choose a state management approach for the growing frontend",
            Shape::Debate,
        ),
        (
            "design the rate-limiting and abuse-prevention strategy for signups",
            Shape::Debate,
        ),
        (
            "propose the best way to structure the plugin system for extensibility",
            Shape::Debate,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn predict(task: &str) -> Strategy {
        Coordinator::seed().predict(task)
    }

    #[test]
    fn features_are_in_range_and_aligned() {
        let f = extract_features("design the caching strategy across the whole codebase");
        assert_eq!(f.len(), FEATURE_DIM);
        assert_eq!(FEATURE_NAMES.len(), FEATURE_DIM);
        for v in f {
            assert!((0.0..=1.0).contains(&v), "feature out of range: {v}");
        }
        assert_eq!(f[FEATURE_DIM - 1], 1.0, "bias must be 1.0");
        // "across" + "whole" + "codebase" should light up breadth.
        assert!(f[7] > 0.0, "breadth feature should fire");
    }

    #[test]
    fn seed_model_learns_the_priors() {
        // Held-out phrasings the model was NOT trained on verbatim.
        let solo = predict("implement a balanced binary search tree with node deletion");
        assert_eq!(solo.shape, Shape::Solo, "{}", solo.summary_line());

        let tiered = predict("add boilerplate CRUD endpoints for every resource in the schema");
        assert_eq!(tiered.shape, Shape::Tiered, "{}", tiered.summary_line());

        let debate = predict("design the overall architecture and decide the approach for billing");
        assert_eq!(debate.shape, Shape::Debate, "{}", debate.summary_line());
    }

    #[test]
    fn difficulty_and_tier_track_shape() {
        let hard =
            predict("implement a lock-free concurrent hash map with correct memory ordering");
        assert!(hard.difficulty > 0.5, "{}", hard.summary_line());
        assert_eq!(hard.tier, Tier::Strong);

        let easy = predict("rename a variable across the project and update the imports");
        assert!(easy.difficulty < 0.5, "{}", easy.summary_line());
    }

    #[test]
    fn probabilities_are_a_distribution() {
        let s = predict("write a parser for CSV with quoted fields");
        let sum: f32 = s.shape_probs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-3, "probs sum to {sum}");
        assert!(s.confidence > 0.0 && s.confidence <= 1.0);
    }

    #[test]
    fn training_is_deterministic() {
        let a = Coordinator::seed();
        let b = Coordinator::seed();
        let ta = a.predict("implement Dijkstra with a Fibonacci heap");
        let tb = b.predict("implement Dijkstra with a Fibonacci heap");
        assert_eq!(ta.shape, tb.shape);
        assert!((ta.shape_probs[0] - tb.shape_probs[0]).abs() < 1e-6);
    }

    #[test]
    fn json_round_trips() {
        let coord = Coordinator::seed();
        let json = serde_json::to_string(&coord).unwrap();
        let back: Coordinator = serde_json::from_str(&json).unwrap();
        let a = coord.predict("design the auth approach");
        let b = back.predict("design the auth approach");
        assert_eq!(a.shape, b.shape);
        assert!((a.confidence - b.confidence).abs() < 1e-6);
    }

    #[test]
    fn traces_bias_the_model() {
        // Feeding repeated trace examples should be learnable without breaking
        // the seed priors.
        let extra: Vec<(String, Shape)> = std::iter::repeat_n(
            (
                "implement a bespoke binary protocol decoder".to_string(),
                Shape::Solo,
            ),
            5,
        )
        .collect();
        let coord = Coordinator::trained_from(&extra);
        assert!(coord.trace_count >= 5);
        assert_eq!(
            coord
                .predict("implement a bespoke binary protocol decoder")
                .shape,
            Shape::Solo
        );
    }
}
