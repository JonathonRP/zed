#!/usr/bin/env python3
"""Authorize an RP stable release from trusted control-plane code."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import tempfile
from typing import Any
from urllib.parse import quote

from rp_workflow_transition import (
    WorkflowTransitionError,
    validate_reviewed_control_topology,
    validate_transition_record,
    validate_transition_topology,
)


REPOSITORY = "JonathonRP/zed"
OWNER = "JonathonRP"
RELEASE_REF = "refs/heads/release/rp-stable"
ORDINARY_WORKFLOW = ".github/workflows/rp_stable_sync.yml"
ORDINARY_JOB = "Validate RP stable compatibility"
PROFILE_WORKFLOW = ".github/workflows/rp_profile_compatibility_build.yml"
PROFILE_CONTROL_REF = "refs/heads/automation/rp-control"
AUTHORIZATION_DIRECTORY = pathlib.Path(".github/rp-release-authorizations")
WORKFLOW_PROMOTION_DIRECTORY = pathlib.Path(".github/rp-workflow-promotions")
TRANSITION_MANIFEST_DIRECTORY = pathlib.PurePosixPath(
    ".github/rp-workflow-transitions"
)
TRANSITION_VALIDATOR_PATH = "script/rp_workflow_transition.py"
INITIAL_WORKFLOW_RELEASE_SHA = "5b0c427ee3a4956527c652ff7ea4c156113be2b1"
INITIAL_ORDINARY_WORKFLOW_BLOB_SHA = "dbdb85382fa5a8a529313ca6c4d49e6eb5fec0f5"
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")
CHECK_DETAILS = re.compile(
    r"^https://github\.com/JonathonRP/zed/actions/runs/"
    r"(?P<run_id>[1-9][0-9]*)/job/(?P<job_id>[1-9][0-9]*)$"
)


class AuthorizationError(RuntimeError):
    pass


def require_keys(value: Any, expected: set[str], source: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != expected:
        raise AuthorizationError(f"{source} must contain exactly {sorted(expected)}")
    return value


def require_sha(value: Any, source: str) -> str:
    if not isinstance(value, str) or FULL_SHA.fullmatch(value) is None:
        raise AuthorizationError(f"{source} must be a lowercase full commit SHA")
    return value


def require_positive_integer(value: Any, source: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise AuthorizationError(f"{source} must be a positive integer")
    return value


def run(
    arguments: list[str],
    *,
    cwd: pathlib.Path | None = None,
    binary: bool = False,
) -> str | bytes:
    result = subprocess.run(
        arguments,
        cwd=cwd,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=not binary,
    )
    if result.returncode != 0:
        stderr = result.stderr
        stdout = result.stdout
        if binary:
            detail = (stderr or stdout).decode("utf-8", errors="replace").strip()
        else:
            detail = (stderr or stdout).strip()
        raise AuthorizationError(f"{' '.join(arguments)} failed: {detail}")
    return result.stdout


def gh_json(endpoint: str) -> Any:
    output = run(["gh", "api", endpoint])
    assert isinstance(output, str)
    return json.loads(output)


def git(control_root: pathlib.Path, *arguments: str) -> str:
    output = run(["git", *arguments], cwd=control_root)
    assert isinstance(output, str)
    return output.strip()


def validate_event(
    *,
    event_name: str,
    repository: str,
    ref: str,
    actor: str,
    triggering_actor: str,
    control_sha: str,
    release_sha: str,
    expected_head_sha: str | None,
) -> None:
    if repository != REPOSITORY:
        raise AuthorizationError(f"release repository must be {REPOSITORY}")
    if ref != RELEASE_REF:
        raise AuthorizationError(f"release workflow must run from {RELEASE_REF}")
    require_sha(control_sha, "control SHA")
    require_sha(release_sha, "release SHA")
    if expected_head_sha is not None:
        require_sha(expected_head_sha, "validated PR head SHA")
    if event_name not in {"push", "workflow_dispatch"}:
        raise AuthorizationError(f"unsupported release event {event_name!r}")
    if event_name == "push":
        if release_sha != control_sha:
            raise AuthorizationError("push release SHA must equal the control SHA")
    elif actor != OWNER or triggering_actor != OWNER:
        raise AuthorizationError(
            f"manual release must be initiated and triggered by {OWNER}"
        )
    elif expected_head_sha is None:
        raise AuthorizationError("manual release requires the validated PR head SHA")


def validate_release_history(
    control_root: pathlib.Path,
    control_sha: str,
    release_sha: str,
    release_remote: str = "origin",
) -> None:
    remote_ref = "refs/remotes/origin/rp-release-authorization"
    git(
        control_root,
        "fetch",
        "--force",
        "--no-tags",
        release_remote,
        f"+{RELEASE_REF}:{remote_ref}",
    )
    live_sha = git(control_root, "rev-parse", "--verify", f"{remote_ref}^{{commit}}")
    if live_sha != control_sha:
        raise AuthorizationError(
            f"control SHA {control_sha} is not the live RP stable tip {live_sha}"
        )
    first_parent = set(
        git(control_root, "rev-list", "--first-parent", control_sha).splitlines()
    )
    if release_sha not in first_parent:
        raise AuthorizationError(
            f"release SHA {release_sha} is not on the RP stable first-parent history"
        )


def select_pull_request(
    pulls: Any, release_sha: str, expected_head_sha: str | None
) -> dict[str, Any]:
    if not isinstance(pulls, list):
        raise AuthorizationError("commit pull-request response must be a list")
    matches = []
    for candidate in pulls:
        if not isinstance(candidate, dict):
            continue
        base = candidate.get("base") or {}
        head = candidate.get("head") or {}
        if (
            candidate.get("merged_at") is not None
            and candidate.get("merge_commit_sha") == release_sha
            and base.get("ref") == "release/rp-stable"
            and (base.get("repo") or {}).get("full_name") == REPOSITORY
            and (head.get("repo") or {}).get("full_name") == REPOSITORY
        ):
            matches.append(candidate)
    if len(matches) != 1:
        raise AuthorizationError(
            "release SHA must identify exactly one merged RP stable pull request"
        )
    pull = matches[0]
    head_sha = require_sha((pull["head"] or {}).get("sha"), "PR head SHA")
    if expected_head_sha is not None and head_sha != expected_head_sha:
        raise AuthorizationError(
            f"PR head {head_sha} does not match expected {expected_head_sha}"
        )
    require_positive_integer(pull.get("number"), "PR number")
    return pull


def validate_merge_identity(
    control_root: pathlib.Path,
    release_sha: str,
    head_sha: str,
    expected_workflow_blob_sha: str | None = None,
) -> str:
    parents = git(
        control_root, "rev-list", "--parents", "-n", "1", release_sha
    ).split()
    if len(parents) != 3 or parents[0] != release_sha or parents[2] != head_sha:
        raise AuthorizationError(
            "release must be a two-parent merge whose second parent is the "
            "validated PR head"
        )
    release_tree = git(control_root, "rev-parse", f"{release_sha}^{{tree}}")
    head_tree = git(control_root, "rev-parse", f"{head_sha}^{{tree}}")
    if release_tree != head_tree:
        raise AuthorizationError(
            "release merge tree differs from the validated PR head tree"
        )
    workflow_path = ORDINARY_WORKFLOW
    head_blob = git(control_root, "rev-parse", f"{head_sha}:{workflow_path}")
    release_blob = git(control_root, "rev-parse", f"{release_sha}:{workflow_path}")
    if head_blob != release_blob:
        raise AuthorizationError(
            "release merge changed the validated ordinary compatibility workflow"
        )
    if release_sha == INITIAL_WORKFLOW_RELEASE_SHA:
        if expected_workflow_blob_sha != INITIAL_ORDINARY_WORKFLOW_BLOB_SHA:
            raise AuthorizationError(
                "initial workflow release requires its audited workflow blob"
            )
    else:
        base_blob = git(control_root, "rev-parse", f"{release_sha}^1:{workflow_path}")
        if base_blob != head_blob:
            raise AuthorizationError(
                "release PR changed its own ordinary compatibility workflow"
            )
    if (
        expected_workflow_blob_sha is not None
        and head_blob != expected_workflow_blob_sha
    ):
        raise AuthorizationError(
            "ordinary compatibility workflow blob changed"
        )
    return head_blob


def parse_check_ids(check: dict[str, Any]) -> tuple[int, int]:
    details_url = check.get("details_url")
    match = CHECK_DETAILS.fullmatch(details_url or "")
    if match is None:
        raise AuthorizationError("compatibility check has an untrusted details URL")
    return int(match["run_id"]), int(match["job_id"])


def validate_ordinary_evidence(
    *,
    head_sha: str,
    check: dict[str, Any],
    workflow_run: dict[str, Any],
    job: dict[str, Any],
    head_branch: str,
    expected_run_id: int | None = None,
    expected_job_id: int | None = None,
) -> tuple[int, int]:
    app = check.get("app") or {}
    if (
        check.get("name") != ORDINARY_JOB
        or check.get("head_sha") != head_sha
        or check.get("status") != "completed"
        or check.get("conclusion") != "success"
        or app.get("slug") != "github-actions"
    ):
        raise AuthorizationError("ordinary compatibility check is not trusted")
    run_id, job_id = parse_check_ids(check)
    if expected_run_id is not None and run_id != expected_run_id:
        raise AuthorizationError("ordinary compatibility run ID changed")
    if expected_job_id is not None and job_id != expected_job_id:
        raise AuthorizationError("ordinary compatibility job ID changed")
    repository = workflow_run.get("repository") or {}
    head_repository = workflow_run.get("head_repository") or {}
    if (
        workflow_run.get("id") != run_id
        or workflow_run.get("event") != "pull_request"
        or workflow_run.get("path") != ORDINARY_WORKFLOW
        or workflow_run.get("head_branch") != head_branch
        or workflow_run.get("head_sha") != head_sha
        or workflow_run.get("status") != "completed"
        or workflow_run.get("conclusion") != "success"
        or repository.get("full_name") != REPOSITORY
        or head_repository.get("full_name") != REPOSITORY
    ):
        raise AuthorizationError("ordinary compatibility workflow run is not trusted")
    if (
        job.get("id") != job_id
        or job.get("name") != ORDINARY_JOB
        or job.get("head_sha") != head_sha
        or job.get("status") != "completed"
        or job.get("conclusion") != "success"
    ):
        raise AuthorizationError("ordinary compatibility job is not trusted")
    return run_id, job_id


def validate_authorization_record(
    record: Any, release_sha: str, head_sha: str
) -> dict[str, Any]:
    record = require_keys(
        record,
        {
            "schema_version",
            "repository",
            "release_merge_sha",
            "pull_request",
            "ordinary_compatibility",
            "copied_profile_attestation",
        },
        "release authorization",
    )
    if record["schema_version"] != 1 or record["repository"] != REPOSITORY:
        raise AuthorizationError("release authorization schema or repository is invalid")
    if require_sha(record["release_merge_sha"], "record release SHA") != release_sha:
        raise AuthorizationError("release authorization targets another merge")
    pull = require_keys(
        record["pull_request"], {"number", "head_sha"}, "record pull request"
    )
    require_positive_integer(pull["number"], "record PR number")
    if require_sha(pull["head_sha"], "record PR head SHA") != head_sha:
        raise AuthorizationError("release authorization targets another PR head")
    ordinary = require_keys(
        record["ordinary_compatibility"],
        {"workflow_path", "workflow_blob_sha", "job_name", "run_id", "job_id"},
        "ordinary compatibility record",
    )
    if (
        ordinary["workflow_path"] != ORDINARY_WORKFLOW
        or ordinary["job_name"] != ORDINARY_JOB
    ):
        raise AuthorizationError("ordinary compatibility identity is invalid")
    if (
        not isinstance(ordinary["workflow_blob_sha"], str)
        or re.fullmatch(r"[0-9a-f]{40}", ordinary["workflow_blob_sha"]) is None
    ):
        raise AuthorizationError("ordinary compatibility workflow blob is invalid")
    require_positive_integer(ordinary["run_id"], "ordinary run ID")
    require_positive_integer(ordinary["job_id"], "ordinary job ID")
    profile = require_keys(
        record["copied_profile_attestation"],
        {
            "attested_by",
            "result",
            "scope",
            "control_ref",
            "control_sha",
            "workflow_path",
            "workflow_blob_sha",
            "run_id",
            "jobs",
            "artifacts",
            "metadata",
        },
        "copied-profile attestation",
    )
    if (
        profile["attested_by"] != OWNER
        or profile["result"] != "passed"
        or profile["scope"]
        != ["copied_profile_runtime", "wsl_remote_server_compatibility"]
        or profile["control_ref"] != PROFILE_CONTROL_REF
        or profile["workflow_path"] != PROFILE_WORKFLOW
    ):
        raise AuthorizationError("copied-profile attestation statement is invalid")
    require_sha(profile["control_sha"], "copied-profile control SHA")
    if (
        not isinstance(profile["workflow_blob_sha"], str)
        or re.fullmatch(r"[0-9a-f]{40}", profile["workflow_blob_sha"]) is None
    ):
        raise AuthorizationError("copied-profile workflow blob SHA is invalid")
    require_positive_integer(profile["run_id"], "copied-profile run ID")
    if not isinstance(profile["jobs"], list) or not profile["jobs"]:
        raise AuthorizationError("copied-profile jobs must be a non-empty list")
    for index, job in enumerate(profile["jobs"]):
        require_keys(job, {"id", "name"}, f"copied-profile job {index}")
        require_positive_integer(job["id"], f"copied-profile job {index} ID")
        if not isinstance(job["name"], str) or not job["name"]:
            raise AuthorizationError(f"copied-profile job {index} name is invalid")
    if not isinstance(profile["artifacts"], list) or not profile["artifacts"]:
        raise AuthorizationError("copied-profile artifacts must be a non-empty list")
    for index, artifact in enumerate(profile["artifacts"]):
        require_keys(
            artifact,
            {"id", "name", "size_in_bytes", "digest"},
            f"copied-profile artifact {index}",
        )
        require_positive_integer(artifact["id"], f"artifact {index} ID")
        require_positive_integer(artifact["size_in_bytes"], f"artifact {index} size")
        if (
            not isinstance(artifact["name"], str)
            or head_sha not in artifact["name"]
            or not isinstance(artifact["digest"], str)
            or re.fullmatch(r"sha256:[0-9a-f]{64}", artifact["digest"]) is None
        ):
            raise AuthorizationError(f"copied-profile artifact {index} is invalid")
    metadata = require_keys(
        profile["metadata"],
        {
            "artifact_id",
            "manifest_file",
            "manifest_sha256",
            "release_notes_file",
            "release_notes_sha256",
            "source_sha",
            "upstream_tag",
            "upstream_tag_commit",
            "upstream_version",
        },
        "copied-profile metadata",
    )
    require_positive_integer(metadata["artifact_id"], "metadata artifact ID")
    if metadata["source_sha"] != head_sha:
        raise AuthorizationError("copied-profile manifest targets another source")
    for key in ("manifest_sha256", "release_notes_sha256"):
        if (
            not isinstance(metadata[key], str)
            or re.fullmatch(r"[0-9a-f]{64}", metadata[key]) is None
        ):
            raise AuthorizationError(f"copied-profile {key} is invalid")
    require_sha(metadata["upstream_tag_commit"], "copied-profile upstream SHA")
    return record


def validate_profile_evidence(
    *,
    profile: dict[str, Any],
    head_sha: str,
    workflow_blob_sha: str,
    workflow_run: dict[str, Any],
    jobs: list[dict[str, Any]],
    artifacts: list[dict[str, Any]],
    metadata_files: dict[str, bytes],
) -> None:
    if workflow_blob_sha != profile["workflow_blob_sha"]:
        raise AuthorizationError("copied-profile workflow blob changed")
    repository = workflow_run.get("repository") or {}
    head_repository = workflow_run.get("head_repository") or {}
    if (
        workflow_run.get("id") != profile["run_id"]
        or workflow_run.get("event") != "workflow_dispatch"
        or workflow_run.get("path") != PROFILE_WORKFLOW
        or workflow_run.get("head_branch") != PROFILE_CONTROL_REF.removeprefix(
            "refs/heads/"
        )
        or workflow_run.get("head_sha") != profile["control_sha"]
        or workflow_run.get("status") != "completed"
        or workflow_run.get("conclusion") != "success"
        or (workflow_run.get("actor") or {}).get("login") != OWNER
        or (workflow_run.get("triggering_actor") or {}).get("login") != OWNER
        or repository.get("full_name") != REPOSITORY
        or head_repository.get("full_name") != REPOSITORY
    ):
        raise AuthorizationError("copied-profile workflow run is not trusted")
    expected_jobs = {(job["id"], job["name"]) for job in profile["jobs"]}
    actual_jobs = {
        (job.get("id"), job.get("name"))
        for job in jobs
        if job.get("status") == "completed"
        and job.get("conclusion") == "success"
        and job.get("head_sha") == profile["control_sha"]
    }
    if actual_jobs != expected_jobs or len(jobs) != len(expected_jobs):
        raise AuthorizationError("copied-profile job evidence changed")
    expected_artifacts = {
        (
            artifact["id"],
            artifact["name"],
            artifact["size_in_bytes"],
            artifact["digest"],
        )
        for artifact in profile["artifacts"]
    }
    actual_artifacts = {
        (
            artifact.get("id"),
            artifact.get("name"),
            artifact.get("size_in_bytes"),
            artifact.get("digest"),
        )
        for artifact in artifacts
        if artifact.get("expired") is False
        and (artifact.get("workflow_run") or {}).get("id") == profile["run_id"]
        and (artifact.get("workflow_run") or {}).get("head_sha")
        == profile["control_sha"]
    }
    if actual_artifacts != expected_artifacts or len(artifacts) != len(
        expected_artifacts
    ):
        raise AuthorizationError("copied-profile artifact evidence changed or expired")
    metadata = profile["metadata"]
    if set(metadata_files) != {
        metadata["manifest_file"],
        metadata["release_notes_file"],
    }:
        raise AuthorizationError("copied-profile metadata artifact contents changed")
    for filename, digest_key in (
        (metadata["manifest_file"], "manifest_sha256"),
        (metadata["release_notes_file"], "release_notes_sha256"),
    ):
        digest = hashlib.sha256(metadata_files[filename]).hexdigest()
        if digest != metadata[digest_key]:
            raise AuthorizationError(f"copied-profile {filename} digest changed")
    manifest = json.loads(metadata_files[metadata["manifest_file"]])
    expected_manifest = {
        "commit": head_sha,
        "upstream_tag": metadata["upstream_tag"],
        "upstream_tag_commit": metadata["upstream_tag_commit"],
        "upstream_version": metadata["upstream_version"],
    }
    actual_manifest = {key: manifest.get(key) for key in expected_manifest}
    if actual_manifest != expected_manifest:
        raise AuthorizationError("copied-profile manifest identity changed")


def read_metadata_artifact(run_id: int, artifact_name: str) -> dict[str, bytes]:
    with tempfile.TemporaryDirectory() as temporary_directory:
        destination = pathlib.Path(temporary_directory)
        run(
            [
                "gh",
                "run",
                "download",
                str(run_id),
                "--repo",
                REPOSITORY,
                "--name",
                artifact_name,
                "--dir",
                str(destination),
            ]
        )
        if any(path.is_dir() for path in destination.iterdir()):
            raise AuthorizationError("metadata artifact contains a directory")
        return {
            path.name: path.read_bytes()
            for path in destination.iterdir()
            if path.is_file()
        }


def find_ordinary_evidence(
    head_sha: str, head_branch: str, ordinary: dict[str, Any] | None
) -> tuple[int, int]:
    response = gh_json(
        f"repos/{REPOSITORY}/commits/{head_sha}/check-runs"
        f"?check_name={quote(ORDINARY_JOB)}&per_page=100"
    )
    checks = response.get("check_runs") if isinstance(response, dict) else None
    if not isinstance(checks, list):
        raise AuthorizationError("check-runs response is invalid")
    failures: list[str] = []
    for check in checks:
        if not isinstance(check, dict) or check.get("name") != ORDINARY_JOB:
            continue
        try:
            run_id, job_id = parse_check_ids(check)
            if ordinary is not None and (
                run_id != ordinary["run_id"] or job_id != ordinary["job_id"]
            ):
                continue
            workflow_run = gh_json(
                f"repos/{REPOSITORY}/actions/runs/{run_id}"
            )
            job = gh_json(f"repos/{REPOSITORY}/actions/jobs/{job_id}")
            return validate_ordinary_evidence(
                head_sha=head_sha,
                check=check,
                workflow_run=workflow_run,
                job=job,
                head_branch=head_branch,
                expected_run_id=ordinary["run_id"] if ordinary else None,
                expected_job_id=ordinary["job_id"] if ordinary else None,
            )
        except AuthorizationError as error:
            failures.append(str(error))
    detail = f": {'; '.join(failures)}" if failures else ""
    raise AuthorizationError(f"no trusted ordinary compatibility check found{detail}")


def validate_product_base(
    control_root: pathlib.Path,
    release_sha: str,
    metadata: dict[str, Any] | None,
) -> None:
    raw = git(
        control_root,
        "show",
        f"{release_sha}:.github/rp-stable-base.json",
    )
    base = json.loads(raw)
    if base.get("upstream_repository") != "zed-industries/zed":
        raise AuthorizationError("product source has an invalid upstream repository")
    require_sha(base.get("upstream_tag_commit"), "product upstream SHA")
    if base.get("upstream_tag") != f"v{base.get('upstream_version')}":
        raise AuthorizationError("product upstream tag and version do not match")
    if metadata is not None:
        expected = {
            "upstream_tag": metadata["upstream_tag"],
            "upstream_tag_commit": metadata["upstream_tag_commit"],
            "upstream_version": metadata["upstream_version"],
        }
        actual = {key: base.get(key) for key in expected}
        if actual != expected:
            raise AuthorizationError(
                "product upstream identity does not match the authorization"
            )


def validate_committed_records(control_root: pathlib.Path) -> None:
    directory = control_root / AUTHORIZATION_DIRECTORY
    if not directory.is_dir():
        raise AuthorizationError(f"missing authorization directory {directory}")
    for path in sorted(directory.glob("*.json")):
        release_sha = require_sha(path.stem, f"{path} filename")
        record = json.loads(path.read_text(encoding="utf-8"))
        pull = record.get("pull_request") if isinstance(record, dict) else None
        head_sha = require_sha(
            pull.get("head_sha") if isinstance(pull, dict) else None,
            f"{path} PR head SHA",
        )
        validate_authorization_record(record, release_sha, head_sha)


def validate_workflow_promotion_record(
    record: Any, promotion_path: pathlib.Path | None = None
) -> dict[str, Any]:
    record = require_keys(
        record,
        {
            "schema_version",
            "repository",
            "control_authorization",
            "transition_manifest",
            "transition_validator",
            "release_transition",
        },
        "workflow promotion",
    )
    if record["schema_version"] != 1 or record["repository"] != REPOSITORY:
        raise AuthorizationError("workflow promotion schema or repository is invalid")

    control = require_keys(
        record["control_authorization"],
        {"ref", "commit_sha", "pull_request"},
        "workflow promotion control authorization",
    )
    if control["ref"] != PROFILE_CONTROL_REF:
        raise AuthorizationError("workflow promotion control ref is invalid")
    authorization_sha = require_sha(
        control["commit_sha"], "workflow promotion control commit"
    )
    authorization_pull = require_keys(
        control["pull_request"],
        {"number", "head_sha", "merge_sha"},
        "workflow promotion control pull request",
    )
    require_positive_integer(
        authorization_pull["number"], "workflow promotion control PR number"
    )
    if (
        require_sha(
            authorization_pull["head_sha"],
            "workflow promotion control PR head",
        )
        != authorization_sha
    ):
        raise AuthorizationError(
            "workflow promotion control commit must equal its PR head"
        )
    require_sha(
        authorization_pull["merge_sha"],
        "workflow promotion control PR merge",
    )

    release = require_keys(
        record["release_transition"],
        {
            "previous_tip",
            "merge_sha",
            "head_sha",
            "previous_workflow_blob_sha",
            "promoted_workflow_blob_sha",
        },
        "workflow promotion release transition",
    )
    for key in (
        "previous_tip",
        "merge_sha",
        "head_sha",
        "previous_workflow_blob_sha",
        "promoted_workflow_blob_sha",
    ):
        require_sha(release[key], f"workflow promotion {key}")
    if (
        release["previous_workflow_blob_sha"]
        == release["promoted_workflow_blob_sha"]
    ):
        raise AuthorizationError("workflow promotion must change the workflow blob")
    if promotion_path is not None and promotion_path.stem != release["merge_sha"]:
        raise AuthorizationError(
            "workflow promotion filename must equal its release merge SHA"
        )

    manifest = require_keys(
        record["transition_manifest"],
        {"path", "blob_sha"},
        "workflow promotion transition manifest",
    )
    expected_manifest_path = str(
        TRANSITION_MANIFEST_DIRECTORY
        / f"{release['promoted_workflow_blob_sha']}.json"
    )
    if manifest["path"] != expected_manifest_path:
        raise AuthorizationError("workflow promotion manifest path is invalid")
    require_sha(manifest["blob_sha"], "workflow promotion manifest blob")

    validator = require_keys(
        record["transition_validator"],
        {"path", "blob_sha"},
        "workflow promotion transition validator",
    )
    if validator["path"] != TRANSITION_VALIDATOR_PATH:
        raise AuthorizationError("workflow promotion validator path is invalid")
    require_sha(validator["blob_sha"], "workflow promotion validator blob")
    return record


def load_workflow_promotions(control_root: pathlib.Path) -> list[dict[str, Any]]:
    directory = control_root / WORKFLOW_PROMOTION_DIRECTORY
    if not directory.is_dir():
        raise AuthorizationError(f"missing workflow promotion directory {directory}")
    paths = sorted(directory.iterdir())
    if not paths:
        raise AuthorizationError("workflow promotion directory is empty")
    promotions = []
    for path in paths:
        if not path.is_file() or path.suffix != ".json":
            raise AuthorizationError(f"unexpected workflow promotion entry {path}")
        promotions.append(
            validate_workflow_promotion_record(
                json.loads(path.read_text(encoding="utf-8")), path
            )
        )
    return promotions


def release_workflow_transitions(
    control_root: pathlib.Path, release_tip: str
) -> list[dict[str, str]]:
    history = git(
        control_root,
        "rev-list",
        "--first-parent",
        "--reverse",
        f"{INITIAL_WORKFLOW_RELEASE_SHA}^..{release_tip}",
    ).splitlines()
    if not history or history[0] != INITIAL_WORKFLOW_RELEASE_SHA:
        raise AuthorizationError(
            "release history does not contain the audited initial workflow release"
        )
    previous_sha = history[0]
    previous_blob = git(
        control_root, "rev-parse", f"{previous_sha}:{ORDINARY_WORKFLOW}"
    )
    if previous_blob != INITIAL_ORDINARY_WORKFLOW_BLOB_SHA:
        raise AuthorizationError("audited initial ordinary workflow blob changed")

    transitions = []
    for commit_sha in history[1:]:
        current_blob = git(
            control_root, "rev-parse", f"{commit_sha}:{ORDINARY_WORKFLOW}"
        )
        if current_blob != previous_blob:
            parents = git(
                control_root, "rev-list", "--parents", "-n", "1", commit_sha
            ).split()
            if (
                len(parents) != 3
                or parents[0] != commit_sha
                or parents[1] != previous_sha
            ):
                raise AuthorizationError(
                    "ordinary workflow changed outside a two-parent release merge"
                )
            transitions.append(
                {
                    "previous_tip": previous_sha,
                    "merge_sha": commit_sha,
                    "head_sha": parents[2],
                    "previous_workflow_blob_sha": previous_blob,
                    "promoted_workflow_blob_sha": current_blob,
                }
            )
        previous_sha = commit_sha
        previous_blob = current_blob
    return transitions


def require_ancestor(
    control_root: pathlib.Path, ancestor: str, descendant: str, source: str
) -> None:
    try:
        git(control_root, "merge-base", "--is-ancestor", ancestor, descendant)
    except AuthorizationError as error:
        raise AuthorizationError(
            f"{source} {ancestor} is not reachable from {descendant}"
        ) from error


def validate_control_pull_request(
    pull: Any,
    *,
    number: int,
    head_sha: str,
    merge_sha: str | None,
) -> str:
    if not isinstance(pull, dict):
        raise AuthorizationError(f"control PR #{number} response is invalid")
    base = pull.get("base") or {}
    head = pull.get("head") or {}
    actual_merge_sha = require_sha(
        pull.get("merge_commit_sha"), f"control PR #{number} merge"
    )
    if (
        pull.get("number") != number
        or pull.get("merged_at") is None
        or base.get("ref") != PROFILE_CONTROL_REF.removeprefix("refs/heads/")
        or (base.get("repo") or {}).get("full_name") != REPOSITORY
        or head.get("sha") != head_sha
        or (head.get("repo") or {}).get("full_name") != REPOSITORY
        or (merge_sha is not None and actual_merge_sha != merge_sha)
    ):
        raise AuthorizationError(f"control PR #{number} identity is not trusted")
    return actual_merge_sha


def validate_workflow_promotions(
    control_root: pathlib.Path,
    release_tip: str,
    release_remote: str = "origin",
) -> None:
    promotions = load_workflow_promotions(control_root)
    transitions = release_workflow_transitions(control_root, release_tip)

    by_merge: dict[str, list[dict[str, Any]]] = {}
    authorization_commits: set[str] = set()
    manifest_blobs: set[str] = set()
    transition_pairs: set[tuple[str, str]] = set()
    for promotion in promotions:
        release = promotion["release_transition"]
        by_merge.setdefault(release["merge_sha"], []).append(promotion)
        authorization_sha = promotion["control_authorization"]["commit_sha"]
        manifest_blob = promotion["transition_manifest"]["blob_sha"]
        transition_pair = (
            release["previous_workflow_blob_sha"],
            release["promoted_workflow_blob_sha"],
        )
        if (
            authorization_sha in authorization_commits
            or manifest_blob in manifest_blobs
            or transition_pair in transition_pairs
        ):
            raise AuthorizationError("workflow promotion was duplicated or replayed")
        authorization_commits.add(authorization_sha)
        manifest_blobs.add(manifest_blob)
        transition_pairs.add(transition_pair)

    matched_promotions: list[dict[str, Any]] = []
    for transition in transitions:
        matches = by_merge.pop(transition["merge_sha"], [])
        if len(matches) != 1 or matches[0]["release_transition"] != transition:
            raise AuthorizationError(
                "ordinary workflow transition does not have exactly one promotion"
            )
        matched_promotions.append(matches[0])
    if by_merge:
        raise AuthorizationError("workflow promotion targets an unrelated transition")
    if not matched_promotions:
        raise AuthorizationError(
            "release history has no promoted ordinary workflow transition"
        )

    latest_validator = matched_promotions[-1]["transition_validator"]
    tracked_validator_blob = git(
        control_root,
        "rev-parse",
        f"{release_tip}:{latest_validator['path']}",
    )
    local_validator_blob = git(
        control_root,
        "hash-object",
        latest_validator["path"],
    )
    if (
        tracked_validator_blob != latest_validator["blob_sha"]
        or local_validator_blob != latest_validator["blob_sha"]
    ):
        raise AuthorizationError(
            "local workflow transition validator blob changed"
        )

    remote_ref = "refs/remotes/origin/rp-control-workflow-promotions"
    git(
        control_root,
        "fetch",
        "--force",
        "--no-tags",
        release_remote,
        f"+{PROFILE_CONTROL_REF}:{remote_ref}",
    )
    live_control_sha = git(
        control_root, "rev-parse", "--verify", f"{remote_ref}^{{commit}}"
    )

    for promotion in promotions:
        control = promotion["control_authorization"]
        authorization_sha = control["commit_sha"]
        require_ancestor(
            control_root,
            authorization_sha,
            live_control_sha,
            "workflow promotion authorization commit",
        )
        authorization_pull = control["pull_request"]
        authorization_merge_sha = validate_control_pull_request(
            gh_json(
                f"repos/{REPOSITORY}/pulls/{authorization_pull['number']}"
            ),
            number=authorization_pull["number"],
            head_sha=authorization_pull["head_sha"],
            merge_sha=authorization_pull["merge_sha"],
        )
        require_ancestor(
            control_root,
            authorization_merge_sha,
            live_control_sha,
            "workflow promotion authorization merge",
        )

        manifest_identity = promotion["transition_manifest"]
        validator_identity = promotion["transition_validator"]
        remote_manifest_blob = git(
            control_root,
            "rev-parse",
            f"{authorization_sha}:{manifest_identity['path']}",
        )
        remote_validator_blob = git(
            control_root,
            "rev-parse",
            f"{authorization_sha}:{validator_identity['path']}",
        )
        if remote_manifest_blob != manifest_identity["blob_sha"]:
            raise AuthorizationError("workflow promotion manifest blob changed")
        if remote_validator_blob != validator_identity["blob_sha"]:
            raise AuthorizationError("workflow promotion validator blob changed")

        remote_manifest = validate_transition_record(
            json.loads(
                git(
                    control_root,
                    "show",
                    f"{authorization_sha}:{manifest_identity['path']}",
                )
            )
        )
        release = promotion["release_transition"]
        if (
            remote_manifest["repository"] != promotion["repository"]
            or remote_manifest["workflow_path"] != ORDINARY_WORKFLOW
            or remote_manifest["previous_workflow_blob_sha"]
            != release["previous_workflow_blob_sha"]
            or remote_manifest["promoted_workflow_blob_sha"]
            != release["promoted_workflow_blob_sha"]
            or remote_manifest["release_transition"]
            != {
                "previous_tip": release["previous_tip"],
                "merge_sha": release["merge_sha"],
                "head_sha": release["head_sha"],
            }
            or remote_manifest["reviewed_control"]["ref"] != control["ref"]
        ):
            raise AuthorizationError(
                "workflow promotion does not match its remote transition manifest"
            )
        reviewed_remote_ref = (
            "refs/remotes/origin/rp-reviewed-control-workflow-promotions"
        )
        reviewed_control_ref = remote_manifest["reviewed_control"]["ref"]
        git(
            control_root,
            "fetch",
            "--force",
            "--no-tags",
            release_remote,
            f"+{reviewed_control_ref}:{reviewed_remote_ref}",
        )
        git(
            control_root,
            "rev-parse",
            "--verify",
            f"{reviewed_remote_ref}^{{commit}}",
        )
        validate_transition_topology(control_root, remote_manifest)
        validate_reviewed_control_topology(
            control_root, remote_manifest, reviewed_remote_ref
        )
        for reviewed_pull in remote_manifest["reviewed_control"]["pull_requests"]:
            validate_control_pull_request(
                gh_json(
                    f"repos/{REPOSITORY}/pulls/{reviewed_pull['number']}"
                ),
                number=reviewed_pull["number"],
                head_sha=reviewed_pull["head_sha"],
                merge_sha=reviewed_pull["merge_sha"],
            )
            require_ancestor(
                control_root,
                reviewed_pull["merge_sha"],
                authorization_sha,
                f"reviewed control PR #{reviewed_pull['number']} merge",
            )


def write_results(
    path: pathlib.Path | None,
    values: dict[str, str | int],
) -> None:
    if path is None:
        return
    with path.open("a", encoding="utf-8") as output:
        for key, value in values.items():
            output.write(f"{key}={value}\n")


def append_summary(
    path: pathlib.Path | None,
    *,
    event_name: str,
    control_sha: str,
    release_sha: str,
    head_sha: str,
    pull_number: int,
    ordinary_run_id: int,
    ordinary_job_id: int,
    profile: dict[str, Any] | None,
) -> None:
    if path is None:
        return
    lines = [
        "## RP release authorization",
        "",
        f"- Event: `{event_name}`",
        f"- Control SHA: `{control_sha}`",
        f"- Release merge SHA: `{release_sha}`",
        f"- Pull request: `#{pull_number}`",
        f"- Validated PR head: `{head_sha}`",
        f"- Ordinary compatibility: run `{ordinary_run_id}`, job `{ordinary_job_id}`",
    ]
    if profile is None:
        lines.append("- Copied-profile attestation: not required for push validation")
    else:
        lines.extend(
            [
                f"- Copied-profile run: `{profile['run_id']}`",
                f"- Copied-profile control SHA: `{profile['control_sha']}`",
                f"- Copied-profile workflow blob: `{profile['workflow_blob_sha']}`",
                f"- Upstream stable: `{profile['metadata']['upstream_tag']}` / "
                f"`{profile['metadata']['upstream_tag_commit']}`",
            ]
        )
    with path.open("a", encoding="utf-8") as summary:
        summary.write("\n".join(lines) + "\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--control-root", type=pathlib.Path, required=True)
    parser.add_argument("--event-name", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--ref", required=True)
    parser.add_argument("--actor", required=True)
    parser.add_argument("--triggering-actor", required=True)
    parser.add_argument("--control-sha", required=True)
    parser.add_argument("--release-sha", required=True)
    parser.add_argument("--release-remote", default="origin")
    parser.add_argument("--validated-head-sha")
    parser.add_argument("--require-attestation", action="store_true")
    parser.add_argument("--github-output", type=pathlib.Path)
    parser.add_argument("--step-summary", type=pathlib.Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        expected_head_sha = args.validated_head_sha or None
        validate_event(
            event_name=args.event_name,
            repository=args.repository,
            ref=args.ref,
            actor=args.actor,
            triggering_actor=args.triggering_actor,
            control_sha=args.control_sha,
            release_sha=args.release_sha,
            expected_head_sha=expected_head_sha,
        )
        validate_release_history(
            args.control_root,
            args.control_sha,
            args.release_sha,
            args.release_remote,
        )
        validate_workflow_promotions(
            args.control_root,
            args.control_sha,
            args.release_remote,
        )
        if args.event_name == "push":
            validate_committed_records(args.control_root)
        pulls = gh_json(
            f"repos/{REPOSITORY}/commits/{args.release_sha}/pulls?per_page=100"
        )
        pull = select_pull_request(pulls, args.release_sha, expected_head_sha)
        head_sha = pull["head"]["sha"]
        record = None
        profile = None
        ordinary = None
        require_attestation = (
            args.require_attestation or args.event_name == "workflow_dispatch"
        )
        if require_attestation:
            record_path = (
                args.control_root
                / AUTHORIZATION_DIRECTORY
                / f"{args.release_sha}.json"
            )
            if not record_path.is_file():
                raise AuthorizationError(
                    f"missing release authorization {record_path}"
                )
            record = validate_authorization_record(
                json.loads(record_path.read_text(encoding="utf-8")),
                args.release_sha,
                head_sha,
            )
            if record["pull_request"]["number"] != pull["number"]:
                raise AuthorizationError("authorization PR number changed")
            ordinary = record["ordinary_compatibility"]
            profile = record["copied_profile_attestation"]
        validate_merge_identity(
            args.control_root,
            args.release_sha,
            head_sha,
            ordinary["workflow_blob_sha"] if ordinary else None,
        )
        ordinary_run_id, ordinary_job_id = find_ordinary_evidence(
            head_sha, pull["head"]["ref"], ordinary
        )
        validate_product_base(
            args.control_root,
            args.release_sha,
            profile["metadata"] if profile else None,
        )
        if profile is not None:
            encoded_path = quote(PROFILE_WORKFLOW, safe="/")
            contents = gh_json(
                f"repos/{REPOSITORY}/contents/{encoded_path}"
                f"?ref={profile['control_sha']}"
            )
            if not isinstance(contents, dict):
                raise AuthorizationError(
                    "copied-profile workflow contents response is invalid"
                )
            workflow_run = gh_json(
                f"repos/{REPOSITORY}/actions/runs/{profile['run_id']}"
            )
            jobs_response = gh_json(
                f"repos/{REPOSITORY}/actions/runs/{profile['run_id']}"
                "/jobs?per_page=100"
            )
            artifacts_response = gh_json(
                f"repos/{REPOSITORY}/actions/runs/{profile['run_id']}"
                "/artifacts?per_page=100"
            )
            if not isinstance(jobs_response, dict) or not isinstance(
                artifacts_response, dict
            ):
                raise AuthorizationError(
                    "copied-profile jobs or artifacts response is invalid"
                )
            metadata_artifact = next(
                artifact
                for artifact in profile["artifacts"]
                if artifact["id"] == profile["metadata"]["artifact_id"]
            )
            metadata_files = read_metadata_artifact(
                profile["run_id"], metadata_artifact["name"]
            )
            validate_profile_evidence(
                profile=profile,
                head_sha=head_sha,
                workflow_blob_sha=contents.get("sha", ""),
                workflow_run=workflow_run,
                jobs=jobs_response.get("jobs", []),
                artifacts=artifacts_response.get("artifacts", []),
                metadata_files=metadata_files,
            )
        outputs = {
            "control_sha": args.control_sha,
            "release_sha": args.release_sha,
            "head_sha": head_sha,
            "pull_number": pull["number"],
            "ordinary_run_id": ordinary_run_id,
            "ordinary_job_id": ordinary_job_id,
        }
        write_results(args.github_output, outputs)
        append_summary(
            args.step_summary,
            event_name=args.event_name,
            control_sha=args.control_sha,
            release_sha=args.release_sha,
            head_sha=head_sha,
            pull_number=pull["number"],
            ordinary_run_id=ordinary_run_id,
            ordinary_job_id=ordinary_job_id,
            profile=profile,
        )
    except (
        AuthorizationError,
        WorkflowTransitionError,
        json.JSONDecodeError,
        OSError,
        StopIteration,
    ) as error:
        print(f"RP release authorization failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
