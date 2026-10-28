#!/usr/bin/env python3
"""Validate separately reviewed RP stable workflow transitions."""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
from typing import Any


REPOSITORY = "JonathonRP/zed"
CONTROL_REF = "refs/heads/automation/rp-control"
ORDINARY_WORKFLOW = ".github/workflows/rp_stable_sync.yml"
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")


class WorkflowTransitionError(RuntimeError):
    pass


def require_keys(value: Any, expected: set[str], source: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != expected:
        raise WorkflowTransitionError(
            f"{source} must contain exactly {sorted(expected)}"
        )
    return value


def require_sha(value: Any, source: str) -> str:
    if not isinstance(value, str) or FULL_SHA.fullmatch(value) is None:
        raise WorkflowTransitionError(
            f"{source} must be a lowercase full object SHA"
        )
    return value


def git(repo: pathlib.Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=repo,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise WorkflowTransitionError(
            f"git {' '.join(arguments)} failed: {detail}"
        )
    return result.stdout.strip()


def is_ancestor(repo: pathlib.Path, ancestor: str, descendant: str) -> bool:
    result = subprocess.run(
        ["git", "merge-base", "--is-ancestor", ancestor, descendant],
        cwd=repo,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode == 0:
        return True
    if result.returncode == 1:
        return False
    detail = result.stderr.decode(errors="replace").strip()
    raise WorkflowTransitionError(
        f"could not compare control history: {detail}"
    )


def load_transition(path: pathlib.Path) -> dict[str, Any]:
    record = json.loads(path.read_text(encoding="utf-8"))
    return validate_transition_record(record)


def validate_transition_record(record: Any) -> dict[str, Any]:
    record = require_keys(
        record,
        {
            "schema_version",
            "repository",
            "workflow_path",
            "previous_workflow_blob_sha",
            "promoted_workflow_blob_sha",
            "release_transition",
            "reviewed_control",
        },
        "workflow transition",
    )
    if (
        record["schema_version"] != 1
        or record["repository"] != REPOSITORY
        or record["workflow_path"] != ORDINARY_WORKFLOW
    ):
        raise WorkflowTransitionError(
            "workflow transition schema, repository, or path is invalid"
        )
    previous_blob = require_sha(
        record["previous_workflow_blob_sha"], "previous workflow blob"
    )
    promoted_blob = require_sha(
        record["promoted_workflow_blob_sha"], "promoted workflow blob"
    )
    if previous_blob == promoted_blob:
        raise WorkflowTransitionError("workflow transition must change the blob")

    release = require_keys(
        record["release_transition"],
        {"previous_tip", "merge_sha", "head_sha"},
        "release transition identity",
    )
    for key in ("previous_tip", "merge_sha", "head_sha"):
        require_sha(release[key], f"release transition {key}")

    reviewed = require_keys(
        record["reviewed_control"],
        {
            "ref",
            "workflow_path",
            "previous_workflow_blob_sha",
            "promoted_workflow_blob_sha",
            "control_workflow_blob_sha",
            "reviewed_tip",
            "pull_requests",
        },
        "reviewed control identity",
    )
    if (
        reviewed["ref"] != CONTROL_REF
        or reviewed["workflow_path"] != ORDINARY_WORKFLOW
    ):
        raise WorkflowTransitionError(
            "reviewed control ref or workflow path is invalid"
        )
    control_previous_blob = require_sha(
        reviewed["previous_workflow_blob_sha"],
        "reviewed control previous workflow blob",
    )
    control_promoted_blob = require_sha(
        reviewed["promoted_workflow_blob_sha"],
        "reviewed release promoted workflow blob",
    )
    require_sha(
        reviewed["control_workflow_blob_sha"],
        "reviewed control workflow blob",
    )
    require_sha(reviewed["reviewed_tip"], "reviewed control tip")
    if control_previous_blob != previous_blob:
        raise WorkflowTransitionError(
            "release and control previous workflow blobs differ"
        )
    if control_promoted_blob != promoted_blob:
        raise WorkflowTransitionError(
            "release and reviewed promoted workflow blobs differ"
        )
    pulls = reviewed["pull_requests"]
    if not isinstance(pulls, list) or not pulls:
        raise WorkflowTransitionError(
            "reviewed control pull requests must be a non-empty list"
        )
    numbers: set[int] = set()
    merge_shas: set[str] = set()
    for index, pull in enumerate(pulls):
        pull = require_keys(
            pull,
            {"number", "head_sha", "merge_sha"},
            f"reviewed control pull request {index}",
        )
        number = pull["number"]
        if (
            not isinstance(number, int)
            or isinstance(number, bool)
            or number <= 0
            or number in numbers
        ):
            raise WorkflowTransitionError(
                f"reviewed control pull request {index} number is invalid"
            )
        numbers.add(number)
        require_sha(pull["head_sha"], f"reviewed control PR {number} head")
        merge_sha = require_sha(
            pull["merge_sha"], f"reviewed control PR {number} merge"
        )
        if merge_sha in merge_shas:
            raise WorkflowTransitionError(
                "reviewed control merge SHAs must be unique"
            )
        merge_shas.add(merge_sha)
    return record


def validate_transition_topology(
    repo: pathlib.Path, record: dict[str, Any]
) -> None:
    record = validate_transition_record(record)
    release = record["release_transition"]
    merge_sha = release["merge_sha"]
    parents = git(repo, "rev-list", "--parents", "-n", "1", merge_sha).split()
    if parents != [
        merge_sha,
        release["previous_tip"],
        release["head_sha"],
    ]:
        raise WorkflowTransitionError(
            "release transition merge topology changed"
        )
    merge_tree = git(repo, "rev-parse", f"{merge_sha}^{{tree}}")
    head_tree = git(repo, "rev-parse", f"{release['head_sha']}^{{tree}}")
    if merge_tree != head_tree:
        raise WorkflowTransitionError(
            "release transition merge tree differs from its reviewed head"
        )

    workflow_path = record["workflow_path"]
    previous_blob = git(
        repo, "rev-parse", f"{release['previous_tip']}:{workflow_path}"
    )
    head_blob = git(repo, "rev-parse", f"{release['head_sha']}:{workflow_path}")
    merge_blob = git(repo, "rev-parse", f"{merge_sha}:{workflow_path}")
    if previous_blob != record["previous_workflow_blob_sha"]:
        raise WorkflowTransitionError(
            "release transition previous workflow blob changed"
        )
    if (
        head_blob != record["promoted_workflow_blob_sha"]
        or merge_blob != record["promoted_workflow_blob_sha"]
    ):
        raise WorkflowTransitionError(
            "release transition promoted workflow blob changed"
        )


def validate_reviewed_control_topology(
    repo: pathlib.Path,
    record: dict[str, Any],
    resolved_control_ref: str,
) -> None:
    record = validate_transition_record(record)
    reviewed = record["reviewed_control"]
    workflow_path = reviewed["workflow_path"]
    promoted_blob = reviewed["control_workflow_blob_sha"]
    previous_merge: str | None = None
    for index, pull in enumerate(reviewed["pull_requests"]):
        merge_sha = pull["merge_sha"]
        parents = git(
            repo, "rev-list", "--parents", "-n", "1", merge_sha
        ).split()
        if (
            len(parents) != 3
            or parents[0] != merge_sha
            or parents[2] != pull["head_sha"]
        ):
            raise WorkflowTransitionError(
                f"reviewed control PR #{pull['number']} topology changed"
            )
        if previous_merge is not None and parents[1] != previous_merge:
            raise WorkflowTransitionError(
                "reviewed control pull requests are not sequential"
            )
        if index == 0:
            base_blob = git(
                repo, "rev-parse", f"{parents[1]}:{workflow_path}"
            )
            if base_blob != reviewed["previous_workflow_blob_sha"]:
                raise WorkflowTransitionError(
                    "reviewed control transition previous workflow blob changed"
                )
        head_blob = git(
            repo, "rev-parse", f"{pull['head_sha']}:{workflow_path}"
        )
        merge_blob = git(repo, "rev-parse", f"{merge_sha}:{workflow_path}")
        if head_blob != promoted_blob or merge_blob != promoted_blob:
            raise WorkflowTransitionError(
                f"reviewed control PR #{pull['number']} did not retain "
                "the promoted workflow blob"
            )
        previous_merge = merge_sha
    if previous_merge != reviewed["reviewed_tip"]:
        raise WorkflowTransitionError(
            "reviewed control tip does not match the final reviewed merge"
        )
    live_tip = git(
        repo, "rev-parse", f"{resolved_control_ref}^{{commit}}"
    )
    if not is_ancestor(repo, reviewed["reviewed_tip"], live_tip):
        raise WorkflowTransitionError(
            "reviewed control tip is not contained in the live control ref"
        )
