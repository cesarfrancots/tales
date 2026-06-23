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

// --- Adversarial mixed-signal hardening -------------------------------------
// Clean templates teach "migrate all -> tiered, decide -> debate", but a model
// trained only on those gets fooled by mixed signals (the exact failure the
// keyword coordinator has). These examples teach the *boundaries* — labels are
// ground-truth by construction, matching the shape definitions and the v3 prompt.

/// Algorithmic-sounding nouns that, under a mechanical verb, are still TIERED.
const ADV_ALGO_NOUNS: &[&str] = &[
    "quicksort",
    "heapsort",
    "binary-search",
    "hash",
    "parser",
    "tokenizer",
    "graph-traversal",
    "LRU cache",
    "b-tree",
    "regex",
];
/// A uniform/mechanical edit over an algorithmic subsystem -> TIERED, not solo/debate.
const ADV_TIERED_TEMPLATES: &[&str] = &[
    "rename the {n} helpers to camelCase across all files",
    "replace every call to the {n} routine throughout the codebase",
    "convert all the {n} modules to the new API across the repo",
    "update each handler that uses the {n} to the new signature project-wide",
];
const ADV_SITES: &[&str] = &[
    "API endpoint",
    "request handler",
    "controller",
    "route",
    "serializer",
    "service",
];
const ADV_ADD_THINGS: &[&str] = &[
    "a traceId field",
    "structured logging",
    "the new auth header",
    "a correlation id",
];

/// A DECISION wearing volume/mechanical clothing -> DEBATE (the deciding is the work).
const ADV_SYS: &[&str] = &[
    "services",
    "cron jobs",
    "controllers",
    "data models",
    "endpoints",
];
const ADV_TECH: &[&str] = &[
    "the message queue",
    "gRPC",
    "the new framework",
    "an event-sourced model",
];
const ADV_DEBATE_TEMPLATES: &[&str] = &[
    "decide whether to migrate all the {s} to {t}, and lay out the rollout plan",
    "weigh whether to rename the entire {s} layer, given the downstream breakage",
    "should we rewrite all the {s} or refactor them in place — make the call",
    "figure out whether to convert every one of the {s} to {t} and how",
];

/// Focused correctness bugs (no algorithmic keyword) or exact-spec work -> SOLO.
const ADV_SOLO: &[&str] = &[
    "two browser tabs both submit checkout and the cart double-charges; make it deterministic",
    "a race between retries duplicates the write; make it exactly-once and correct",
    "under load the cache serves stale prices; make reads consistent and correct",
    "the pagination skips a row when items are deleted mid-scroll; fix it precisely",
    "implement the rate limiter exactly as the approved spec defines, matching every edge case",
    "the CSV importer mis-parses quoted commas; make it spec-correct on every case",
    "the timezone math is off by an hour on DST boundaries; make it exact",
    "the idempotency key occasionally collides; make duplicate requests safe",
];

/// Codex-proposed examples that *complement* the adversarial pass (Codex as the
/// training reviewer): mechanical edits over crypto/algorithmic-sounding nouns ->
/// tiered, balance/tune/choose-tradeoffs -> debate, focused game-dev bugs/features
/// -> solo. Curated against TalesSML's measured misroutes. See training/README.
const CODEX_COMPLEMENT: &[(&str, Shape)] = &[
    ("replace every md5_hash(...) helper call with sha256_hash(...) across the backend services", Shape::Tiered),
    ("rename all crc32_checksum fields to content_digest in API structs, database mappers, and tests", Shape::Tiered),
    ("swap every recursive_fib(...) call in benchmark fixtures to iterative_fib(...) without changing behavior", Shape::Tiered),
    ("replace all AES128Encrypt usages with AES256Encrypt across payment, auth, worker, and migration code", Shape::Tiered),
    ("convert every quicksortMetrics label to sortMetrics across telemetry emitters and dashboards", Shape::Tiered),
    ("replace every legacy_rng(...) call with secure_rng(...) throughout matchmaking, loot, and tests", Shape::Tiered),
    ("update every DynamicProgrammingSolver import path after moving it into the algorithms module", Shape::Tiered),
    ("change every SHA1_FINGERPRINT column reference to SHA256_FINGERPRINT in repositories, queries, and fixtures", Shape::Tiered),
    ("choose whether enemy pathfinding should use A*, flow fields, or waypoint graphs for 200-unit battles", Shape::Debate),
    ("balance the stamina cost, cooldown, and damage for the rogue dash attack against the warrior charge", Shape::Debate),
    ("decide whether the crafting system should use fixed recipes, discovery-based recipes, or skill-tree unlocks", Shape::Debate),
    ("tune all eight tower-defense towers for fair early-game and late-game viability", Shape::Debate),
    ("choose the tradeoff between deterministic lockstep networking and server-authoritative snapshots for multiplayer", Shape::Debate),
    ("balance enemy spawn rates, resource drops, and wave lengths so each difficulty mode feels distinct", Shape::Debate),
    ("decide whether boss fights should prioritize pattern memorization, reactive dodging, or build preparation", Shape::Debate),
    ("choose how to trade off card rarity, mana cost, and win rate when rebalancing the starter deck", Shape::Debate),
    ("fix the off-by-one bug in the inventory slot index calculation", Shape::Solo),
    ("implement binary search for the sorted asset manifest lookup", Shape::Solo),
    ("make the jump buffer accept input up to 120ms before landing", Shape::Solo),
    ("fix the collision resolver so the player cannot clip through one-tile walls", Shape::Solo),
    ("parse the level seed string into a u64 and return a clear error for invalid input", Shape::Solo),
    ("add a clamp so camera zoom stays between the configured min and max values", Shape::Solo),
    ("fix the save-file checksum validation to reject truncated files", Shape::Solo),
    ("implement deterministic dice rolls from a provided game seed", Shape::Solo),
];

