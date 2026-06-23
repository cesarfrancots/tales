#!/usr/bin/env python3
"""Anti-hallucination guard for TalesSML's orchestration-plan output.

The served conductor model (a small LLM) sometimes emits broken or hallucinated
plans. This module is the runtime/inference guard: it leniently extracts the JSON
object, repairs the one *mechanical* corruption we can prove safe to repair (the
"corrupt-float" pattern), then validates every enum and the structure against the
ground-truth policy in `crates/tales-core/src/dataset.rs::orchestration_plan`.

Design principles (deliberately conservative):
  * Only *repair* the corrupt-float pattern, and only when it turns otherwise-
    invalid JSON into valid JSON. We never guess at content.
  * Never silently rewrite a hallucinated enum (e.g. shape "codemod"). We *flag*
    it as an issue and let the caller decide whether to fall back to the keyword
    router. Rewriting a hallucination would hide the model's mistake.
  * `valid` is True only when the plan parsed AND has zero enum/structural issues.

Pure stdlib — no torch, no deps. See GUARD-FINDINGS.md for the failure taxonomy
this is built from and how the Rust runtime / training should use it.
"""
import json
import re

# Allowed enum sets — must mirror dataset.rs / eval_plan.py exactly.
SHAPES = {"solo", "debate", "tiered"}
ROLES = {"drafter", "critic", "executor", "reviewer", "verifier"}
AGENTS = {"claude", "codex"}
MODELS = {"opus", "sonnet", "haiku", "gpt-5.x"}
METHODS = {"tests", "review", "build", "none"}

REQUIRED_TOP_KEYS = (
    "schema_version", "shape", "difficulty", "roster", "coordination",
    "verify", "escalate",
)

# The dominant corruption: a number, then a comma, then ANOTHER bare number
# (optionally more), where the grammar expects a key or a `}`. The training f32
# bug emitted e.g.  "difficulty":0.2,-0.79,-0.13,"rationale":...  — the stray
# numbers after the first value break JSON. We strip the stray run.
#
# Match a JSON number value followed by one-or-more `, <number>` runs that are
# then followed by a comma+string-key or a closing brace. We keep the FIRST
# number and drop the rest of the numeric run.
#   group(1) = the kept number   group(2) = the trailing `,"` or `}` boundary
_CORRUPT_FLOAT = re.compile(
    r"""(-?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)        # 1: first (kept) number
        (?:\s*,\s*-?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)+ # one+ stray ", number" runs
        (\s*(?:,\s*"|\}))                          # 2: real boundary that follows
    """,
    re.VERBOSE,
)


def _extract_json_blob(raw):
    """Leniently pull the JSON object out of a model reply.

    Handles prose before/after and ```json code fences. Returns the substring
    from the first `{` to the last `}`, or None if there's no brace pair.
    """
    text = raw.strip()
    # Drop a leading/trailing code fence if present (```json ... ```).
    fence = re.search(r"```(?:json)?\s*(.*?)```", text, re.S | re.I)
    if fence:
        text = fence.group(1).strip()
    start, end = text.find("{"), text.rfind("}")
    if start == -1 or end <= start:
        return None
    return text[start : end + 1]


def _repair_corrupt_floats(blob):
    """Drop stray numbers from the corrupt-float pattern. Conservative: only used
    when the blob fails to parse as-is, and we re-validate parsing afterward."""
    return _CORRUPT_FLOAT.sub(lambda m: m.group(1) + m.group(2), blob)


def _try_parse(blob):
    try:
        obj = json.loads(blob)
        return obj if isinstance(obj, dict) else None
    except Exception:
        return None


