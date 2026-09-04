#!/usr/bin/env python3
"""Validate and advance the pinned upstream base for RP stable releases."""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
from typing import Any


BASE_PATH = pathlib.Path(".github/rp-stable-base.json")
UPSTREAM_REPOSITORY = "zed-industries/zed"
SYNC_CRON_UTC = "0 0 * * *"
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")
STABLE_VERSION = re.compile(
    r"^(?P<major>0|[1-9][0-9]*)\."
    r"(?P<minor>0|[1-9][0-9]*)\."
    r"(?P<patch>0|[1-9][0-9]*)$"
)


class StableBaseError(RuntimeError):
    pass


def run_git(repo: pathlib.Path, *args: str, check: bool = True) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if check and result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise StableBaseError(f"git {' '.join(args)} failed: {detail}")
    return result.stdout.strip()


def parse_version(value: str) -> tuple[int, int, int]:
    match = STABLE_VERSION.fullmatch(value)
    if match is None:
        raise StableBaseError(f"invalid stable semantic version {value!r}")
    return tuple(int(match[group]) for group in ("major", "minor", "patch"))


def validate_base(base: Any, source: str) -> dict[str, Any]:
    expected_keys = {
        "schema_version",
        "automation",
        "initial_source_correction",
        "upstream_repository",
        "upstream_tag",
        "upstream_tag_commit",
        "upstream_version",
    }
    if not isinstance(base, dict) or set(base) != expected_keys:
        raise StableBaseError(f"{source} must contain exactly {sorted(expected_keys)}")
    if base["schema_version"] != 2:
        raise StableBaseError(f"{source} has an unsupported schema version")
    expected_automation = {
        "fork_main_sync_cron_utc": SYNC_CRON_UTC,
        "stable_sync_cron_utc": SYNC_CRON_UTC,
    }
    if base["automation"] != expected_automation:
        raise StableBaseError(
            f"{source} must schedule both upstream syncs at midnight UTC"
        )
    correction = base["initial_source_correction"]
    correction_keys = {
        "previous_rp_tip",
        "previous_upstream_commit",
        "previous_upstream_version",
    }
    if not isinstance(correction, dict) or set(correction) != correction_keys:
        raise StableBaseError(
            f"{source} must contain the initial source-correction identity"
        )
    for key in ("previous_rp_tip", "previous_upstream_commit"):
        if not isinstance(correction[key], str) or not FULL_SHA.fullmatch(
            correction[key]
        ):
            raise StableBaseError(f"{source} has an invalid {key}")
    parse_version(correction["previous_upstream_version"])
    if base["upstream_repository"] != UPSTREAM_REPOSITORY:
        raise StableBaseError(f"{source} must pin {UPSTREAM_REPOSITORY}")
    version = base["upstream_version"]
    parse_version(version)
    if base["upstream_tag"] != f"v{version}":
        raise StableBaseError(f"{source} tag does not match its semantic version")
    if not isinstance(base["upstream_tag_commit"], str) or not FULL_SHA.fullmatch(
        base["upstream_tag_commit"]
    ):
        raise StableBaseError(
            f"{source} must contain a lowercase full tag commit SHA"
        )
    return base


def read_base(path: pathlib.Path) -> dict[str, Any]:
    return validate_base(json.loads(path.read_text(encoding="utf-8")), str(path))


def cargo_version_at(repo: pathlib.Path, revision: str) -> str:
    cargo_toml = run_git(repo, "show", f"{revision}:crates/zed/Cargo.toml")
    match = re.search(r'(?m)^version\s*=\s*"([^"]+)"\s*$', cargo_toml)
    if match is None:
        raise StableBaseError(f"could not read crates/zed version at {revision}")
    return match.group(1)


def verify_base(
    repo: pathlib.Path, base_path: pathlib.Path, app_version: str | None = None
) -> dict[str, Any]:
    base = read_base(base_path)
    tag_ref = f"refs/tags/{base['upstream_tag']}^{{commit}}"
    tag_commit = run_git(repo, "rev-parse", "--verify", tag_ref)
    if tag_commit != base["upstream_tag_commit"]:
        raise StableBaseError(
            f"{base['upstream_tag']} resolves to {tag_commit}, "
            f"not pinned commit {base['upstream_tag_commit']}"
        )
    tag_version = cargo_version_at(repo, tag_commit)
    if tag_version != base["upstream_version"]:
        raise StableBaseError(
            f"{base['upstream_tag']} contains Zed {tag_version}, "
            f"not {base['upstream_version']}"
        )
    head_version = cargo_version_at(repo, "HEAD")
    if head_version != base["upstream_version"]:
        raise StableBaseError(
            f"RP checkout contains Zed {head_version}, not pinned stable "
            f"{base['upstream_version']}"
        )
    ancestry = subprocess.run(
        [
            "git",
            "merge-base",
            "--is-ancestor",
            base["upstream_tag_commit"],
            "HEAD",
        ],
        cwd=repo,
        check=False,
    )
    if ancestry.returncode != 0:
        raise StableBaseError(
            f"RP checkout is not based on {base['upstream_tag_commit']}"
        )
    if app_version is not None:
        normalized = app_version.split("+", 1)[0].split("-", 1)[0]
        if normalized != base["upstream_version"]:
            raise StableBaseError(
                f"built AppVersion base {normalized} does not equal pinned stable "
                f"{base['upstream_version']}"
            )
    return base


