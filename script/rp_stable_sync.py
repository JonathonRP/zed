#!/usr/bin/env python3
"""Classify RP stable rebases and render manual-port handoffs."""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys
from dataclasses import dataclass


class StableSyncError(RuntimeError):
    pass


@dataclass(frozen=True)
class RebaseRequest:
    repo: pathlib.Path
    release_ref: str
    old_tag: str
    old_sha: str
    old_version: str
    new_tag: str
    new_sha: str
    new_version: str


def run_git(
    repo: pathlib.Path, *args: str, check: bool = True
) -> subprocess.CompletedProcess[str]:
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
        raise StableSyncError(f"git {' '.join(args)} failed: {detail}")
    return result


def git_output(repo: pathlib.Path, *args: str) -> str:
    return run_git(repo, *args).stdout.strip()


def rebase_in_progress(repo: pathlib.Path) -> bool:
    for name in ("rebase-merge", "rebase-apply"):
        path = pathlib.Path(git_output(repo, "rev-parse", "--git-path", name))
        if not path.is_absolute():
            path = repo / path
        if path.exists():
            return True
    return False


def unmerged_paths(repo: pathlib.Path) -> list[str]:
    result = subprocess.run(
        ["git", "diff", "--name-only", "--diff-filter=U", "-z"],
        cwd=repo,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        raise StableSyncError(f"could not list unmerged paths: {detail}")
    return sorted(
        path.decode("utf-8", errors="surrogateescape")
        for path in result.stdout.split(b"\0")
        if path
    )


def write_github_outputs(path: pathlib.Path, values: dict[str, str]) -> None:
    with path.open("a", encoding="utf-8", newline="\n") as output:
        for key, value in values.items():
            if "\n" in value:
                raise StableSyncError(f"GitHub output {key} must be one line")
            output.write(f"{key}={value}\n")


def abort_rebase(repo: pathlib.Path, release_tip: str) -> None:
    run_git(repo, "rebase", "--abort")
    if git_output(repo, "rev-parse", "HEAD") != release_tip:
        raise StableSyncError("rebase abort did not restore the release tip")
    if run_git(repo, "status", "--porcelain", "--untracked-files=no").stdout:
        raise StableSyncError("rebase abort left a dirty release checkout")


def attempt_rebase(
    request: RebaseRequest,
    report_path: pathlib.Path,
    github_output: pathlib.Path | None = None,
) -> str:
    repo = request.repo.resolve()
    release_tip = git_output(repo, "rev-parse", f"{request.release_ref}^{{commit}}")
    command = [
        "rebase",
        "--rebase-merges",
        "--onto",
        request.new_sha,
        request.old_sha,
        request.release_ref,
    ]
    result = run_git(repo, *command, check=False)
    if result.returncode == 0:
        outcome = "rebased"
        if github_output is not None:
            write_github_outputs(
                github_output,
                {"outcome": outcome, "release_tip": release_tip},
            )
        return outcome

    conflicts = unmerged_paths(repo)
    rebase_head = run_git(
        repo, "rev-parse", "--verify", "REBASE_HEAD^{commit}", check=False
    )
    if (
        not rebase_in_progress(repo)
        or rebase_head.returncode != 0
        or not conflicts
    ):
        if rebase_in_progress(repo):
            run_git(repo, "rebase", "--abort", check=False)
        detail = result.stderr.strip() or result.stdout.strip()
        raise StableSyncError(f"rebase failed without a conflict: {detail}")

    failed_commit = rebase_head.stdout.strip()
    failed_subject = git_output(repo, "show", "-s", "--format=%s", failed_commit)
    report = {
        "schema_version": 1,
        "outcome": "manual_port_required",
        "base_branch": "release/rp-stable",
        "release_tip": release_tip,
        "previous": {
            "tag": request.old_tag,
            "commit": request.old_sha,
            "version": request.old_version,
        },
        "candidate": {
            "tag": request.new_tag,
            "commit": request.new_sha,
            "version": request.new_version,
        },
        "rebase": {
            "command": ["git", *command],
            "failed_commit": failed_commit,
            "failed_subject": failed_subject,
            "conflicted_paths": conflicts,
        },
    }
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    abort_rebase(repo, release_tip)

    if github_output is not None:
        write_github_outputs(
            github_output,
            {
                "outcome": "conflict",
                "release_tip": release_tip,
                "failed_commit": failed_commit,
                "conflicted_path_count": str(len(conflicts)),
            },
        )
    return "conflict"


def read_report(path: pathlib.Path) -> dict[str, object]:
    report = json.loads(path.read_text(encoding="utf-8"))
    if (
        not isinstance(report, dict)
        or report.get("schema_version") != 1
        or report.get("outcome") != "manual_port_required"
    ):
        raise StableSyncError(f"{path} is not an RP stable port request")
    return report


def report_mapping(report: dict[str, object], key: str) -> dict[str, object]:
    value = report.get(key)
    if not isinstance(value, dict):
        raise StableSyncError(f"port request has an invalid {key} section")
    return value


def report_string(section: dict[str, object], key: str) -> str:
    value = section.get(key)
    if not isinstance(value, str) or not value:
        raise StableSyncError(f"port request has an invalid {key}")
    return value


def manual_port_body(report: dict[str, object], run_url: str) -> str:
    previous = report_mapping(report, "previous")
    candidate = report_mapping(report, "candidate")
    rebase = report_mapping(report, "rebase")
    paths = rebase.get("conflicted_paths")
    if not isinstance(paths, list) or not paths or not all(
        isinstance(path, str) and path for path in paths
    ):
        raise StableSyncError("port request has invalid conflicted paths")
    old_tag = report_string(previous, "tag")
    old_commit = report_string(previous, "commit")
    old_version = report_string(previous, "version")
    new_tag = report_string(candidate, "tag")
    new_commit = report_string(candidate, "commit")
    new_version = report_string(candidate, "version")
    failed_commit = report_string(rebase, "failed_commit")
    failed_subject = report_string(rebase, "failed_subject")
    path_lines = "\n".join(f"- `{path}`" for path in paths)
    return f"""The guarded RP stable sync found patch drift that requires a manual port.

| | Current | Target |
|---|---|---|
| Tag | `{old_tag}` | `{new_tag}` |
| Commit | `{old_commit}` | `{new_commit}` |
| Zed version | `{old_version}` | `{new_version}` |

The first failing RP commit is `{failed_commit}`:
`{failed_subject}`.

Conflicted paths:

{path_lines}

[Scheduled run evidence]({run_url})

This draft is the single handoff for the target version. Replace the generated
port-request commit with a reviewed manual port, remove
`.github/rp-stable-port-request.json`, and let the existing provenance and
compatibility checks run. Nothing in this workflow auto-merges or publishes a
release.
"""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    rebase = subparsers.add_parser("rebase")
    rebase.add_argument("--repo", type=pathlib.Path, required=True)
    rebase.add_argument("--release-ref", required=True)
    rebase.add_argument("--old-tag", required=True)
    rebase.add_argument("--old-sha", required=True)
    rebase.add_argument("--old-version", required=True)
    rebase.add_argument("--new-tag", required=True)
    rebase.add_argument("--new-sha", required=True)
    rebase.add_argument("--new-version", required=True)
    rebase.add_argument("--report", type=pathlib.Path, required=True)
    rebase.add_argument("--github-output", type=pathlib.Path)

    render = subparsers.add_parser("render")
    render.add_argument("--report", type=pathlib.Path, required=True)
    render.add_argument("--run-url", required=True)
    render.add_argument("--output", type=pathlib.Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "rebase":
            request = RebaseRequest(
                repo=args.repo,
                release_ref=args.release_ref,
                old_tag=args.old_tag,
                old_sha=args.old_sha,
                old_version=args.old_version,
                new_tag=args.new_tag,
                new_sha=args.new_sha,
                new_version=args.new_version,
            )
            attempt_rebase(request, args.report, args.github_output)
        else:
            body = manual_port_body(read_report(args.report), args.run_url)
            args.output.write_text(body, encoding="utf-8", newline="\n")
    except (OSError, json.JSONDecodeError, StableSyncError) as error:
        print(f"RP stable sync error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