// Mechanical bulk edits over architectural/infra-sounding nouns, with varied
// determiners ("the", "all the", "the legacy", "every one of") — these *read*
// architectural but the work is uniform volume, so they're TIERED. Targets the
// tiered/debate boundary the model slipped on. Phrasings differ from the held-out
// eval tasks on purpose: teach the boundary, don't memorize the test.
const ADV_INFRA_NOUNS: &[&str] = &[
    "api endpoints",
    "route handlers",
    "controllers",
    "serializers",
    "data models",
    "config files",
    "service modules",
    "database queries",
    "view templates",
    "graphql resolvers",
];
const ADV_INFRA_TIERED_TEMPLATES: &[&str] = &[
    "rename the legacy {n} to the new naming convention across the project",
    "update all the deprecated {n} to the new signature repo-wide",
    "scaffold the standard CRUD pages for each of the {n}",
    "regenerate all the {n} after the schema migration",
    "convert every one of the {n} to the new format across all packages",
];
// Function-call swaps across the codebase — mechanical despite the crypto/algo
// flavor -> TIERED (the "md5(->sha256(" class). Different fn pairs than the eval.
const ADV_FNSWAP: &[&str] = &[
    "replace every sha1( call with sha512( across the whole service layer",
    "swap each base64encode( call for base64urlencode( throughout the api code",
    "change every gettime( call to getmonotonic( across all the workers",
    "replace each parseInt( call with safeParseInt( repo-wide",
    "convert every logf( call to structuredLog( across all the handlers",
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

    // Adversarial mixed-signal hardening (see consts above).
    for tmpl in ADV_TIERED_TEMPLATES {
        for n in ADV_ALGO_NOUNS {
            out.push((tmpl.replace("{n}", n), Shape::Tiered));
        }
    }
    for thing in ADV_ADD_THINGS {
        for site in ADV_SITES {
            out.push((
                format!("add {thing} to every {site} across all services"),
                Shape::Tiered,
            ));
        }
    }
    for tmpl in ADV_DEBATE_TEMPLATES {
        for s in ADV_SYS {
            for t in ADV_TECH {
                out.push((tmpl.replace("{s}", s).replace("{t}", t), Shape::Debate));
            }
        }
    }
    for task in ADV_SOLO {
        out.push((task.to_string(), Shape::Solo));
    }
    // Architectural-sounding nouns under bulk verbs, and crypto/algo fn-call swaps
    // -> still TIERED (the boundary the model misread).
    for tmpl in ADV_INFRA_TIERED_TEMPLATES {
        for n in ADV_INFRA_NOUNS {
            out.push((tmpl.replace("{n}", n), Shape::Tiered));
        }
    }
    for task in ADV_FNSWAP {
        out.push((task.to_string(), Shape::Tiered));
    }
    for (task, shape) in CODEX_COMPLEMENT {
        out.push((task.to_string(), *shape));
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

// --- TalesSML v2: orchestration-plan target --------------------------------
// Beyond shape+difficulty, TalesSML reasons about WHICH agents/models to call,
// WHY, and HOW to coordinate them. We synthesize the full plan by deterministic
// policy (ground-truth by construction), so a bigger base (Qwen2.5-3B) can learn
// to emit a real orchestration plan, not just a 3-way label. Designed with Codex.

/// System prompt for the orchestration-plan model. Teaches the schema + the agent
/// pool. Kept identical between training and inference.
pub const CONDUCTOR_PLAN_SYSTEM: &str = "You are TalesSML, the orchestration brain for Tales. Given one coding task, decide HOW to coordinate AI agents to deliver it, and reply with ONE valid JSON object and NOTHING else: no prose, no markdown, no code fences, no comments. Output every field exactly once and stop after the closing brace.\n\nUse ONLY these tokens; never invent others:\n- shape: solo | debate | tiered\n- roster[].role: drafter | critic | executor | reviewer | verifier\n- roster[].agent: claude | codex\n- roster[].model: opus | sonnet | haiku | gpt-5.x\n- verify.method: tests | review | build | none\n\ndifficulty is a single decimal between 0 and 1 with at most two decimals (e.g. 0.2, 0.6, 0.85). Write it once, as one clean number — no extra digits, no second number, no trailing tokens.\n\nEmit exactly this schema, with the top-level keys in this exact order:\n{\"coordination\":{\"order\":[\"...\"],\"parallelizable\":true,\"handoff\":\"...\"},\"difficulty\":0.6,\"escalate\":{\"if\":[\"...\"],\"to\":{\"agent\":\"claude\",\"model\":\"opus\"}},\"rationale\":\"short reason\",\"risks\":[\"...\"],\"roster\":[{\"role\":\"drafter|critic|executor|reviewer|verifier\",\"agent\":\"claude|codex\",\"model\":\"opus|sonnet|haiku|gpt-5.x\",\"why\":\"capability match\"}],\"schema_version\":1,\"shape\":\"solo|debate|tiered\",\"verify\":{\"required\":true,\"method\":\"tests|review|build|none\",\"agent\":\"codex\"}}\n\nAgents: claude (opus=strongest reasoning, sonnet=balanced, haiku=cheap/fast) and codex (gpt-5.x, best reviewer). shape: solo=one strong model plans+executes (hard, self-contained); debate=two planners argue then execute (ambiguous, architecture-defining); tiered=strong models plan, a cheap model executes (voluminous, mechanical). Choose the roster, order, verification, and escalation that fit the task's difficulty and risk; coordination.order must list the roster roles in order; escalate to opus when difficulty is high or the task is correctness/security-sensitive.";

fn role(role: &str, agent: &str, model: &str, why: &str) -> serde_json::Value {
    json!({ "role": role, "agent": agent, "model": model, "why": why })
}

/// Build the ground-truth orchestration plan for a task by deterministic policy.
/// This is exactly what TalesSML learns to emit.
pub fn orchestration_plan(task: &str, shape: Shape) -> serde_json::Value {
    let f = crate::coordinator::extract_features(task);
    let difficulty = shape_difficulty(shape);
    let ambiguity = f[5] > 0.3;
    let correctness = f[6] > 0.3;
    let breadth = f[7] > 0.3;
    let hard = difficulty >= 0.75 || correctness;
    let exec_model = if hard { "opus" } else { "sonnet" };

    let (roster, order, parallel, method): (Vec<serde_json::Value>, Vec<&str>, bool, &str) =
        match shape {
            Shape::Solo => (
                vec![
                    role(
                        "executor",
                        "claude",
                        exec_model,
                        "self-contained correctness-heavy work needs one strong model",
                    ),
                    role(
                        "verifier",
                        "codex",
                        "gpt-5.x",
                        "tests/review confirm the implementation is correct",
                    ),
                ],
                vec!["executor", "verifier"],
                false,
                "tests",
            ),
            Shape::Debate => (
                vec![
                    role(
                        "drafter",
                        "claude",
                        "sonnet",
                        "proposes the architecture/approach",
                    ),
                    role(
                        "critic",
                        "codex",
                        "gpt-5.x",
                        "independent review surfaces design flaws",
                    ),
                    role(
                        "executor",
                        "claude",
                        exec_model,
                        "implements the agreed design",
                    ),
                    role(
                        "reviewer",
                        "codex",
                        "gpt-5.x",
                        "checks the result against the agreed design",
                    ),
                ],
                vec!["drafter", "critic", "executor", "reviewer"],
                true, // planners draft in parallel, then synthesize
                "review",
            ),
            Shape::Tiered => (
                vec![
                    role(
                        "drafter",
                        "claude",
                        "sonnet",
                        "plans the repetitive change once",
                    ),
                    role(
                        "executor",
                        "claude",
                        "haiku",
                        "a cheap, fast model applies the mechanical change",
                    ),
                    role("reviewer", "codex", "gpt-5.x", "spot-checks the bulk edit"),
                    role(
                        "verifier",
                        "codex",
                        "gpt-5.x",
                        "tests confirm nothing broke",
                    ),
                ],
                vec!["drafter", "executor", "reviewer", "verifier"],
                false,
                "tests",
            ),
        };

    let mut risks: Vec<&str> = Vec::new();
    if ambiguity {
        risks.push("unclear requirements");
    }
    if breadth {
        risks.push("large blast radius");
    }
    if correctness {
        risks.push("correctness-critical");
    }

    let mut escalate_if: Vec<&str> = vec!["failed_verification"];
    if !hard {
        escalate_if.push("low_confidence");
    }
    if correctness {
        escalate_if.push("security_sensitive");
    }

    json!({
        "schema_version": 1,
        "shape": shape.as_str(),
        // Round to 2 decimals: a raw f32 serializes as 0.20000000298023224, and
        // that long high-entropy digit string is what the SLM hallucinates extra
        // numeric tokens around (corrupting the JSON). A clean 0.2 is trivial to
        // emit. Matches the v1 routing target's `{:.2}` formatting.
        "difficulty": (f64::from(difficulty) * 100.0).round() / 100.0,
        "rationale": plan_rationale(shape, &f),
        "risks": risks,
        "roster": roster,
        "coordination": {
            "order": order,
            "parallelizable": parallel,
            "handoff": "brief context + explicit acceptance criteria",
        },
        "verify": { "required": true, "method": method, "agent": "codex" },
        "escalate": { "if": escalate_if, "to": { "agent": "claude", "model": "opus" } },
    })
}

fn plan_rationale(shape: Shape, f: &crate::coordinator::Features) -> String {
    let driver = if f[3] > 0.3 {
        "algorithmic difficulty"
    } else if f[4] > 0.3 {
        "mechanical volume"
    } else if f[5] > 0.3 {
        "architectural ambiguity"
    } else if f[6] > 0.3 {
        "correctness risk"
    } else {
        "the task shape"
    };
    match shape {
        Shape::Solo => format!("{driver}: one strong model implements, a reviewer verifies"),
        Shape::Debate => {
            format!("{driver}: two planners debate the approach before a strong model executes")
        }
        Shape::Tiered => format!(
            "{driver}: strong models plan once, a cheap model executes the bulk, a reviewer checks"
        ),
    }
}

/// Render a corpus as chat-JSONL where the assistant emits the full orchestration
/// plan (TalesSML v2 training target).
pub fn to_plan_jsonl(corpus: &[(String, Shape)]) -> String {
    let mut s = String::new();
    for (task, shape) in corpus {
        let plan = orchestration_plan(task, *shape);
        let line = json!({
            "messages": [
                {"role": "system", "content": CONDUCTOR_PLAN_SYSTEM},
                {"role": "user", "content": task},
                {"role": "assistant", "content": plan.to_string()},
            ]
        });
        s.push_str(&line.to_string());
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orchestration_plans_are_valid_and_well_formed() {
        let corpus = generate();
        // Sample one task of each shape so all roster branches are exercised.
        for shape in Shape::ALL {
            let (task, _) = corpus.iter().find(|(_, s)| *s == shape).unwrap();
            let plan = orchestration_plan(task, shape);
            assert_eq!(plan["shape"], shape.as_str());
            let roster = plan["roster"].as_array().unwrap();
            assert!(!roster.is_empty(), "roster must not be empty");
            for r in roster {
                assert!(matches!(
                    r["agent"].as_str(),
                    Some("claude") | Some("codex")
                ));
                assert!(r["role"].is_string() && r["why"].is_string());
            }
            assert!(plan["coordination"]["order"].as_array().unwrap().len() == roster.len());
            assert_eq!(plan["escalate"]["to"]["model"], "opus");
            // the plan stays backward-compatible: shape + difficulty are top-level.
            assert!(plan["difficulty"].as_f64().unwrap() >= 0.0);
        }
        // The whole JSONL round-trips as valid chat lines with parseable plans.
        let jsonl = to_plan_jsonl(&corpus[..3]);
        for line in jsonl.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let content = v["messages"][2]["content"].as_str().unwrap();
            let _plan: serde_json::Value = serde_json::from_str(content).unwrap();
        }
    }

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
