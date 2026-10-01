#!/usr/bin/env python3
"""Classify RP stable rebases and render manual-port handoffs."""

from __future__ import annotations

import argparse
import json
import pathlib
import re
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


@dataclass(frozen=True)
class Handoff:
    state: str
    pr_number: str = ""
    pr_state: str = ""
    pr_url: str = ""


PORT_REQUEST_PATH = ".github/rp-stable-port-request.json"
PORT_REQUEST_MARKER_PATTERN = re.compile(
    r"^<!-- rp-stable-port-request: (\{[^\r\n]*\}) -->\r?$",
    re.MULTILINE,
)


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


def port_request_identity(report: dict[str, object]) -> dict[str, object]:
    previous = report_mapping(report, "previous")
    candidate = report_mapping(report, "candidate")
    identity = {
        "schema_version": report.get("schema_version"),
        "base_branch": report.get("base_branch"),
        "release_tip": report.get("release_tip"),
        "previous": {
            key: report_string(previous, key)
            for key in ("tag", "commit", "version")
        },
        "candidate": {
            key: report_string(candidate, key)
            for key in ("tag", "commit", "version")
        },
    }
    if identity["schema_version"] != 1:
        raise StableSyncError("port request has an invalid schema version")
    if not isinstance(identity["base_branch"], str) or not identity["base_branch"]:
        raise StableSyncError("port request has an invalid base branch")
    if not isinstance(identity["release_tip"], str) or not identity["release_tip"]:
        raise StableSyncError("port request has an invalid release tip")
    return identity


def port_request_marker(report: dict[str, object]) -> str:
    identity = json.dumps(
        port_request_identity(report),
        sort_keys=True,
        separators=(",", ":"),
    )
    return f"<!-- rp-stable-port-request: {identity} -->"


def parse_port_request_marker(body: object) -> dict[str, object]:
    if not isinstance(body, str):
        raise StableSyncError("manual-port PR has no request identity")
    markers = PORT_REQUEST_MARKER_PATTERN.findall(body)
    if len(markers) != 1:
        raise StableSyncError(
            "manual-port PR must contain exactly one request identity"
        )
    try:
        marker = json.loads(markers[0])
    except json.JSONDecodeError as error:
        raise StableSyncError(
            "manual-port PR has an invalid request identity"
        ) from error
    if not isinstance(marker, dict):
        raise StableSyncError("manual-port PR has an invalid request identity")
    return marker


def read_report_at_ref(
    repo: pathlib.Path, ref: str, path: str = PORT_REQUEST_PATH
) -> dict[str, object] | None:
    result = run_git(repo, "show", f"{ref}:{path}", check=False)
    if result.returncode != 0:
        return None
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise StableSyncError(
            f"{ref} has an invalid {path}"
        ) from error
    if not isinstance(report, dict):
        raise StableSyncError(f"{ref} has an invalid {path}")
    return report


def has_matching_marker_history(
    repo: pathlib.Path,
    ref: str,
    expected_identity: dict[str, object],
) -> bool:
    commits = git_output(repo, "rev-list", ref, "--", PORT_REQUEST_PATH).splitlines()
    for commit in commits:
        report = read_report_at_ref(repo, commit)
        if report is None:
            continue
        try:
            identity = port_request_identity(report)
        except StableSyncError:
            continue
        if identity == expected_identity:
            return True
    return False


def validate_pr(
    pr: dict[str, object],
    expected_repository: str,
    branch_name: str,
    branch_tip: str,
    base_branch: str,
) -> None:
    head = pr.get("head")
    base = pr.get("base")
    if (
        not isinstance(pr.get("number"), int)
        or pr["number"] < 1
        or pr.get("state") not in ("open", "closed")
        or not isinstance(pr.get("html_url"), str)
        or not pr["html_url"]
        or "merged_at" not in pr
    ):
        raise StableSyncError("manual-port PR metadata is invalid")
    if not isinstance(head, dict) or not isinstance(base, dict):
        raise StableSyncError("manual-port PR has invalid head or base metadata")
    head_repo = head.get("repo")
    base_repo = base.get("repo")
    if not isinstance(head_repo, dict) or not isinstance(base_repo, dict):
        raise StableSyncError("manual-port PR has invalid repository metadata")
    if head_repo.get("full_name") != expected_repository:
        raise StableSyncError("manual-port PR head repository is not trusted")
    if base_repo.get("full_name") != expected_repository:
        raise StableSyncError("manual-port PR base repository is not trusted")
    if head.get("ref") != branch_name or head.get("sha") != branch_tip:
        raise StableSyncError("manual-port PR head does not match the live branch")
    if base.get("ref") != base_branch:
        raise StableSyncError("manual-port PR targets an unexpected base")
    if pr.get("merged_at") is not None:
        raise StableSyncError(
            "manual-port PR is merged while the verified release tip is unchanged"
        )