def _validate(plan, expected_shape, issues):
    """Append every enum/structural violation to `issues`. Returns nothing —
    the caller derives validity from whether `issues` is empty."""
    # Structural completeness: required top-level keys.
    for k in REQUIRED_TOP_KEYS:
        if k not in plan:
            issues.append(f"missing top-level key '{k}'")

    # shape enum (flag, never rewrite — caller may fall back to keyword router).
    shape = str(plan.get("shape", "")).strip().lower()
    if shape not in SHAPES:
        issues.append(f"hallucinated shape '{plan.get('shape')}' not in {sorted(SHAPES)}")
    if expected_shape and shape and shape != expected_shape.lower():
        issues.append(f"shape '{shape}' != expected '{expected_shape}'")

    # roster: non-empty list, each row's enums valid.
    roster = plan.get("roster")
    if not isinstance(roster, list) or not roster:
        issues.append("roster missing or empty")
        roles = []
    else:
        roles = [str(r.get("role", "")).lower() for r in roster]
        for r in roster:
            role, agent, model = (
                str(r.get("role", "")).lower(),
                str(r.get("agent", "")).lower(),
                str(r.get("model", "")).lower(),
            )
            if role not in ROLES:
                issues.append(f"hallucinated role '{r.get('role')}'")
            if agent not in AGENTS:
                issues.append(f"hallucinated agent '{r.get('agent')}'")
            if model not in MODELS:
                issues.append(f"hallucinated model '{r.get('model')}'")

    # coordination.order must match the roster roles in order.
    coord = plan.get("coordination") or {}
    order = coord.get("order") if isinstance(coord, dict) else None
    if not isinstance(order, list) or not order:
        issues.append("coordination.order missing or empty")
    elif [str(x).lower() for x in order] != roles:
        issues.append("coordination.order does not match roster roles")

    # verify.method enum.
    verify = plan.get("verify") or {}
    method = str(verify.get("method", "")).lower() if isinstance(verify, dict) else ""
    if method not in METHODS:
        issues.append(f"hallucinated verify.method '{verify.get('method') if isinstance(verify, dict) else verify}'")

    # escalate.to.model must be present (policy escalates to opus).
    esc = plan.get("escalate") or {}
    to = esc.get("to") if isinstance(esc, dict) else None
    if not isinstance(to, dict) or not to.get("model"):
        issues.append("escalate.to.model missing")


def validate_and_repair(raw_text, expected_shape=None):
    """Extract, (conditionally) repair, parse, and validate a plan reply.

    Returns {"plan": dict|None, "repaired": bool, "issues": [str], "valid": bool}.
    `valid` is True only if the plan parsed AND has no enum/structural issues.
    A hallucinated shape is reported via `issues` (and `valid=False`) but the
    parsed plan is still returned so the caller can inspect it.
    """
    issues = []
    repaired = False

    blob = _extract_json_blob(raw_text)
    if blob is None:
        return {"plan": None, "repaired": False,
                "issues": ["no JSON object found in reply"], "valid": False}

    plan = _try_parse(blob)
    if plan is None:
        # Only now attempt the corrupt-float repair, and only accept it if it
        # makes the blob parse. This keeps the repair from ever changing a
        # plan that was already valid.
        fixed = _repair_corrupt_floats(blob)
        if fixed != blob:
            candidate = _try_parse(fixed)
            if candidate is not None:
                plan, repaired = candidate, True
                issues.append("repaired corrupt-float pattern (stray numbers dropped)")
    if plan is None:
        return {"plan": None, "repaired": repaired,
                "issues": issues + ["unparseable JSON (no safe repair)"], "valid": False}

    _validate(plan, expected_shape, issues)
    # Validity ignores the informational "repaired ..." note: it's not a defect.
    hard_issues = [i for i in issues if not i.startswith("repaired ")]
    return {"plan": plan, "repaired": repaired, "issues": issues, "valid": not hard_issues}