def verify_transition(
    repo: pathlib.Path, current: dict[str, Any], previous_ref: str
) -> None:
    previous_sha = run_git(repo, "rev-parse", "--verify", f"{previous_ref}^{{commit}}")
    previous_json = run_git(
        repo,
        "show",
        f"{previous_ref}:{BASE_PATH.as_posix()}",
        check=False,
    )
    if previous_json:
        previous = validate_base(
            json.loads(previous_json), f"{previous_ref}:{BASE_PATH.as_posix()}"
        )
        if parse_version(current["upstream_version"]) < parse_version(
            previous["upstream_version"]
        ):
            raise StableBaseError(
                "pinned upstream stable version regresses from "
                f"{previous['upstream_version']} to {current['upstream_version']}"
            )
        return

    correction = current["initial_source_correction"]
    if previous_sha != correction["previous_rp_tip"]:
        raise StableBaseError(
            "previous release has no stable-base metadata and does not match the "
            "audited initial source-correction tip"
        )
    previous_version = cargo_version_at(repo, previous_ref)
    if previous_version != correction["previous_upstream_version"]:
        raise StableBaseError(
            f"previous RP tip contains Zed {previous_version}, not audited "
            f"{correction['previous_upstream_version']}"
        )
    ancestry = subprocess.run(
        [
            "git",
            "merge-base",
            "--is-ancestor",
            correction["previous_upstream_commit"],
            previous_ref,
        ],
        cwd=repo,
        check=False,
    )
    if ancestry.returncode != 0:
        raise StableBaseError(
            "previous RP tip is not based on its audited unreleased-main commit"
        )


def candidate_base(
    repo: pathlib.Path,
    current: dict[str, Any],
    release: dict[str, Any],
) -> dict[str, Any] | None:
    if release.get("draft") is not False or release.get("prerelease") is not False:
        raise StableBaseError("latest upstream release is draft or prerelease")
    tag = release.get("tag_name")
    if not isinstance(tag, str) or not tag.startswith("v"):
        raise StableBaseError("latest upstream release has an invalid tag")
    version = tag[1:]
    candidate_version = parse_version(version)
    current_version = parse_version(current["upstream_version"])
    if candidate_version <= current_version:
        return None

    tag_commit = run_git(repo, "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}")
    if not FULL_SHA.fullmatch(tag_commit):
        raise StableBaseError(f"could not resolve {tag} to a full commit")
    if cargo_version_at(repo, tag_commit) != version:
        raise StableBaseError(f"{tag} does not contain Zed version {version}")
    return {
        "schema_version": 2,
        "automation": current["automation"],
        "initial_source_correction": current["initial_source_correction"],
        "upstream_repository": UPSTREAM_REPOSITORY,
        "upstream_tag": tag,
        "upstream_tag_commit": tag_commit,
        "upstream_version": version,
    }


def write_github_outputs(path: pathlib.Path, values: dict[str, str]) -> None:
    with path.open("a", encoding="utf-8", newline="\n") as output:
        for key, value in values.items():
            output.write(f"{key}={value}\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", type=pathlib.Path, default=pathlib.Path.cwd())
    parser.add_argument("--base", type=pathlib.Path)
    subparsers = parser.add_subparsers(dest="command", required=True)

    verify = subparsers.add_parser("verify")
    verify.add_argument("--app-version")
    verify.add_argument("--previous-ref")

    candidate = subparsers.add_parser("candidate")
    candidate.add_argument("--release-json", type=pathlib.Path, required=True)
    candidate.add_argument("--output", type=pathlib.Path, required=True)
    candidate.add_argument("--github-output", type=pathlib.Path)

    args = parser.parse_args()
    repo = args.repo.resolve()
    base_path = (args.base or repo / BASE_PATH).resolve()
    try:
        current = verify_base(repo, base_path)
        if args.command == "verify":
            verify_base(repo, base_path, args.app_version)
            if args.previous_ref:
                verify_transition(repo, current, args.previous_ref)
            print(json.dumps(current, sort_keys=True))
        else:
            release = json.loads(args.release_json.read_text(encoding="utf-8"))
            update = candidate_base(repo, current, release)
            if update is None:
                if args.github_output:
                    write_github_outputs(
                        args.github_output,
                        {
                            "update": "false",
                            "old_tag": current["upstream_tag"],
                            "old_sha": current["upstream_tag_commit"],
                            "old_version": current["upstream_version"],
                        },
                    )
                print("already tracking the latest official stable release")
            else:
                args.output.write_text(
                    json.dumps(update, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
                if args.github_output:
                    write_github_outputs(
                        args.github_output,
                        {
                            "update": "true",
                            "old_tag": current["upstream_tag"],
                            "old_sha": current["upstream_tag_commit"],
                            "old_version": current["upstream_version"],
                            "new_tag": update["upstream_tag"],
                            "new_sha": update["upstream_tag_commit"],
                            "new_version": update["upstream_version"],
                        },
                    )
                print(json.dumps(update, sort_keys=True))
    except (OSError, json.JSONDecodeError, StableBaseError) as error:
        print(f"RP stable base error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
