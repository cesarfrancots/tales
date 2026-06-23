#!/usr/bin/env python3
"""Score a served LLM conductor's routing accuracy on the held-out corpus.

Calls the model EXACTLY as Tales' `tales-core::llm_conductor::LlmConductor` does
(same CONDUCTOR_SYSTEM prompt, same {"shape","difficulty"} contract, temperature
0, lenient brace-extraction), so the number this prints is the real routing
quality `tales run --conductor llm` would get. Pure stdlib — no torch, no deps.

Usage:
    python eval_llm_conductor.py [--url URL] [--model NAME] [--max N] [--json OUT]
Defaults to the local ollama OpenAI-compatible endpoint and qwen3.5:9b.
"""
import argparse
import json
import subprocess
import sys
import time
import urllib.request

# Verbatim from crates/tales-core/src/dataset.rs (CONDUCTOR_SYSTEM). Keep in sync.
CONDUCTOR_SYSTEM = (
    "You are Tales' orchestration conductor. Given one coding task, reply ONLY with "
    'compact JSON: {"shape":"solo|debate|tiered","difficulty":0.0-1.0}.\n\n'
    "Choose by the task's primary work:\n"
    "- solo = one strong model plans AND executes. Use for self-contained "
    "correctness-heavy implementation where correctness IS the implementation: "
    "algorithms, parsers, data structures, tricky focused bugs, precise logic.\n"
    "- debate = two planners argue the approach, then execute. Use for ambiguous, "
    "architecture-DEFINING work with no known-correct answer. Triggers: propose, "
    "choose, decide, design architecture, select approach, compare strategy, "
    "evaluate tradeoffs.\n"
    "- tiered = strong models plan, a cheap model executes. Use for voluminous, "
    "MECHANICAL, repetitive work across many files/resources, or ONE uniform change "
    "applied to MANY sites. Triggers: scaffold, generate for each, migrate, convert, "
    "rename, replace, update all, codemod, CRUD pages, boilerplate, "
    "every/all/each/throughout/across handlers/endpoints/controllers/files/modules/call sites.\n\n"
    "Boundary rules: if the design is already implied and the hard part is "
    "volume/repetition, choose tiered, not debate. If a task applies the same change "
    "across many sites, choose tiered even when the subsystem sounds architectural or "
    "the per-site edit sounds small. If the task asks to propose/choose/decide the "
    "architecture/approach/strategy, choose debate, not solo or tiered. difficulty is "
    "correctness risk / how unsafe it is to run cheap."
)

# Verbatim from crates/tales-core/src/coordinator.rs (eval_corpus) — held-out,
# never used to train/seed. (task, expected_shape).
EVAL_CORPUS = [
    ("implement a trie supporting prefix search and deletion", "solo"),
    ("implement quicksort with a median-of-three pivot", "solo"),
    ("build a bloom filter backed by k hash functions", "solo"),
    ("write a function to detect a cycle in a directed graph", "solo"),
    ("implement a min-heap with a decrease-key operation", "solo"),
    ("implement base64 encode and decode without a library", "solo"),
    ("migrate all the test files from mocha to jest", "tiered"),
    ("rename the legacy api endpoints throughout the service", "tiered"),
    ("scaffold crud pages for each admin resource", "tiered"),
    ("convert all callbacks to async await across the codebase", "tiered"),
    ("add logging to every handler across the controllers", "tiered"),
    ("generate the client stubs for all endpoints", "tiered"),
    ("figure out the best approach and tradeoffs for offline-first sync", "debate"),
    ("decide whether to shard the database and how", "debate"),
    ("propose an architecture for the notification system", "debate"),
    ("evaluate the options for the plugin extension model", "debate"),
    ("choose the right approach for feature flags", "debate"),
    ("design the access-control architecture and approach for new tenants", "debate"),
]

SHAPES = ("solo", "debate", "tiered")


def parse_decision(content):
    """Mirror LlmConductor::parse_decision — direct parse, else first {..} slice."""
    content = content.strip()
    try:
        return json.loads(content)
    except Exception:
        pass
    start = content.find("{")
    end = content.rfind("}")
    if start != -1 and end > start:
        try:
            return json.loads(content[start : end + 1])
        except Exception:
            return None
    return None