# --------------------------------------------------------------------------- #
# Self-check: run `python plan_guard.py` — prints PASS/FAIL, exits non-zero on  #
# failure. Four hard-coded raw strings cover the confirmed failure modes.       #
# --------------------------------------------------------------------------- #
if __name__ == "__main__":
    import sys

    # A well-formed solo plan (matches the dataset policy for solo).
    CLEAN = json.dumps({
        "schema_version": 1, "shape": "solo", "difficulty": 0.85,
        "rationale": "algorithmic difficulty: one strong model implements",
        "risks": ["correctness-critical"],
        "roster": [
            {"role": "executor", "agent": "claude", "model": "opus", "why": "strong"},
            {"role": "verifier", "agent": "codex", "model": "gpt-5.x", "why": "tests"},
        ],
        "coordination": {"order": ["executor", "verifier"], "parallelizable": False,
                         "handoff": "criteria"},
        "verify": {"required": True, "method": "tests", "agent": "codex"},
        "escalate": {"if": ["failed_verification"], "to": {"agent": "claude", "model": "opus"}},
    })

    # Corrupt-float: a stray run of bare numbers after the difficulty value.
    CORRUPT = (
        '{"schema_version":1,"shape":"tiered",'
        '"difficulty":0.2000000024587651,-0.7999999523208747,-0.13,'
        '"rationale":"mechanical volume","risks":["large blast radius"],'
        '"roster":[{"role":"drafter","agent":"claude","model":"sonnet","why":"plan"},'
        '{"role":"executor","agent":"claude","model":"haiku","why":"bulk"},'
        '{"role":"reviewer","agent":"codex","model":"gpt-5.x","why":"spot"},'
        '{"role":"verifier","agent":"codex","model":"gpt-5.x","why":"tests"}],'
        '"coordination":{"order":["drafter","executor","reviewer","verifier"],'
        '"parallelizable":false,"handoff":"x"},'
        '"verify":{"required":true,"method":"tests","agent":"codex"},'
        '"escalate":{"if":["failed_verification"],"to":{"agent":"claude","model":"opus"}}}'
    )

    # Hallucinated shape enum: "codemod" is not in the allowed set.
    HALLUCINATED = CLEAN.replace('"shape": "solo"', '"shape": "codemod"')

    # Truncated reply: cut off mid-roster, no safe repair possible.
    TRUNCATED = (
        '{"schema_version":1,"shape":"debate","difficulty":0.6,'
        '"rationale":"architectural ambiguity","roster":[{"role":"drafter",'
    )

    failed = 0

    def check(name, cond):
        global failed
        status = "PASS" if cond else "FAIL"
        if not cond:
            failed += 1
        print(f"  [{status}] {name}")

    print("plan_guard self-check:")

    r = validate_and_repair(CLEAN, expected_shape="solo")
    check("clean plan is valid", r["valid"] and not r["repaired"])
    check("clean plan has no issues", r["issues"] == [])

    r = validate_and_repair(CORRUPT, expected_shape="tiered")
    check("corrupt-float repaired", r["repaired"])
    check("corrupt-float now valid", r["valid"])
    check("corrupt-float difficulty kept first number",
          r["plan"] is not None and abs(r["plan"]["difficulty"] - 0.2000000024587651) < 1e-9)

    r = validate_and_repair(HALLUCINATED, expected_shape="solo")
    check("hallucinated shape is flagged invalid", not r["valid"])
    check("hallucinated shape NOT silently rewritten",
          r["plan"] is not None and r["plan"]["shape"] == "codemod")
    check("hallucinated shape issue mentions 'codemod'",
          any("codemod" in i for i in r["issues"]))

    r = validate_and_repair(TRUNCATED, expected_shape="debate")
    check("truncated reply is unparseable", r["plan"] is None and not r["valid"])

    # A clean plan whose blob is wrapped in prose + a code fence still parses.
    r = validate_and_repair("Sure, here is the plan:\n```json\n" + CLEAN + "\n```")
    check("fenced/prose-wrapped clean plan parses & is valid", r["valid"])

    print("PASS" if failed == 0 else f"FAIL ({failed} check(s) failed)")
    sys.exit(1 if failed else 0)
