#!/usr/bin/env python3
"""P4-C A/A launch supervisor (fail-closed six-attempt launcher).

Wraps the exact frozen pilot loop from P4C-AA-MANIFEST-v2.md. Unlike the bare
shell loop, this supervisor:

* verifies experiment-wide pinned identities (task definitions, fixtures,
  prompts, suites, variant, template, frozen wrapper, bundled binary, TLS CA
  bundle, evaluator scripts, preflight digest pins) before EVERY slot;
* refuses to reuse an existing queue root (fresh, previously unused root per
  attempt);
* after each Queue invocation inspects BOTH the process exit status and the
  structured queue row (state + classification) plus the graded result
  evidence, and applies a predeclared stop/continue decision table;
* never automatically retries, refills, or replaces an attempt; an
  interrupted or ambiguous slot stays sealed and stops the experiment;
* preserves all evidence: stop-on-invalid never deletes or overwrites
  earlier attempt directories;
* records labeled digests (wrapper vs. bundled binary) in a supervisor log,
  addressing the metadata limitation that runtime_binary_digest records the
  wrapper digest.

This is an experiment-launch control only: no registry, scheduler, database,
or evaluation framework. It reuses the existing P3 queue unchanged.

Usage:
  python3 p4c_aa_supervise.py --queue-base DIR [--identity FILE] [--dry-run]

The key file is read at run time and never copied, printed, or logged.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sqlite3
import sys
import sysconfig
import uuid
from typing import Any

REPO = Path(__file__).resolve().parent
EVAL_DIR = REPO / "tools" / "eval"
E2E_QUEUE = EVAL_DIR / "e2e_queue.py"

sys.path.insert(0, str(EVAL_DIR))
from offline_grader import snapshot_paths  # grader-owned snapshot discovery

# Supervisor's own queue subprocesses (enqueue/start) are bounded: a hung
# queue must stop the experiment, not hang it. Generous vs. the pinned
# attempt_timeout (1800 s); evidence is preserved on timeout.
SUPERVISOR_SUBPROCESS_TIMEOUT = 3600

# Grader outcome categories (offline_grader.py:296-311).
GRADER_CATEGORIES = {"completed", "provider_blocked", "cancelled", "timed_out",
                     "runtime_failed", "unknown", "incomplete"}
# Queue-level classifications (e2e_queue.py transition sites).
QUEUE_CLASSIFICATIONS = {
    "configuration_blocked", "confinement_unverified", "submission_uncertain",
    "evidence_corrupt", "evidence_ambiguous", "acceptance_unavailable",
    "interrupted", "infrastructure_failed",
}

# Predeclared stop/continue decision table (Task 2). Keys are (state,
# classification) pairs as produced by e2e_queue.py; grader categories with
# no dedicated queue classification arrive as (completed|failed, classification).
# measured code_correctness == False is a valid functional failure (category
# "completed" with metrics.code_correctness False) and MUST continue.
DECISION: dict[tuple[str, str], dict[str, Any]] = {}
for _state, _cls, _verdict, _reason in [
    # --- valid, completed evaluations ---
    ("completed", "completed", "continue",
     "valid graded attempt; measured code_correctness decides functional pass/fail"),
    ("failed", "runtime_failed", "stop",
     "pipeline failed; measured functional failure is only code_correctness=False "
     "under category=completed, not a runtime failure"),
    # --- agent-level escalation/timeout with attributable evidence ---
    ("blocked", "cancelled", "stop",
     "agent-level escalation/cancel with attributable terminal evidence"),
    ("blocked", "timed_out", "stop",
     "attempt deadline or agent timeout"),
    ("blocked", "unknown", "stop",
     "ambiguous terminal state; adjudicate manually, never resubmit"),
    ("blocked", "incomplete", "stop",
     "incomplete workflow evidence"),
    # --- provider blockage ---
    ("blocked", "provider_blocked", "stop",
     "provider blockage; no retry"),
    # --- confinement failure (before any chat submission) ---
    ("blocked", "confinement_unverified", "stop",
     "sandbox admission not verified; experiment-contaminating"),
    # --- configuration mismatch ---
    ("blocked", "configuration_blocked", "stop",
     "effective configuration outside experiment pins"),
    # --- missing or invalid grading evidence ---
    ("blocked", "evidence_corrupt", "stop",
     "grading evidence missing or malformed"),
    ("blocked", "evidence_ambiguous", "stop",
     "multiple or ambiguous result packages"),
    ("blocked", "acceptance_unavailable", "stop",
     "acceptance measurement unavailable"),
    # --- interrupted / uncertain ---
    ("interrupted", "interrupted", "stop",
     "no terminal evidence; supervisor must never resubmit"),
    ("blocked", "submission_uncertain", "stop",
     "chat POST outcome uncertain; resubmission is forbidden"),
    # --- infrastructure failure ---
    ("failed", "infrastructure_failed", "stop",
     "unexpected local harness failure"),
]:
    DECISION[(_state, _cls)] = {"verdict": _verdict, "reason": _reason}
# Any (state, classification) outside this table is STOP with reason
# "unexpected or ambiguous attempt state" — fail-closed by default.


def sha_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical(value: Any) -> bytes:
    """Byte-identical canonicalization shared with tools/eval/e2e_queue.py."""
    return json.dumps(value, sort_keys=True, ensure_ascii=False,
                      separators=(",", ":")).encode()


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def tree_digest(root: Path) -> str:
    """Same digest scheme as e2e_queue.tree_digest, without importing it."""
    if not root.is_dir() or root.is_symlink():
        raise ValueError(f"unsafe directory: {root}")
    aggregate = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.is_symlink() or not (path.is_dir() or path.is_file()):
            raise ValueError(f"unsupported entry: {path}")
        if path.is_file():
            rel = path.relative_to(root).as_posix()
            if any(part in ("", ".", "..") for part in rel.split("/")):
                raise ValueError(f"unsafe path: {rel}")
            data = path.read_bytes()
            aggregate.update(f"{len(rel.encode())}\x1f{rel}\x1f{len(data)}\x1e".encode())
            aggregate.update(data)
    return aggregate.hexdigest()


class IdentityError(Exception):
    """Experiment-wide identity mismatch; the pilot must not proceed."""


class Identity:
    """Experiment-wide pinned identities, verified before every slot."""

    def __init__(self, path: Path):
        raw = json.loads(path.read_text())
        self.path = path
        self.raw = raw
        self.benchmark_root = Path(raw["benchmark_root"])
        repo = REPO
        order = raw["order"]
        expected = ["b2-inventory-merge", "b3-rate-limiter"] * 3
        if order != expected:
            raise IdentityError(f"order {order} != frozen b2,b3 interleaving")
        self.order = order
        self.prepared = {}
        for task_id, pin in raw["prepared"].items():
            entry = dict(pin)
            for key in ("definition", "fixture_dir", "prompt_path", "acceptance_spec"):
                entry["_" + key] = self.benchmark_root / pin[key]
            self.prepared[task_id] = entry
        self.evaluators = {name: EVAL_DIR / name
                           for name in raw["evaluators"]}
        self.evaluator_pins = raw["evaluators"]
        self.template_dir = repo / raw["template"]["dir"]
        self.template_pins = raw["template"]["files"]
        self.preflight_path = self.benchmark_root / raw["preflight"]["path"]
        self.preflight_sha = raw["preflight"]["sha256"]
        self.variant_path = Path(raw["variant"]["path"])
        self.variant_pin = raw["variant"]
        baseline = raw["baseline"]
        freeze = Path(baseline["freeze_dir"])
        self.binary_path = freeze / baseline["bundled_binary"]["path"]
        self.binary_sha = baseline["bundled_binary"]["sha256"]
        self.wrapper_path = freeze / baseline["wrapper"]["path"]
        self.wrapper_sha = baseline["wrapper"]["sha256"]
        self.ca_path = freeze / baseline["ca_bundle"]["path"]
        self.ca_sha = baseline["ca_bundle"]["sha256"]
        self.parameters = raw["parameters"]

    def verify(self, label: str) -> None:
        """Re-verify every experiment-wide identity; raise IdentityError on drift."""
        problems: list[str] = []

        def check(name: str, actual: str, expected: str) -> None:
            if actual != expected:
                problems.append(f"{name}: {actual} != pinned {expected}")

        for task_id, pin in self.prepared.items():
            definition = pin["_definition"]
            check(f"{task_id}:definition", sha_file(definition), pin["definition_sha256"])
            obj = json.loads(definition.read_text())
            for key, path_key in (("fixture_tree_digest", "_fixture_dir"),
                                  ("baseline_digest", "_fixture_dir"),
                                  ("prompt_digest", "_prompt_path"),
                                  ("acceptance_spec_digest", "_acceptance_spec"),
                                  ("variant_digest", None), ("suite_digest", None)):
                if path_key:
                    pass
            # definition-internal pins must match the identity file exactly
            for key in ("task_id", "version", "fixture_tree_digest", "baseline_digest",
                        "prompt_digest", "acceptance_spec_digest", "suite_digest",
                        "suite_version", "variant_id", "variant_digest",
                        "template_digest"):
                check(f"{task_id}:definition:{key}", str(obj.get(key)),
                      str(pin.get(key)))
            check(f"{task_id}:fixture", tree_digest(pin["_fixture_dir"]),
                  pin["fixture_tree_digest"])
            check(f"{task_id}:prompt", sha_file(pin["_prompt_path"]), pin["prompt_digest"])
            check(f"{task_id}:acceptance_spec", sha_file(pin["_acceptance_spec"]),
                  pin["acceptance_spec_digest"])

        check("variant", sha_file(self.variant_path), self.variant_pin["sha256"])
        variant = json.loads(self.variant_path.read_text())
        if variant.get("id") != self.variant_pin["id"]:
            problems.append(f"variant id drifted: {variant.get('id')!r}")
        if variant.get("models") != self.variant_pin["models"]:
            problems.append("variant models drifted (a readable name is not identity)")
        if variant.get("base_url") != self.variant_pin["base_url"]:
            problems.append("variant base_url drifted")

        check("bundled_binary", sha_file(self.binary_path), self.binary_sha)
        check("wrapper", sha_file(self.wrapper_path), self.wrapper_sha)
        check("ca_bundle", sha_file(self.ca_path), self.ca_sha)

        for name, path in self.evaluators.items():
            check(f"evaluator:{name}", sha_file(path), self.evaluator_pins[name])

        check("template:team.json", sha_file(self.template_dir / "team.json"),
              self.template_pins["team.json"])
        for rel, pin in self.template_pins.items():
            if rel != "team.json":
                check(f"template:{rel}", sha_file(self.template_dir / rel), pin)
        if tree_digest(self.template_dir) != list(self.prepared.values())[0]["template_digest"]:
            problems.append("template tree digest drifted from prepared definitions")

        check("preflight", sha_file(self.preflight_path), self.preflight_sha)
        preflight = json.loads(self.preflight_path.read_text())
        for rel, expected_digest in preflight["digests"].items():
            target = self.benchmark_root / rel
            if not target.is_file():
                problems.append(f"preflight pin missing: {rel}")
            else:
                check(f"preflight:{rel}", sha_file(target), expected_digest)

        if problems:
            detail = "; ".join(problems[:12])
            raise IdentityError(f"identity verification failed at {label}: {detail}")

    def snapshot(self) -> dict[str, Any]:
        return {
            "identity_file": str(self.path),
            "order": list(self.order),
            "bundled_binary_sha256": self.binary_sha,
            "wrapper_sha256": self.wrapper_sha,
            "ca_bundle_sha256": self.ca_sha,
            "variant_sha256": self.variant_pin["sha256"],
            "definition_sha256": {t: p["definition_sha256"]
                                  for t, p in self.prepared.items()},
            "evaluator_sha256": dict(self.evaluator_pins),
            "template_files_sha256": dict(self.template_pins),
            "parameters": dict(self.parameters),
            "hosts": {"python": sysconfig.get_python_version(),
                      "bubblewrap": self.raw.get("hosts", {}).get("bubblewrap")},
        }


class Log:
    def __init__(self, path: Path | None):
        self.path = path
        self.entries: list[dict[str, Any]] = []
        if path is not None:
            path.parent.mkdir(parents=True, exist_ok=True)

    def record(self, event: str, **fields: Any) -> None:
        entry = {"at": datetime.now(timezone.utc).isoformat(), "event": event, **fields}
        self.entries.append(entry)
        line = json.dumps(entry, ensure_ascii=False, sort_keys=True)
        print(line, flush=True)
        if self.path is not None:
            with self.path.open("a") as stream:
                stream.write(line + "\n")


def evaluate_outcome(state: str, classification: str | None,
                     code_correctness: bool | None) -> dict[str, Any]:
    """Apply the predeclared decision table to a structured attempt outcome."""
    key = (state, classification)
    entry = DECISION.get(key)
    if entry is None:
        return {"verdict": "stop", "reason": "unexpected or ambiguous attempt state",
                "code_correctness": code_correctness}
    return {**entry, "code_correctness": code_correctness}


GRADER_METRIC_STATUSES = {"measured", "blocked", "not_measured", "not_applicable",
                          "measurement_error"}


def validate_result(result: Any) -> tuple[str, str | None, bool | None]:
    """Strictly validate one structured grading result package.

    Returns (outcome_category, code_status, code_value). A completed measured
    correctness requires a REAL JSON boolean (True/False) — not null, number,
    string, list, object, or missing. Reuses the existing grader's semantics:
    categories must come from offline_grader.py's taxonomy and metric status
    from its STATUSES set; anything else is invalid evidence.
    """
    if not isinstance(result, dict):
        raise ValueError("result.json top level is not an object")
    if result.get("schema_version") != "1":
        raise ValueError(f"unsupported result schema_version: {result.get('schema_version')!r}")
    outcome = result.get("outcome")
    if not isinstance(outcome, dict):
        raise ValueError("outcome is not an object")
    category = outcome.get("category")
    if category not in GRADER_CATEGORIES:
        raise ValueError(f"unknown outcome category: {category!r}")
    metrics = result.get("metrics")
    if not isinstance(metrics, dict):
        raise ValueError("metrics is not an object")
    code = metrics.get("code_correctness")
    if not isinstance(code, dict) or code.get("status") not in GRADER_METRIC_STATUSES:
        raise ValueError(f"code_correctness status invalid: "
                         f"{code.get('status') if isinstance(code, dict) else code!r}")
    status, value = code["status"], code.get("value")
    if status == "measured":
        # A measured boolean must be a real JSON boolean; bool is not a
        # subclass of int here because we check identity against True/False
        # FIRST and reject everything else (0, 1, "true", [..], None).
        if value is not True and value is not False:
            raise ValueError(f"measured code_correctness value is not a JSON boolean: {value!r}")
    elif "value" in code:
        raise ValueError(f"non-measured code_correctness carries a value ({code['value']!r})")
    return category, status, (value if status == "measured" else None)


def check_acceptance_attribution(run_dir: Path) -> None:
    """Verify acceptance attribution: each r1/r2 record's issue_id and run_id
    must equal those of the snapshot manifest it names. The grader binds
    snapshot_id/manifest_digest; issue_id/run_id must be consistent too, or
    the acceptance result is attributed to the wrong workflow. Historical
    valid evidence (issue_id/run_id identical on both sides) is unaffected.
    """
    snapshots: dict[str, dict[str, Any]] = {}
    for manifest_path in snapshot_paths(run_dir / "source"):
        try:
            manifest = json.loads(manifest_path.read_text())
        except (OSError, ValueError):
            continue
        if manifest.get("snapshot_id"):
            snapshots[manifest["snapshot_id"]] = manifest
    acceptance_dir = run_dir / "acceptance"
    for record_path in sorted(acceptance_dir.glob("r[12].json")):
        try:
            record = json.loads(record_path.read_text())
        except (OSError, ValueError) as exc:
            raise ValueError(f"acceptance record unreadable: {record_path.name}: {exc}")
        if not isinstance(record, dict):
            raise ValueError(f"acceptance record is not an object: {record_path.name}")
        snap = snapshots.get(record.get("snapshot_id"))
        if snap is None:
            # The grader rejects unknown snapshot_ids itself; nothing extra
            # to enforce here for records without a matching snapshot.
            continue
        for field in ("issue_id", "run_id"):
            if record.get(field) != snap.get(field):
                raise ValueError(
                    f"acceptance attribution mismatch in {record_path.name}: "
                    f"record {field}={record.get(field)!r} != snapshot {field}={snap.get(field)!r}")


QUEUE_COMMAND_RUNNER = None  # test seam: None → real e2e_queue.py subprocess


def run_queue_command(args: list[str], env: dict[str, str]) -> subprocess.CompletedProcess:
    """Invoke the frozen queue (enqueue/start) or a test-injected stub."""
    command = ["python3", "-B", str(E2E_QUEUE), *args]
    if QUEUE_COMMAND_RUNNER is not None:
        return QUEUE_COMMAND_RUNNER(command)
    return subprocess.run(command, capture_output=True, text=True, env=env,
                          timeout=SUPERVISOR_SUBPROCESS_TIMEOUT)


def resolve_attempt_root(queue_root: Path, row: dict[str, Any]) -> tuple[Path, Path | None]:
    """Resolve the attempt root and the graded-run pointer from a queue row.

    Queue semantics (e2e_queue.py): `artifact_dir` is set to the attempt root
    at 'preparing' and REWRITTEN to the graded-run directory
    (attempts/<id>/evaluation/runs/<dir>) at the terminal transition. The
    attempt root is therefore owned by `attempt_id`
    (`<queue_root>/attempts/<attempt_id>`), never inferred from artifact_dir.
    Returns (attempt_root, run_dir_or_None); raises ValueError on
    inconsistent or escaping pointers.
    """
    attempt_id = row.get("attempt_id")
    if not attempt_id or not isinstance(attempt_id, str):
        raise ValueError("missing attempt_id in queue row")
    try:
        uuid.UUID(attempt_id)
    except ValueError as exc:
        raise ValueError(f"malformed attempt_id: {attempt_id!r}") from exc
    attempt_root = (queue_root / "attempts" / attempt_id).resolve()
    if attempt_root.parent != (queue_root / "attempts").resolve():
        raise ValueError(f"attempt root escapes queue root: {attempt_root}")
    raw_artifact = row.get("artifact_dir")
    run_dir: Path | None = None
    if raw_artifact:
        pointer = Path(str(raw_artifact))
        if pointer.is_absolute():
            resolved = pointer.resolve(strict=False)
        else:
            resolved = (queue_root / pointer).resolve(strict=False)
        # Legal artifact_dir values: the attempt root (pre-terminal) or a
        # graded-run directory inside it. Anything else is inconsistent.
        if resolved != attempt_root and not resolved.is_relative_to(attempt_root / "evaluation" / "runs"):
            raise ValueError(f"artifact_dir is neither the attempt root nor a "
                             f"graded-run directory inside it: {raw_artifact!r}")
        if resolved == attempt_root:
            run_dir = None
        else:
            # Validate the graded-run pointer: exact two-level layout
            # evaluation/runs/<run>, named in the queue row, inside this attempt.
            if (resolved.parent.name != "runs" or
                    resolved.parent.parent.name != "evaluation" or
                    resolved.parent.parent.parent != attempt_root):
                raise ValueError(f"graded-run pointer is inconsistent: {raw_artifact}")
            if not resolved.is_dir():
                raise ValueError(f"graded-run directory missing: {resolved}")
            run_dir = resolved
    return attempt_root, run_dir


def read_grading(run_dir: Path) -> dict[str, Any]:
    """Read the graded result evidence for an attempt, tolerating early
    admission failures with no result package (never an inference of success).

    `run_dir` is the graded-run directory (queue `artifact_dir` after the
    terminal transition) or the attempt's evaluation/runs directory scanned
    by the caller.
    """
    out: dict[str, Any] = {"result_paths": [], "code_correctness": None,
                           "outcome_category": None, "result_path": None}
    if (run_dir / "result.json").is_file():
        # run_dir is itself one graded-run directory.
        result_paths = [run_dir / "result.json"]
    else:
        runs = run_dir / "evaluation" / "runs"
        result_paths = sorted(runs.glob("*/result.json")) if runs.is_dir() else []

    out["result_paths"] = [str(p) for p in result_paths]
    if len(result_paths) != 1:
        return out
    out["result_path"] = str(result_paths[0])
    try:
        result = json.loads(result_paths[0].read_text())
    except (OSError, ValueError):
        return out
    if not isinstance(result, dict):
        return out  # malformed top-level structure: no evidence inferred
    outcome = result.get("outcome", {})
    if not isinstance(outcome, dict):
        outcome = {}
    out["outcome_category"] = outcome.get("category")
    metrics = result.get("metrics", {})
    code = metrics.get("code_correctness", {}) if isinstance(metrics, dict) else {}
    if not isinstance(code, dict):
        code = {}
    out["code_status"] = code.get("status")
    if code.get("status") == "measured":
        out["code_correctness"] = code.get("value")
    return out


def require_identity_evidence(identity: Identity, attempt_root: Path, row: dict[str, Any],
                              task_id: str, log: Log, slot_label: str
                               ) -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
    """Read and enforce attempt identity evidence for a measured attempt.

    Returns (metadata, effective_team) or (None, None) after recording a stop.
    Enforces: readable metadata and effective team, attempt-attempt identity
    (task, pinned digests), pinned models, required sandbox mode, effective
    member models/providers within pins, and cross-slot configuration equality.
    """
    metadata_path = attempt_root / "metadata.json"
    effective_team_path = attempt_root / "effective-team.json"
    if not metadata_path.is_file():
        log.record("stop", slot=slot_label, classification="evidence_corrupt",
                   reason=f"attempt metadata.json missing: {metadata_path}")
        return None, 10
    try:
        metadata = json.loads(metadata_path.read_text())
        if not isinstance(metadata, dict):
            raise ValueError("metadata is not an object")
    except (OSError, ValueError) as exc:
        log.record("stop", slot=slot_label, classification="evidence_corrupt",
                   reason=f"attempt metadata unreadable: {exc}")
        return None, 10
    # Attempt identity: metadata must belong to the attempt the queue row
    # names, for this task, with the pinned input identities (stable
    # configuration fields; attempt-specific workflow/team ids are excluded).
    stable_fields = (("task_id", task_id),
                     ("attempt_id", row.get("attempt_id")),
                     ("baseline_digest", identity.prepared[task_id]["baseline_digest"]),
                     ("prompt_digest", identity.prepared[task_id]["prompt_digest"]),
                     ("acceptance_spec_digest", identity.prepared[task_id]["acceptance_spec_digest"]),
                     ("suite_digest", identity.prepared[task_id]["suite_digest"]),
                     ("suite_version", identity.prepared[task_id]["suite_version"]),
                     ("variant_id", identity.prepared[task_id]["variant_id"]),
                     ("variant_digest", identity.prepared[task_id]["variant_digest"]),
                     ("template_digest", identity.prepared[task_id]["template_digest"]))
    mismatches = [f"{field}: attempt metadata {metadata.get(field)!r} != expected {expected!r}"
                  for field, expected in stable_fields
                  if metadata.get(field) != expected]
    if mismatches:
        log.record("stop", slot=slot_label, classification="configuration_mismatch",
                   reason="attempt metadata inconsistent with pinned identity: "
                          + "; ".join(mismatches))
        return None, 3
    log.record("runtime_identity", slot=slot_label,
               queue_runtime_binary_digest=metadata.get("runtime_binary_digest"),
               queue_runtime_binary_digest_meaning="wrapper (launcher) digest",
               bundled_binary_sha256=identity.binary_sha,
               wrapper_sha256=identity.wrapper_sha,
               ca_bundle_sha256=identity.ca_sha,
               configuration_fingerprint=metadata.get("configuration_fingerprint"))
    # Effective models must equal the pinned variant.
    if metadata.get("models") != identity.variant_pin["models"]:
        log.record("stop", slot=slot_label, classification="configuration_mismatch",
                   reason="effective models differ from pinned variant")
        return None, 3
    # Effective Team evidence must be readable and consistent.
    if not effective_team_path.is_file():
        log.record("stop", slot=slot_label, classification="confinement_unverified",
                   reason=f"effective team evidence missing: {effective_team_path}")
        return None, 10
    try:
        effective = json.loads(effective_team_path.read_text())
        if not isinstance(effective, dict):
            raise ValueError("effective team is not an object")
    except (OSError, ValueError) as exc:
        log.record("stop", slot=slot_label, classification="confinement_unverified",
                   reason=f"effective team evidence unreadable: {exc}")
        return None, 10
    if (effective.get("sandbox_mode") !=
            identity.raw["admission"]["required_sandbox_mode"]):
        log.record("stop", slot=slot_label, classification="confinement_mismatch",
                   reason=f"effective sandbox_mode is "
                          f"{effective.get('sandbox_mode')!r}, not the required "
                          f"{identity.raw['admission']['required_sandbox_mode']!r}")
        return None, 3
    # Every effective member must run a pinned tier model at the pinned
    # provider; the queue re-derives members into metadata.effective_members.
    derived_members = metadata.get("effective_members")
    if not isinstance(derived_members, list) or not derived_members:
        log.record("stop", slot=slot_label, classification="configuration_mismatch",
                   reason="effective team member configuration missing")
        return None, 3
    expected_models = identity.variant_pin["models"]
    expected_provider = identity.variant_pin["provider_host"]
    for member in derived_members:
        if (not isinstance(member, dict) or
                member.get("model") not in set(expected_models.values()) or
                member.get("provider") != expected_provider):
            log.record("stop", slot=slot_label, classification="configuration_mismatch",
                       reason=f"effective member outside pinned variant: {member}")
            return None, 3
    # Cross-slot configuration equality: stable configuration fields must be
    # IDENTICAL across the six A/A slots; attempt-specific identifiers
    # (attempt_id, team_id, workflow_id, repo_path) are not compared.
    # effective_team_digest covers the whole effective team including the
    # unique per-attempt Team ID, so each A/A slot legitimately differs; it is
    # validated per-attempt below and excluded from cross-slot equality.
    comparable = {"effective_configuration_digest": metadata.get("effective_configuration_digest"),
                  "configuration_fingerprint": metadata.get("configuration_fingerprint")}
    if not all(isinstance(v, str) and v for v in comparable.values()):
        log.record("stop", slot=slot_label, classification="configuration_mismatch",
                   reason="effective configuration digests missing from attempt metadata")
        return None, 3
    # Per-attempt consistency: effective_team_digest must match the attempt's
    # own effective-team.json (unique Team ID included).
    if metadata.get("effective_team_digest") != sha(canonical(effective)):
        log.record("stop", slot=slot_label, classification="configuration_mismatch",
                   reason="effective_team_digest does not match effective-team.json")
        return None, 3
    if _SEEN_SLOT_CONFIG and comparable != _SEEN_SLOT_CONFIG["ref"]:
        differing = [k for k in comparable if comparable[k] != _SEEN_SLOT_CONFIG["ref"].get(k)]
        log.record("stop", slot=slot_label, classification="configuration_mismatch",
                   reason=f"effective configuration differs from earlier slots: {differing}")
        return None, 3
    _SEEN_SLOT_CONFIG.update({"ref": comparable})
    return metadata, effective


# Cross-slot effective-configuration reference (six A/A slots must be equal).
_SEEN_SLOT_CONFIG: dict[str, dict[str, str]] = {}


def supervise(args: argparse.Namespace) -> int:
    identity = Identity(Path(args.identity))
    # Log only after we know the queue base is fresh (Log creates its parent).
    log: Log | None = None

    def record(event: str, **fields: Any) -> None:
        if log is None:
            print(json.dumps({"event": event, **fields}, ensure_ascii=False,
                             sort_keys=True), flush=True)
        else:
            log.record(event, **fields)

    snap = identity.snapshot()
    record("supervisor_start", experiment=snap["experiment"] if "experiment" in snap else "p4c-aa-baseline-v2",
           identity=snap, dry_run=args.dry_run)

    # 1. Initial identity verification (repeat before every slot below).
    try:
        identity.verify("launch")
    except (IdentityError, OSError, ValueError) as exc:
        record("stop", slot=None, classification="configuration_mismatch", reason=str(exc))
        return 3

    # 2. Fresh, previously unused queue base; no overwriting existing roots.
    queue_base = Path(args.queue_base).resolve()
    if queue_base.exists():
        record("stop", slot=None, classification="queue_base_exists",
               reason=f"queue base already exists: {queue_base} (refusing reuse)")
        return 3
    if args.dry_run:
        record("dry_run_complete", planned_order=list(identity.order))
        return 0
    queue_base.mkdir(parents=True, mode=0o700)
    log = Log(queue_base / "supervisor-log.jsonl")

    expected_order = identity.order
    for slot_index, task_id in enumerate(expected_order, start=1):
        slot_label = f"slot-{slot_index}-{task_id}"

        # 3. Identity re-verification before EVERY slot (experiment-wide pins:
        #    definitions, fixture/prompt/suite digests, variant + template
        #    identities, frozen wrapper + bundled binary + CA bundle, evaluator
        #    scripts, preflight pins).
        try:
            identity.verify(slot_label)
        except (IdentityError, OSError) as exc:
            log.record("stop", slot=slot_label, classification="configuration_mismatch",
                       reason=str(exc))
            return 3

        attempt_dir = queue_base / f"attempt-{slot_index}-{task_id}"
        if attempt_dir.exists() or (queue_base / f"attempt-{slot_index}-{task_id}").is_symlink():
            log.record("stop", slot=slot_label, classification="queue_base_exists",
                       reason=f"attempt directory already exists: {attempt_dir}")
            return 3

        definition = identity.prepared[task_id]["_definition"]
        env = dict(os.environ)
        env.setdefault("PYTHONHASHSEED", "0")

        # 4. Enqueue into the fresh queue root; stop on any failure. Bounded:
        #    a hung queue subprocess stops the experiment; evidence preserved.
        try:
            enqueue = run_queue_command(
                ["enqueue", "--queue", str(attempt_dir), str(definition)], env)
        except subprocess.TimeoutExpired:
            log.record("stop", slot=slot_label, classification="supervisor_timeout",
                       reason=f"enqueue subprocess exceeded {SUPERVISOR_SUBPROCESS_TIMEOUT}s; "
                              f"queue root preserved for investigation")
            return 4
        if enqueue.returncode != 0:
            log.record("stop", slot=slot_label, classification="enqueue_failed",
                       reason=f"enqueue exit {enqueue.returncode}: "
                              f"{(enqueue.stderr or enqueue.stdout).strip()[-500:]}")
            return 4

        # 5. Start (one attempt, no retries anywhere downstream). Bounded.
        try:
            start = run_queue_command(
                ["start",
                 "--queue", str(attempt_dir),
                 "--studio-bin", str(identity.wrapper_path),
                 "--key-file", str(Path(args.key_file)),
                 "--max-attempts", str(identity.parameters["max_attempts"]),
                 "--max-duration", str(identity.parameters["max_duration"]),
                 "--attempt-timeout", str(identity.parameters["attempt_timeout"]),
                 "--allow-model", identity.variant_pin["models"]["main"],
                 "--allow-provider", identity.variant_pin["provider_host"]],
                env)
        except subprocess.TimeoutExpired:
            log.record("stop", slot=slot_label, classification="supervisor_timeout",
                       reason=f"start subprocess exceeded {SUPERVISOR_SUBPROCESS_TIMEOUT}s "
                              f"(> attempt_timeout); queue root sealed, no resubmission")
            return 5
        # 5a. Process exit status is inspected, but exit 0 is NEVER sufficient.
        if start.returncode != 0:
            log.record("stop", slot=slot_label, classification="queue_start_failed",
                       reason=f"start exit {start.returncode}: "
                              f"{(start.stderr or start.stdout).strip()[-500:]}")
            return 5

        # 6. Structured state inspection: exit status AND queue state AND
        #    grading evidence are all required.
        import sqlite3
        db = attempt_dir / "queue.sqlite3"
        if not db.is_file():
            log.record("stop", slot=slot_label, classification="evidence_missing",
                       reason="queue database missing after successful start")
            return 5
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
            conn.row_factory = sqlite3.Row
            rows = [dict(r) for r in conn.execute(
                "SELECT * FROM tasks ORDER BY rowid")]
        if len(rows) != 1:
            log.record("stop", slot=slot_label, classification="evidence_ambiguous",
                       reason=f"expected exactly one queue row, found {len(rows)}")
            return 5
        row = rows[0]
        state, classification = row["state"], row["classification"]
        # 6a. Attempt identity: the queue's attempt_id is authoritative; the
        #     attempt root resolves from it, never from artifact_dir. The
        #     graded-run pointer (artifact_dir after a terminal transition)
        #     is validated against the attempt root.
        try:
            attempt_root, run_dir = resolve_attempt_root(attempt_dir, row)
        except ValueError as exc:
            log.record("stop", slot=slot_label, classification="evidence_corrupt",
                       reason=f"attempt identity inconsistent: {exc}")
            return 10
        metadata_path = attempt_root / "metadata.json"
        effective_team_path = attempt_root / "effective-team.json"
        sandbox_observed = None
        if effective_team_path.is_file():
            try:
                sandbox_observed = json.loads(effective_team_path.read_text()).get("sandbox_mode")
            except (OSError, ValueError):
                sandbox_observed = "unreadable"
        # 6b. Grading evidence: located under the attempt root's evaluation
        #     runs; when the queue recorded a graded-run pointer, exactly that
        #     run directory must carry the single result package.
        grading: dict[str, Any] = {"result_paths": [], "code_correctness": None,
                                   "outcome_category": None, "result_path": None}
        try:
            if run_dir is not None:
                grading = read_grading(run_dir)
                if not grading["result_path"]:
                    raise ValueError("graded-run pointer has no readable result package")
            else:
                grading = read_grading(attempt_root)
        except (OSError, ValueError) as exc:
            log.record("stop", slot=slot_label, classification="evidence_corrupt",
                       reason=f"graded evidence unreadable or ambiguous: {exc}")
            return 10
        log.record("attempt_outcome", slot=slot_label, state=state,
                   classification=classification, sandbox_mode=sandbox_observed,
                   code_correctness=grading.get("code_correctness"),
                   outcome_category=grading.get("outcome_category"),
                   result_path=grading.get("result_path"),
                   attempt_id=row.get("attempt_id"), workflow_id=row.get("workflow_id"))

        # 7. Identity evidence after the run. For any attempt that produced a
        #    graded result package (completed measured outcome), missing,
        #    unreadable or inconsistent evidence STOPS the experiment; the
        #    pinned comparisons are enforced, not merely logged. Attempts the
        #    queue itself blocked before measurement keep the queue's
        #    classification and are stopped by the decision table below.
        if grading.get("result_path") is not None:
            metadata, stop_code = require_identity_evidence(
                identity, attempt_root, row, task_id, log, slot_label)
            if metadata is None:
                return stop_code
        elif state == "completed":
            # A completed measurement without readable identity evidence
            # cannot be attributed; fail closed.
            log.record("stop", slot=slot_label, classification="evidence_corrupt",
                       reason="completed measurement lacks readable attempt "
                              "metadata or effective team evidence")
            return 10

        # 8. Predeclared decision table (exit status already checked above).
        # 8a. Strict measured-result validation: exactly one structured result
        #     package, valid grader semantics, and for completed measured
        #     correctness a real JSON boolean. Malformed, ambiguous or
        #     unsupported structures STOP; valid functional failures continue.
        if state == "completed":
            if grading.get("outcome_category") != "completed":
                log.record("stop", slot=slot_label, classification="evidence_missing",
                           reason=f"state completed without a valid graded result package "
                                  f"(category={grading.get('outcome_category')!r})",
                           completed_slots=slot_index,
                           note="all retained evidence preserved under " + str(queue_base))
                return 10
            if grading.get("code_status") != "measured":
                log.record("stop", slot=slot_label, classification="evidence_missing",
                           reason=f"completed outcome lacks measured code_correctness "
                                  f"(status={grading.get('code_status')!r})",
                           completed_slots=slot_index,
                           note="all retained evidence preserved under " + str(queue_base))
                return 10
            try:
                result = json.loads((attempt_root / grading["result_path"]).read_text())
                category, code_status, code_value = validate_result(result)
                if category != "completed" or code_status != "measured":
                    raise ValueError("validated result disagrees with scanned evidence")
            except (OSError, ValueError) as exc:
                log.record("stop", slot=slot_label, classification="evidence_corrupt",
                           reason=f"malformed grading result: {exc}",
                           completed_slots=slot_index,
                           note="all retained evidence preserved under " + str(queue_base))
                return 10
            # 8b. Acceptance attribution: issue_id/run_id on each r1/r2 record
            #     must match the snapshot manifest it names.
            try:
                check_acceptance_attribution(run_dir or attempt_root)
            except ValueError as exc:
                log.record("stop", slot=slot_label, classification="evidence_corrupt",
                           reason=str(exc),
                           completed_slots=slot_index,
                           note="all retained evidence preserved under " + str(queue_base))
                return 10
        decision = evaluate_outcome(state, classification, grading.get("code_correctness"))
        log.record("decision", slot=slot_label, verdict=decision["verdict"],
                   reason=decision["reason"],
                   code_correctness=decision.get("code_correctness"))
        if decision["verdict"] == "stop":
            log.record("stop", slot=slot_label, classification=classification,
                       reason=decision["reason"],
                       completed_slots=slot_index,
                       note="pilot aborted with fewer than six attempted slots; "
                            "all retained evidence preserved under " + str(queue_base))
            return 10
        # verdict continue → next slot; never refill or replace this slot.

    log.record("pilot_complete", slots=6, order=list(expected_order),
               note="all six slots produced valid, decision-table-continue outcomes")
    return 0


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--queue-base", required=True, type=Path,
                        help="fresh directory for the six attempt queue roots")
    parser.add_argument("--key-file", type=Path,
                        default=Path.home() / ".harness/agent-team-studio/apikey",
                        help="provider key file (read at run time; never copied)")
    parser.add_argument("--identity", type=Path,
                        default=Path("/home/liyufeng/p4b-benchmark/p4c-aa-identity.json"))
    parser.add_argument("--dry-run", action="store_true",
                        help="verify identities and plan only; no queue invocations")
    args = parser.parse_args()
    sys.exit(supervise(args))


if __name__ == "__main__":
    main()