def route_one(url, model, task, timeout):
    body = json.dumps(
        {
            "model": model,
            "messages": [
                {"role": "system", "content": CONDUCTOR_SYSTEM},
                {"role": "user", "content": task},
            ],
            "temperature": 0.0,
            "stream": False,
        }
    ).encode()
    req = urllib.request.Request(
        url.rstrip("/") + "/chat/completions",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = json.loads(resp.read().decode())
    content = payload["choices"][0]["message"]["content"]
    decision = parse_decision(content)
    if not decision or "shape" not in decision:
        return None, None, content
    shape = str(decision["shape"]).strip().lower()
    diff = decision.get("difficulty")
    return shape, diff, content


def route_keyword(bin_path, task):
    """Route via the embedded keyword coordinator (`tales coordinator predict`),
    for an apples-to-apples baseline on the same corpus."""
    out = subprocess.run(
        [bin_path, "coordinator", "predict", task, "--json"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    payload = json.loads(out.stdout)
    strat = payload["strategy"]
    return str(strat["shape"]).lower(), strat.get("difficulty"), out.stdout[:120]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://localhost:11434/v1")
    ap.add_argument("--model", default="qwen3.5:9b")
    ap.add_argument("--max", type=int, default=0, help="limit to first N tasks (0=all)")
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("--json", default="", help="write a JSON report to this path")
    ap.add_argument("--corpus-file", default="", help="JSON [[task, shape], ...] to score instead of the held-out corpus")
    ap.add_argument("--keyword", action="store_true", help="score the keyword coordinator (via --keyword-bin) instead of the LLM")
    ap.add_argument("--keyword-bin", default="../target/debug/tales.exe", help="path to the tales binary for --keyword")
    ap.add_argument(
        "--system-file",
        default="",
        help="A/B a candidate system prompt from this file instead of the embedded one",
    )
    args = ap.parse_args()

    global CONDUCTOR_SYSTEM
    if args.system_file:
        with open(args.system_file, encoding="utf-8") as f:
            CONDUCTOR_SYSTEM = f.read().strip()
        print(f"(using candidate system prompt from {args.system_file}, {len(CONDUCTOR_SYSTEM)} chars)")

    if args.corpus_file:
        with open(args.corpus_file, encoding="utf-8") as f:
            corpus = [tuple(x) for x in json.load(f)]
    else:
        corpus = list(EVAL_CORPUS)
    if args.max:
        corpus = corpus[: args.max]
    confusion = {e: {p: 0 for p in SHAPES} for e in SHAPES}
    correct = 0
    misroutes = []
    invalid = 0
    rows = []
    t0 = time.time()

    router = "keyword:" + args.keyword_bin if args.keyword else f"llm:{args.model}"
    print(f"router={router}  tasks={len(corpus)}\n")
    for i, (task, expected) in enumerate(corpus, 1):
        try:
            if args.keyword:
                got, diff, raw = route_keyword(args.keyword_bin, task)
            else:
                got, diff, raw = route_one(args.url, args.model, task, args.timeout)
        except Exception as e:
            got, diff, raw = None, None, f"<error: {e}>"
        ok = got == expected
        if got in SHAPES:
            confusion[expected][got] += 1
            if ok:
                correct += 1
            else:
                misroutes.append((task, expected, got))
        else:
            invalid += 1
            misroutes.append((task, expected, got or "INVALID"))
        mark = "ok " if ok else "XX "
        diff_s = f"{diff:.2f}" if isinstance(diff, (int, float)) else str(diff)
        print(f"{mark}[{i:2}] exp={expected:6} got={str(got):8} diff={diff_s:5} | {task[:54]}")
        if not ok:
            print(f"        raw: {raw[:120]!r}")
        rows.append({"task": task, "expected": expected, "got": got, "difficulty": diff})

    n = len(corpus)
    acc = correct / n if n else 0.0
    elapsed = time.time() - t0
    print(f"\naccuracy: {correct}/{n} = {acc*100:.1f}%   invalid_replies={invalid}   {elapsed:.1f}s")
    print("recall by shape:")
    for s in SHAPES:
        tot = sum(confusion[s].values())
        r = confusion[s][s] / tot if tot else 0.0
        print(f"  {s:7} {r*100:5.1f}%   row={confusion[s]}")
    if misroutes:
        print("\nmisroutes:")
        for task, exp, got in misroutes:
            print(f"  expected {exp:6} got {got:8} | {task}")

    if args.json:
        with open(args.json, "w") as f:
            json.dump(
                {
                    "model": args.model,
                    "url": args.url,
                    "accuracy": acc,
                    "correct": correct,
                    "total": n,
                    "invalid": invalid,
                    "confusion": confusion,
                    "misroutes": [
                        {"task": t, "expected": e, "got": g} for (t, e, g) in misroutes
                    ],
                    "rows": rows,
                },
                f,
                indent=2,
            )
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
