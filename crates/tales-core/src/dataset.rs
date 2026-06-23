//! Training-data synthesis for the LLM conductor.
//!
//! The fine-tuned specialist conductor needs labeled examples: *task → routing
//! decision*. We synthesize them from templates whose label is ground-truth **by
//! construction** — so the dataset is correct even for tasks the keyword
//! [`crate::coordinator`] would misread. That is precisely how the fine-tuned
//! model surpasses the keyword ceiling: it learns from clean labels, not keyword
//! counts.
//!
//! Output is chat-format JSONL (one JSON object per line) compatible with common
//! SFT/LoRA tooling (axolotl, llama-factory, unsloth): each line is
//! `{"messages":[{system},{user:task},{assistant:decision}]}`. Generation is
//! deterministic given the same templates, so the dataset is reproducible.

use serde_json::json;

use crate::coordinator::Shape;

/// System prompt the conductor model is trained (and later served) with. Keep
/// this identical between training and inference so the model sees what it learned.
pub const CONDUCTOR_SYSTEM: &str = "You are Tales' orchestration conductor. Given one coding task, reply ONLY with compact JSON: {\"shape\":\"solo|debate|tiered\",\"difficulty\":0.0-1.0}.\n\nChoose by the task's primary work:\n- solo = one strong model plans AND executes. Use for self-contained correctness-heavy implementation where correctness IS the implementation: algorithms, parsers, data structures, tricky focused bugs, precise logic.\n- debate = two planners argue the approach, then execute. Use for ambiguous, architecture-DEFINING work with no known-correct answer. Triggers: propose, choose, decide, design architecture, select approach, compare strategy, evaluate tradeoffs.\n- tiered = strong models plan, a cheap model executes. Use for voluminous, MECHANICAL, repetitive work across many files/resources, or ONE uniform change applied to MANY sites. Triggers: scaffold, generate for each, migrate, convert, rename, replace, update all, codemod, CRUD pages, boilerplate, every/all/each/throughout/across handlers/endpoints/controllers/files/modules/call sites.\n\nBoundary rules: if the design is already implied and the hard part is volume/repetition, choose tiered, not debate. If a task applies the same change across many sites, choose tiered even when the subsystem sounds architectural or the per-site edit sounds small. If the task asks to propose/choose/decide the architecture/approach/strategy, choose debate, not solo or tiered. difficulty is correctness risk / how unsafe it is to run cheap.";

/// Difficulty target per shape (the label the model learns alongside the shape).
fn shape_difficulty(shape: Shape) -> f32 {
    match shape {
        Shape::Solo => 0.85,
        Shape::Debate => 0.60,
        Shape::Tiered => 0.20,
    }
}

// --- Solo: algorithmically hard, self-contained -----------------------------

const SOLO_SUBJECTS: &[&str] = &[
    "quicksort with a median-of-three pivot",
    "an in-place mergesort",
    "Dijkstra's shortest path with a binary heap",
    "an A* pathfinder with a consistent heuristic",
    "a red-black tree with deletion",
    "an AVL tree with rebalancing",
    "a B-tree with split and merge",
    "a trie with prefix search and deletion",
    "an open-addressing hash map",
    "a counting bloom filter",
    "a min-heap with decrease-key",
    "an O(1) LRU cache",
    "a backtracking regex engine",
    "a streaming JSON parser",
    "a recursive-descent expression parser",
    "a cycle-detecting topological sort",
    "the edit-distance dynamic program",
    "a 0/1 knapsack solver with reconstruction",
    "run-length encoding and decoding",
    "a suffix automaton for substring queries",
];

const SOLO_TEMPLATES: &[&str] = &[
    "implement {x} with full correctness",
    "write {x} from scratch",
    "build {x} and handle the edge cases",
    "implement {x} and verify it against an oracle",
    "code {x} with optimal time complexity",
];

// --- Tiered: mechanically voluminous ----------------------------------------

const TIERED_ACTIONS: &[&str] = &[
    "migrate all",
    "rename every",
    "convert all",
    "update all",
    "add tests for all",
    "reformat every",
    "regenerate all",
];

const TIERED_TARGETS: &[&str] = &[
    "the API endpoints",
    "the React components",
    "the test files",
    "the config files",
    "the model classes",
    "the route handlers",
    "the serializers",
    "the import statements",
];

