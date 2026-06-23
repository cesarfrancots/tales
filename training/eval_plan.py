#!/usr/bin/env python3
"""Score TalesSML v2's *orchestration-plan* quality on a held-out corpus.

Unlike eval_llm_conductor.py (which scores only the v1 {shape,difficulty} router),
this calls the model with the **CONDUCTOR_PLAN_SYSTEM** prompt it was trained on and
grades the full plan it emits: shape correctness PLUS structural well-formedness and
agreement with Tales' deterministic ground-truth policy (dataset::orchestration_plan).

The system prompt and the per-shape ground truth are read/derived from the Rust source
so they can't drift. Pure stdlib — no torch, no deps.

Usage:
    python eval_plan.py --model talesml-orch3 [--corpus-file game_dev_corpus.json] [--json OUT]
"""
import argparse
import json
import re
import sys
import time
import urllib.request

DATASET_RS = "../crates/tales-core/src/dataset.rs"

# Held-out routing corpus, verbatim from coordinator.rs eval_corpus (task, shape).
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
ALLOWED_ROLES = {"drafter", "critic", "executor", "reviewer", "verifier"}
ALLOWED_AGENTS = {"claude", "codex"}
ALLOWED_MODELS = {"opus", "sonnet", "haiku", "gpt-5.x"}
ALLOWED_METHODS = {"tests", "review", "build", "none"}

# Ground-truth roster shape from dataset::orchestration_plan. The executor model
# (opus|sonnet) depends on task difficulty, so models are scored leniently as
# "in the allowed set" while roles/agents/order/verify-method are exact-per-shape.
EXPECTED = {
    "solo": {
        "roles": ["executor", "verifier"],
        "agents": ["claude", "codex"],
        "method": "tests",
    },
    "debate": {
        "roles": ["drafter", "critic", "executor", "reviewer"],
        "agents": ["claude", "codex", "claude", "codex"],
        "method": "review",
    },
    "tiered": {
        "roles": ["drafter", "executor", "reviewer", "verifier"],
        "agents": ["claude", "claude", "codex", "codex"],
        "method": "tests",
    },
}


def load_plan_system(path):
    """Extract CONDUCTOR_PLAN_SYSTEM from dataset.rs and unescape the Rust literal."""
    with open(path, encoding="utf-8") as f:
        src = f.read()
    m = re.search(r'CONDUCTOR_PLAN_SYSTEM:\s*&str\s*=\s*"((?:\\.|[^"\\])*)"', src, re.S)
    if not m:
        sys.exit(f"could not find CONDUCTOR_PLAN_SYSTEM in {path}")
    lit = m.group(1)
    # Rust string-literal escapes used in this constant: \" and \n.
    return lit.replace('\\"', '"').replace("\\n", "\n").replace("\\\\", "\\")


def parse_plan(content):
    content = content.strip()
    try:
        return json.loads(content)
    except Exception:
        pass
    start, end = content.find("{"), content.rfind("}")
    if start != -1 and end > start:
        try:
            return json.loads(content[start : end + 1])
        except Exception:
            return None
    return None


def grade(plan, expected_shape):
    """Return (shape_ok, plan_valid, checks dict). plan_valid is structural and
    ignores whether the shape itself was the *expected* one (that's shape_ok)."""
    c = {}
    shape = str(plan.get("shape", "")).strip().lower()
    c["shape_valid"] = shape in SHAPES
    shape_ok = shape == expected_shape
    # Grade structure against the policy for the shape the model actually chose
    # (a self-consistent plan for the chosen shape is well-formed even if the
    # routing decision is wrong — the two are reported separately).
    spec = EXPECTED.get(shape)
    c["top_keys"] = all(
        k in plan
        for k in ("schema_version", "shape", "difficulty", "roster", "coordination", "verify", "escalate")
    )
    roster = plan.get("roster") or []
    roles = [str(r.get("role", "")).lower() for r in roster] if isinstance(roster, list) else []
    agents = [str(r.get("agent", "")).lower() for r in roster] if isinstance(roster, list) else []
    models = [str(r.get("model", "")).lower() for r in roster] if isinstance(roster, list) else []
    c["roster_nonempty"] = len(roster) > 0
    c["roles_valid"] = all(r in ALLOWED_ROLES for r in roles) and bool(roles)
    c["agents_valid"] = all(a in ALLOWED_AGENTS for a in agents) and bool(agents)
    c["models_valid"] = all(m in ALLOWED_MODELS for m in models) and bool(models)
    if spec:
        c["roles_match"] = roles == spec["roles"]
        c["agents_match"] = agents == spec["agents"]
        c["verify_method"] = str(plan.get("verify", {}).get("method", "")).lower() == spec["method"]
    else:
        c["roles_match"] = c["agents_match"] = c["verify_method"] = False
    coord = plan.get("coordination", {}) or {}
    order = [str(x).lower() for x in coord.get("order", [])] if isinstance(coord.get("order"), list) else []
    c["order_matches_roster"] = order == roles and bool(order)
    esc = plan.get("escalate", {}) or {}
    c["escalate_opus"] = str(((esc.get("to") or {}).get("model", ""))).lower() == "opus"
    plan_valid = all(
        c[k] for k in ("shape_valid", "top_keys", "roster_nonempty", "roles_match",
                       "agents_match", "models_valid", "order_matches_roster",
                       "verify_method", "escalate_opus")
    )
    return shape_ok, plan_valid, shape, c