def classify_handoff(
    repo: pathlib.Path,
    branch_ref: str | None,
    branch_name: str,
    report: dict[str, object],
    prs: list[object],
    expected_repository: str,
) -> Handoff:
    expected_identity = port_request_identity(report)
    base_branch = expected_identity["base_branch"]
    release_tip = expected_identity["release_tip"]
    candidate = report_mapping(report, "candidate")
    candidate_commit = report_string(candidate, "commit")
    if not isinstance(base_branch, str) or not isinstance(release_tip, str):
        raise StableSyncError("port request identity is invalid")

    if len(prs) > 1:
        raise StableSyncError("multiple manual-port PRs use the deterministic branch")
    if prs and not isinstance(prs[0], dict):
        raise StableSyncError("manual-port PR metadata is invalid")

    if branch_ref is None:
        if prs:
            raise StableSyncError("manual-port PR exists without its head branch")
        return Handoff(state="missing")

    branch_tip = git_output(repo, "rev-parse", f"{branch_ref}^{{commit}}")
    pr = prs[0] if prs else None
    if pr is not None:
        validate_pr(
            pr,
            expected_repository,
            branch_name,
            branch_tip,
            base_branch,
        )

    marker_report = read_report_at_ref(repo, branch_ref)
    if marker_report is not None:
        if marker_report.get("outcome") != "manual_port_required":
            raise StableSyncError(
                "generated manual-port branch has an invalid request marker"
            )
        if port_request_identity(marker_report) != expected_identity:
            raise StableSyncError(
                "generated manual-port branch has stale request identity"
            )
        parents = git_output(repo, "show", "-s", "--format=%P", branch_ref).split()
        changed_paths = git_output(
            repo, "diff", "--name-only", release_tip, branch_ref
        ).splitlines()
        if parents != [release_tip] or changed_paths != [PORT_REQUEST_PATH]:
            raise StableSyncError(
                "generated manual-port branch contains unrelated changes"
            )
        return Handoff(
            state="generated_marker",
            pr_number=str(pr.get("number", "")) if pr else "",
            pr_state=str(pr.get("state", "")) if pr else "",
            pr_url=str(pr.get("html_url", "")) if pr else "",
        )

    if pr is None:
        raise StableSyncError(
            "same-name manual-port branch has no durable request evidence"
        )
    if pr.get("state") != "open":
        raise StableSyncError(
            "human-progress manual-port PR is not open; refusing to modify it"
        )
    if parse_port_request_marker(pr.get("body")) != expected_identity:
        raise StableSyncError("manual-port PR has stale request identity")

    candidate_is_ancestor = (
        run_git(
            repo,
            "merge-base",
            "--is-ancestor",
            candidate_commit,
            branch_ref,
            check=False,
        ).returncode
        == 0
    )
    if not candidate_is_ancestor and not has_matching_marker_history(
        repo, branch_ref, expected_identity
    ):
        raise StableSyncError(
            "human-progress branch is unrelated to the requested candidate"
        )
    return Handoff(
        state="human_progress",
        pr_number=str(pr.get("number", "")),
        pr_state=str(pr.get("state", "")),
        pr_url=str(pr.get("html_url", "")),
    )


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
    identity_marker = port_request_marker(report)
    return f"""{identity_marker}

# Objective

- Port the reviewed RP stable release line from `{old_tag}` to `{new_tag}`.
- Resolve the patch drift recorded by the guarded stable-sync workflow.

## Solution

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

## Testing

- [ ] Resolve the recorded conflicts without dropping upstream or RP behavior.
- [ ] Run `python3 script/rp_stable_base.py verify --previous-ref HEAD^1`.
- [ ] Run `python3 -m unittest discover -s script -p 'test_rp*.py'`.
- [ ] Run focused ACP, updater, extension, editor, remote, and release tests.

## Self-Review Checklist:

- [ ] I've reviewed my own diff for quality, security, and reliability
- [ ] Unsafe blocks (if any) have justifying comments
- [ ] The content adheres to Zed's UI standards ([UX/UI](https://github.com/zed-industries/zed/blob/main/CONTRIBUTING.md#uiux-checklist) and [icon](https://github.com/zed-industries/zed/blob/main/crates/icons/README.md) guidelines)
- [ ] Tests cover the new/changed behavior
- [ ] Performance impact has been considered and is acceptable

## Showcase

- N/A; this is a source and release-automation port.

---

Release Notes:

- N/A until the reviewed port is complete.
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

    handoff = subparsers.add_parser("handoff")
    handoff.add_argument("--repo", type=pathlib.Path, required=True)
    handoff.add_argument("--branch-ref")
    handoff.add_argument("--branch-name", required=True)
    handoff.add_argument("--report", type=pathlib.Path, required=True)
    handoff.add_argument("--prs", type=pathlib.Path, required=True)
    handoff.add_argument("--expected-repository", required=True)
    handoff.add_argument("--github-output", type=pathlib.Path, required=True)
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
        elif args.command == "render":
            body = manual_port_body(read_report(args.report), args.run_url)
            args.output.write_text(body, encoding="utf-8", newline="\n")
        else:
            prs = json.loads(args.prs.read_text(encoding="utf-8"))
            if not isinstance(prs, list):
                raise StableSyncError("manual-port PR query did not return a list")
            handoff = classify_handoff(
                args.repo.resolve(),
                args.branch_ref,
                args.branch_name,
                read_report(args.report),
                prs,
                args.expected_repository,
            )
            write_github_outputs(
                args.github_output,
                {
                    "state": handoff.state,
                    "pr_number": handoff.pr_number,
                    "pr_state": handoff.pr_state,
                    "pr_url": handoff.pr_url,
                },
            )
    except (OSError, json.JSONDecodeError, StableSyncError) as error:
        print(f"RP stable sync error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