const TIERED_SCOPES: &[&str] = &[
    "across the codebase",
    "throughout the service",
    "in every module",
    "project-wide",
    "across all packages",
];

// --- Debate: ambiguous, architecture-defining -------------------------------

const DEBATE_ASPECTS: &[&str] = &[
    "the caching strategy",
    "the auth architecture",
    "the data model",
    "the offline-sync approach",
    "the error-handling strategy",
    "the rate-limiting approach",
    "the sharding scheme",
    "the public API design",
];

const DEBATE_SYSTEMS: &[&str] = &[
    "the payments service",
    "the notification system",
    "the multi-tenant platform",
    "the offline-first app",
    "the analytics pipeline",
    "the plugin system",
];

const DEBATE_TEMPLATES: &[&str] = &[
    "decide {a} for {s}",
    "design {a} for {s}",
    "propose an approach to {a} in {s}",
    "evaluate the options for {a} of {s}",
    "figure out the best way to handle {a} in {s}",
];

/// Generate the full labeled corpus: every template × vocabulary combination per
/// shape. Deterministic and deduplicated; returns `(task, shape)` pairs.
pub fn generate() -> Vec<(String, Shape)> {
    let mut out: Vec<(String, Shape)> = Vec::new();

    for tmpl in SOLO_TEMPLATES {
        for subject in SOLO_SUBJECTS {
            out.push((tmpl.replace("{x}", subject), Shape::Solo));
        }
    }
    for action in TIERED_ACTIONS {
        for target in TIERED_TARGETS {
            // Half with an explicit scope, half without — natural variety.
            out.push((format!("{action} {target}"), Shape::Tiered));
            for scope in TIERED_SCOPES {
                out.push((format!("{action} {target} {scope}"), Shape::Tiered));
            }
        }
    }
    for tmpl in DEBATE_TEMPLATES {
        for aspect in DEBATE_ASPECTS {
            for system in DEBATE_SYSTEMS {
                out.push((
                    tmpl.replace("{a}", aspect).replace("{s}", system),
                    Shape::Debate,
                ));
            }
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Counts per shape in a corpus, for reporting balance.
pub fn shape_counts(corpus: &[(String, Shape)]) -> [usize; 3] {
    let mut counts = [0usize; 3];
    for (_, shape) in corpus {
        counts[shape.index()] += 1;
    }
    counts
}

/// Format one example as a chat-JSONL line.
fn chat_line(task: &str, shape: Shape) -> String {
    let completion = format!(
        "{{\"shape\":\"{}\",\"difficulty\":{:.2}}}",
        shape.as_str(),
        shape_difficulty(shape)
    );
    json!({
        "messages": [
            {"role": "system", "content": CONDUCTOR_SYSTEM},
            {"role": "user", "content": task},
            {"role": "assistant", "content": completion},
        ]
    })
    .to_string()
}

/// Render a corpus as chat-format JSONL (one object per line).
pub fn to_chat_jsonl(corpus: &[(String, Shape)]) -> String {
    let mut s = String::new();
    for (task, shape) in corpus {
        s.push_str(&chat_line(task, *shape));
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic_and_balanced() {
        let a = generate();
        let b = generate();
        assert_eq!(a, b, "generation must be reproducible");
        assert!(
            a.len() >= 300,
            "want a few hundred examples, got {}",
            a.len()
        );

        let counts = shape_counts(&a);
        // Every shape is well represented (none starved).
        for (i, c) in counts.iter().enumerate() {
            assert!(*c >= 40, "shape {i} underrepresented: {c}");
        }
    }

    #[test]
    fn no_template_placeholders_leak() {
        for (task, _) in generate() {
            assert!(
                !task.contains('{') && !task.contains('}'),
                "unfilled placeholder in: {task}"
            );
        }
    }

    #[test]
    fn jsonl_lines_are_valid_and_labeled() {
        let corpus = generate();
        let jsonl = to_chat_jsonl(&corpus);
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), corpus.len());

        // Spot-check the first line parses and carries the right structure.
        let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let messages = value["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[2]["role"], "assistant");
        // The assistant content is itself valid decision JSON with a known shape.
        let decision: serde_json::Value =
            serde_json::from_str(messages[2]["content"].as_str().unwrap()).unwrap();
        let shape = decision["shape"].as_str().unwrap();
        assert!(
            matches!(shape, "solo" | "debate" | "tiered"),
            "bad shape {shape}"
        );
    }
}