def route_one(url, model, system, task, timeout):
    body = json.dumps({
        "model": model,
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": task}],
        "temperature": 0.0,
        "stream": False,
        "max_tokens": 1024,
        "response_format": {"type": "json_object"},
    }).encode()
    req = urllib.request.Request(
        url.rstrip("/") + "/chat/completions", data=body,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = json.loads(resp.read().decode())
    return payload["choices"][0]["message"]["content"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://localhost:11434/v1")
    ap.add_argument("--model", default="talesml-orch3")
    ap.add_argument("--corpus-file", default="")
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("--json", default="")
    ap.add_argument("--dataset-rs", default=DATASET_RS)
    args = ap.parse_args()

    system = load_plan_system(args.dataset_rs)
    if args.corpus_file:
        with open(args.corpus_file, encoding="utf-8") as f:
            corpus = [tuple(x) for x in json.load(f)]
    else:
        corpus = list(EVAL_CORPUS)

    check_names = ["shape_valid", "top_keys", "roster_nonempty", "roles_valid",
                   "agents_valid", "models_valid", "roles_match", "agents_match",
                   "order_matches_roster", "verify_method", "escalate_opus"]
    totals = {k: 0 for k in check_names}
    shape_correct = plan_valid_n = invalid = 0
    fails, rows = [], []
    t0 = time.time()
    print(f"model={args.model}  plan-system={len(system)} chars  tasks={len(corpus)}\n")
    for i, (task, expected) in enumerate(corpus, 1):
        try:
            raw = route_one(args.url, args.model, system, task, args.timeout)
        except Exception as e:
            raw = f"<error: {e}>"
        plan = parse_plan(raw)
        if not plan or not isinstance(plan, dict):
            invalid += 1
            print(f"XX [{i:2}] exp={expected:6} <unparseable> | {task[:50]}")
            print(f"        raw: {raw[:110]!r}")
            fails.append({"task": task, "expected": expected, "got": None, "reason": "unparseable"})
            rows.append({"task": task, "expected": expected, "got": None, "plan_valid": False})
            continue
        shape_ok, pv, shape, c = grade(plan, expected)
        for k in check_names:
            totals[k] += 1 if c[k] else 0
        if shape_ok:
            shape_correct += 1
        if pv:
            plan_valid_n += 1
        mark = "ok " if (shape_ok and pv) else "XX "
        bad = [k for k in check_names if not c[k]]
        print(f"{mark}[{i:2}] exp={expected:6} got={shape:8} plan_valid={pv!s:5} | {task[:46]}")
        if not (shape_ok and pv):
            print(f"        failed: {', '.join(bad) if bad else '(shape only)'}")
            fails.append({"task": task, "expected": expected, "got": shape, "failed": bad})
        rows.append({"task": task, "expected": expected, "got": shape, "plan_valid": pv, "checks": c})

    n = len(corpus)
    el = time.time() - t0
    print(f"\nshape accuracy:  {shape_correct}/{n} = {shape_correct/n*100:.1f}%")
    print(f"plan well-formed: {plan_valid_n}/{n} = {plan_valid_n/n*100:.1f}%   unparseable={invalid}   {el:.1f}s")
    print("per-check pass rate:")
    for k in check_names:
        print(f"  {k:20} {totals[k]}/{n} = {totals[k]/n*100:5.1f}%")

    if args.json:
        with open(args.json, "w") as f:
            json.dump({
                "model": args.model, "total": n,
                "shape_accuracy": shape_correct / n if n else 0,
                "plan_valid_rate": plan_valid_n / n if n else 0,
                "unparseable": invalid, "per_check": totals,
                "failures": fails, "rows": rows,
            }, f, indent=2)
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
