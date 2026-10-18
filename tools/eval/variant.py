"""P4-A variant manifest: capture, fingerprinting and verification.

A variant manifest is a plain JSON document that identifies one exact Coding
Team implementation + configuration combination so that repeated evaluations
are comparable.  It reuses the P3 queue's own digest primitives
(``e2e_queue.tree_digest`` and the baseline digest of a pinned fixture) so a
manifest can never disagree with a queue definition built from the same
inputs.

Manifest schema (``schema_version`` ``"1"``):

    variant_id            required nonempty string
    description           optional string
    base_variant          optional variant id this derives from
    source                {revision, dirty, status_digest, status_porcelain,
                           untracked_files, branch}
    runtime_binary        {path, sha256, source_provenance}
    team_template         {path, tree_digest, prompt_digests {member: sha256}}
    models                {strong, main, cheap} tier mapping
    base_url              credential-free provider origin (optional)
    scout_interval_secs   positive int (optional, default 3600)
    evaluation            {fixture_tree_digest, baseline_digest, prompt_digest,
                           suite_digest, acceptance_spec_digest} (all optional;
                           when present they must match a prepared task)
    created_at            ISO-8601 timestamp
    variant_digest        SHA-256 over the canonical bytes of the manifest
                          without this field (self digest)
    reconstructable       optional bool: whether the exact source and binary
                          can be recovered from what this manifest references

``variant_digest`` is a configuration identity: two manifests with the same
variant id but different effective content always produce different digests,
and identical content always produces the same digest regardless of path.
Secrets are rejected.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Any
from urllib.parse import urlsplit

REPO = Path(__file__).resolve().parents[2]

MANIFEST_VERSION = "1"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def stamp() -> str:
    from datetime import datetime, timezone
    return datetime.now(timezone.utc).isoformat()


def canonical(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()


def save_json(path: Path, value: Any) -> None:
    import os
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_bytes(json.dumps(value, indent=2, ensure_ascii=False, sort_keys=True).encode() + b"\n")
    os.replace(temporary, path)


def tree_digest(root: Path) -> str:
    from offline_grader import safe_rel
    if not root.is_dir() or root.is_symlink():
        raise ValueError(f"unsafe directory: {root}")
    aggregate = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.is_symlink() or not (path.is_dir() or path.is_file()):
            raise ValueError(f"unsupported directory entry: {path}")
        if path.is_file():
            rel = path.relative_to(root).as_posix()
            if not safe_rel(rel):
                raise ValueError(f"unsafe path: {rel}")
            data = path.read_bytes()
            aggregate.update(f"{len(rel.encode())}\x1f{rel}\x1f{len(data)}\x1e".encode())
            aggregate.update(data)
    return aggregate.hexdigest()


def validate_base_url(value: str) -> None:
    if not value:
        return
    parsed = urlsplit(value)
    if (parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username or
            parsed.password or parsed.query or parsed.fragment):
        raise ValueError("base_url must be a credential-free HTTP(S) origin")


def validate_variant_file(path: Path) -> dict[str, Any]:
    """Structural validation of a P3 variant.json file (credential-free)."""
    variant = json.loads(path.read_text())
    if not isinstance(variant, dict) or not isinstance(variant.get("models"), dict):
        raise ValueError("variant requires models")
    if set(variant) - {"id", "models", "base_url", "scout_interval_secs"}:
        raise ValueError("unsupported variant field")
    validate_tier_config(variant["models"], variant.get("base_url", ""),
                         variant.get("scout_interval_secs", 3600))
    if any(key.lower() in ("api_key", "token", "secret", "password") for key in variant):
        raise ValueError("variant must not contain credentials")
    return variant


def validate_tier_config(models: Any, base_url: Any = "", scout_interval_secs: Any = 3600) -> None:
    if not isinstance(models, dict):
        raise ValueError("variant requires models")
    for tier in ("strong", "main", "cheap"):
        if not isinstance(models.get(tier), str) or not models[tier]:
            raise ValueError(f"variant models.{tier} required")
    if not isinstance(base_url, str):
        raise ValueError("variant base_url must be a string")
    if type(scout_interval_secs) is not int or scout_interval_secs < 1:
        raise ValueError("scout_interval_secs must be positive")
    validate_base_url(base_url)


def git(*args: str, cwd: Path = REPO) -> str:
    try:
        return subprocess.run(["git", *args], cwd=cwd, capture_output=True,
                              text=True, check=True, timeout=10).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return "unavailable"


def template_prompt_digests(template: Path) -> dict[str, str]:
    prompts = template / "prompts"
    if not prompts.is_dir():
        return {}
    digests: dict[str, str] = {}
    for path in sorted(prompts.iterdir()):
        if path.is_file() and path.suffix == ".md" and not path.is_symlink():
            digests[path.name.removesuffix(".md")] = sha(path.read_bytes())
    return digests


def binary_provenance(path: Path) -> str:
    """Best-effort statement of whether a binary corresponds to REPO source."""
    if not (path.is_file() and not path.is_symlink()):
        return "missing"
    digest = sha(path.read_bytes())
    state = capture_source_state(cwd=path.resolve().parent)
    if state["status_porcelain"] is not None and state["status_porcelain"] != "":
        return ("unproven: source working tree is dirty; binary digest " + digest[:16] +
                " recorded but no source snapshot can prove it was built from this tree")
    return "unproven: digest recorded; source snapshot captured but not formally linked"


def capture_source_state(cwd: Path = REPO) -> dict[str, Any]:
    revision = git("rev-parse", "HEAD", cwd=cwd)
    if revision == "unavailable":
        return {"revision": None, "dirty": None, "status_digest": None,
                "status_porcelain": None, "untracked_files": None, "branch": None}
    status = git("status", "--porcelain", "--untracked-files=all", cwd=cwd)
    untracked = git("ls-files", "--others", "--exclude-standard", cwd=cwd)
    return {"revision": revision,
            "dirty": bool(status),
            "status_digest": sha(status.encode()),
            "status_porcelain": status,
            "untracked_files": [line for line in untracked.splitlines() if line],
            "branch": git("rev-parse", "--abbrev-ref", "HEAD", cwd=cwd)}


def capture_manifest(variant_id: str, *, description: str = "", base_variant: str = "",
                     binary: Path | None = None, template: Path = REPO / "crates/llm-harness-agent-team-studio/templates/coding-team",
                     variant: Path | None = None, fixture: Path | None = None,
                     reconstructable: bool | None = None,
                     source_cwd: Path = REPO) -> dict[str, Any]:
    """Build a manifest dict from current disk state.  No network, no secrets."""
    if not variant_id or not isinstance(variant_id, str):
        raise ValueError("variant id must be a nonempty string")
    manifest: dict[str, Any] = {"schema_version": MANIFEST_VERSION, "variant_id": variant_id,
                                "created_at": stamp()}
    if description:
        manifest["description"] = description
    if base_variant:
        manifest["base_variant"] = base_variant

    manifest["source"] = capture_source_state(cwd=source_cwd)

    binary = binary.resolve(strict=True) if binary else REPO / "target/release/agent-studio"
    manifest["runtime_binary"] = {"path": str(binary), "sha256": sha(binary.read_bytes()),
                                  "source_provenance": binary_provenance(binary)}

    template = template.resolve(strict=True)
    manifest["team_template"] = {"path": str(template), "tree_digest": tree_digest(template),
                                 "prompt_digests": template_prompt_digests(template)}

    if variant is not None:
        tier = validate_variant_file(variant)
        models = tier["models"]
        base_url = tier.get("base_url", "")
        interval = tier.get("scout_interval_secs", 3600)
    else:
        models, base_url, interval = {}, "", 3600
    validate_tier_config(models, base_url, interval)
    manifest["models"] = models
    manifest["base_url"] = base_url
    manifest["scout_interval_secs"] = interval

    if fixture is not None:
        from e2e_queue import baseline_digest  # local import keeps module init light
        manifest["evaluation"] = {"fixture_tree_digest": tree_digest(fixture),
                                  "baseline_digest": baseline_digest(fixture)}
    else:
        manifest["evaluation"] = {}

    if reconstructable is not None:
        manifest["reconstructable"] = bool(reconstructable)

    secrets = [key for key in manifest
               if key.lower() in ("api_key", "token", "secret", "password")]
    if secrets:
        raise ValueError(f"manifest must not contain credentials: {secrets}")

    manifest["variant_digest"] = sha(canonical({key: value for key, value in manifest.items()
                                                if key not in ("variant_digest", "created_at")}))
    return manifest


def load_manifest(path: Path) -> dict[str, Any]:
    manifest = json.loads(path.read_text())
    if not isinstance(manifest, dict):
        raise ValueError("manifest must be a JSON object")
    if manifest.get("schema_version") != MANIFEST_VERSION:
        raise ValueError(f"unsupported manifest schema version: {manifest.get('schema_version')}")
    if not isinstance(manifest.get("variant_id"), str) or not manifest["variant_id"]:
        raise ValueError("manifest requires variant_id")
    recorded = manifest.get("variant_digest")
    if not isinstance(recorded, str) or len(recorded) != 64:
        raise ValueError("manifest requires a 64-hex variant_digest")
    recomputed = sha(canonical({key: value for key, value in manifest.items()
                                if key not in ("variant_digest", "created_at")}))
    if recorded != recomputed:
        raise ValueError("manifest variant_digest mismatch: manifest was modified after capture")
    return manifest


def manifest_fingerprint(manifest: dict[str, Any]) -> str:
    """Stable configuration fingerprint of an already-captured manifest."""
    return manifest["variant_digest"]


def compare_manifests(left: dict[str, Any], right: dict[str, Any]) -> dict[str, Any]:
    """Field-level differences between two manifests, excluding volatile keys."""
    volatile = {"created_at", "variant_digest", "variant_id", "description",
                "base_variant", "source"}
    keys = sorted((set(left) | set(right)) - volatile)
    differences: dict[str, Any] = {}
    for key in keys:
        left_value, right_value = left.get(key), right.get(key)
        if key in ("runtime_binary", "team_template", "evaluation"):
            sub = {sub_key: {"left": (left_value or {}).get(sub_key),
                             "right": (right_value or {}).get(sub_key)}
                   for sub_key in set(left_value or {}) | set(right_value or {})
                   if (left_value or {}).get(sub_key) != (right_value or {}).get(sub_key)
                   and sub_key != "path"}
            if sub:
                differences[key] = sub
        elif left_value != right_value:
            differences[key] = {"left": left_value, "right": right_value}
    return differences


def verify_against_attempt(manifest: dict[str, Any], attempt_dir: Path) -> list[str]:
    """Cross-check a queue attempt's metadata against a manifest.

    Returns a list of human-readable mismatch descriptions; empty list means
    the attempt's actual execution configuration matches the manifest.
    """
    metadata = json.loads((attempt_dir / "metadata.json").read_text())
    problems: list[str] = []
    if manifest["runtime_binary"]["sha256"] != metadata.get("runtime_binary_digest"):
        problems.append("runtime binary digest differs between manifest and attempt")
    manifest_team = manifest.get("team_template", {})
    if manifest_team.get("tree_digest") and manifest_team["tree_digest"] != metadata.get("template_digest"):
        problems.append("team template digest differs between manifest and attempt")
    for member, digest in manifest_team.get("prompt_digests", {}).items():
        if digest != (metadata.get("prompt_digests") or {}).get(member):
            problems.append(f"prompt digest for {member} differs between manifest and attempt")
    if manifest.get("models") and manifest["models"] != metadata.get("models"):
        problems.append("model tier mapping differs between manifest and attempt")
    if manifest.get("base_url", "") != metadata.get("base_url", ""):
        problems.append("base_url differs between manifest and attempt")
    if manifest.get("scout_interval_secs", 3600) != metadata.get("scout_interval_secs", 3600):
        problems.append("scout_interval_secs differs between manifest and attempt")
    manifest_source = manifest.get("source", {})
    run_source = metadata.get("source_at_run") or {}
    if manifest_source.get("revision") and manifest_source["revision"] != run_source.get("revision"):
        problems.append("source revision differs between manifest and attempt")
    if manifest_source.get("status_digest") and manifest_source["status_digest"] != run_source.get("status_digest"):
        problems.append("source working-tree digest differs between manifest and attempt")
    return problems


def validate_manifest_inputs(manifest: dict[str, Any]) -> None:
    """Structural checks beyond load_manifest: models, base_url, no secrets."""
    if "models" in manifest and manifest["models"]:
        validate_tier_config(manifest["models"], manifest.get("base_url", ""),
                             manifest.get("scout_interval_secs", 3600))
    if manifest.get("base_url"):
        validate_base_url(manifest["base_url"])
    for key in manifest:
        if key.lower() in ("api_key", "token", "secret", "password"):
            raise ValueError("manifest must not contain credentials")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    capture = commands.add_parser("capture", help="capture a variant manifest from current state")
    capture.add_argument("--variant-id", required=True)
    capture.add_argument("--description", default="")
    capture.add_argument("--base-variant", default="")
    capture.add_argument("--binary", type=Path, default=REPO / "target/release/agent-studio")
    capture.add_argument("--template", type=Path,
                         default=REPO / "crates/llm-harness-agent-team-studio/templates/coding-team")
    capture.add_argument("--variant", type=Path, help="P3 variant.json with models/base_url")
    capture.add_argument("--fixture", type=Path, help="pinned benchmark fixture directory")
    capture.add_argument("--reconstructable", action="store_true",
                         help="declare that source and binary are recoverable from this manifest")
    capture.add_argument("--output", type=Path, required=True)
    verify = commands.add_parser("verify", help="recheck a manifest against current files or an attempt")
    verify.add_argument("manifest", type=Path)
    verify.add_argument("--attempt", type=Path, help="queue attempt directory to cross-check")
    args = parser.parse_args()
    if args.command == "capture":
        manifest = capture_manifest(args.variant_id, description=args.description,
                                    base_variant=args.base_variant, binary=args.binary,
                                    template=args.template,
                                    variant=args.variant, fixture=args.fixture,
                                    reconstructable=True if args.reconstructable else None)
        if args.output.exists():
            raise SystemExit(f"output already exists: {args.output}")
        save_json(args.output, manifest)
        print(json.dumps({"variant_id": manifest["variant_id"],
                          "variant_digest": manifest["variant_digest"]}))
    else:
        manifest = load_manifest(args.manifest)
        validate_manifest_inputs(manifest)
        if args.attempt:
            problems = verify_against_attempt(manifest, args.attempt)
            print(json.dumps({"manifest": str(args.manifest), "attempt": str(args.attempt),
                              "problems": problems}))
            sys.exit(1 if problems else 0)
        print(json.dumps({"variant_id": manifest["variant_id"],
                          "variant_digest": manifest["variant_digest"],
                          "binary_sha256": manifest["runtime_binary"]["sha256"],
                          "source_dirty": manifest["source"]["dirty"]}))


if __name__ == "__main__":
    main()
