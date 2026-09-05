import copy
import hashlib
import json
import pathlib
import subprocess
import tempfile
import unittest

from rp_release_authorization import (
    AuthorizationError,
    ORDINARY_JOB,
    ORDINARY_WORKFLOW,
    PROFILE_CONTROL_REF,
    PROFILE_WORKFLOW,
    REPOSITORY,
    select_pull_request,
    validate_authorization_record,
    validate_committed_records,
    validate_event,
    validate_release_history,
    validate_merge_identity,
    validate_ordinary_evidence,
    validate_profile_evidence,
)


RELEASE_SHA = "5b0c427ee3a4956527c652ff7ea4c156113be2b1"
HEAD_SHA = "407ca4bc4ff66ee6f406d2e70fa8ce5976523c22"
CONTROL_SHA = "f5a6fdb96b042777a8d08d70c3ef7a3d04edfc1a"
WORKFLOW_BLOB_SHA = "0d2da4ee29fd3262a197503d5a69185cea8b93db"
ORDINARY_RUN_ID = 33908384435
ORDINARY_JOB_ID = 101138690327
PROFILE_RUN_ID = 33909407320


def pull_request() -> dict:
    return {
        "number": 15,
        "merged_at": "2026-09-04T19:07:00Z",
        "merge_commit_sha": RELEASE_SHA,
        "base": {
            "ref": "release/rp-stable",
            "repo": {"full_name": REPOSITORY},
        },
        "head": {
            "sha": HEAD_SHA,
            "repo": {"full_name": REPOSITORY},
        },
    }


def ordinary_check() -> dict:
    return {
        "name": ORDINARY_JOB,
        "head_sha": HEAD_SHA,
        "status": "completed",
        "conclusion": "success",
        "app": {"slug": "github-actions"},
        "details_url": (
            f"https://github.com/{REPOSITORY}/actions/runs/"
            f"{ORDINARY_RUN_ID}/job/{ORDINARY_JOB_ID}"
        ),
    }


def ordinary_run() -> dict:
    return {
        "id": ORDINARY_RUN_ID,
        "event": "pull_request",
        "path": ORDINARY_WORKFLOW,
        "head_branch": "jonathonrp-track-zed-stable",
        "head_sha": HEAD_SHA,
        "status": "completed",
        "conclusion": "success",
        "repository": {"full_name": REPOSITORY},
        "head_repository": {"full_name": REPOSITORY},
    }


def ordinary_job() -> dict:
    return {
        "id": ORDINARY_JOB_ID,
        "name": ORDINARY_JOB,
        "head_sha": HEAD_SHA,
        "status": "completed",
        "conclusion": "success",
    }


def authorization_record() -> dict:
    repo = pathlib.Path(__file__).resolve().parents[1]
    return json.loads(
        (
            repo / ".github/rp-release-authorizations" / f"{RELEASE_SHA}.json"
        ).read_text(encoding="utf-8")
    )


def profile_run(profile: dict) -> dict:
    return {
        "id": PROFILE_RUN_ID,
        "event": "workflow_dispatch",
        "path": PROFILE_WORKFLOW,
        "head_branch": PROFILE_CONTROL_REF.removeprefix("refs/heads/"),
        "head_sha": CONTROL_SHA,
        "status": "completed",
        "conclusion": "success",
        "actor": {"login": "JonathonRP"},
        "triggering_actor": {"login": "JonathonRP"},
        "repository": {"full_name": REPOSITORY},
        "head_repository": {"full_name": REPOSITORY},
    }


def profile_jobs(profile: dict) -> list[dict]:
    return [
        {
            "id": job["id"],
            "name": job["name"],
            "head_sha": CONTROL_SHA,
            "status": "completed",
            "conclusion": "success",
        }
        for job in profile["jobs"]
    ]


def profile_artifacts(profile: dict) -> list[dict]:
    return [
        {
            **artifact,
            "expired": False,
            "workflow_run": {
                "id": PROFILE_RUN_ID,
                "head_sha": CONTROL_SHA,
            },
        }
        for artifact in profile["artifacts"]
    ]


class EventValidationTests(unittest.TestCase):
    def test_accepts_push_and_owner_dispatch(self):
        validate_event(
            event_name="push",
            repository=REPOSITORY,
            ref="refs/heads/release/rp-stable",
            actor="github-actions[bot]",
            triggering_actor="github-actions[bot]",
            control_sha=RELEASE_SHA,
            release_sha=RELEASE_SHA,
            expected_head_sha=None,
        )
        validate_event(
            event_name="workflow_dispatch",
            repository=REPOSITORY,
            ref="refs/heads/release/rp-stable",
            actor="JonathonRP",
            triggering_actor="JonathonRP",
            control_sha="a" * 40,
            release_sha=RELEASE_SHA,
            expected_head_sha=HEAD_SHA,
        )

    def test_rejects_untrusted_context_and_abbreviated_sha(self):
        base = {
            "event_name": "workflow_dispatch",
            "repository": REPOSITORY,
            "ref": "refs/heads/release/rp-stable",
            "actor": "JonathonRP",
            "triggering_actor": "JonathonRP",
            "control_sha": "a" * 40,
            "release_sha": RELEASE_SHA,
            "expected_head_sha": HEAD_SHA,
        }
        for mutation in (
            {"repository": "zed-industries/zed"},
            {"ref": "refs/tags/rp-stable-test"},
            {"actor": "another-user"},
            {"triggering_actor": "another-user"},
            {"release_sha": RELEASE_SHA[:12]},
            {"expected_head_sha": HEAD_SHA.upper()},
            {"expected_head_sha": None},
        ):
            with self.subTest(mutation=mutation), self.assertRaises(
                AuthorizationError
            ):
                validate_event(**(base | mutation))
        with self.assertRaises(AuthorizationError):
            validate_event(
                **(
                    base
                    | {
                        "event_name": "push",
                        "release_sha": RELEASE_SHA,
                        "control_sha": "a" * 40,
                        "expected_head_sha": None,
                    }
                )
            )


class PullRequestEvidenceTests(unittest.TestCase):
    def test_selects_exact_merged_release_pull_request(self):
        selected = select_pull_request([pull_request()], RELEASE_SHA, HEAD_SHA)
        self.assertEqual(selected["number"], 15)

    def test_rejects_wrong_or_ambiguous_pull_request(self):
        for mutation in (
            {"merged_at": None},
            {"merge_commit_sha": "a" * 40},
            {"base": {"ref": "main", "repo": {"full_name": REPOSITORY}}},
            {
                "head": {
                    "sha": HEAD_SHA,
                    "repo": {"full_name": "someone/zed"},
                }
            },
        ):
            with self.subTest(mutation=mutation), self.assertRaises(
                AuthorizationError
            ):
                select_pull_request([pull_request() | mutation], RELEASE_SHA, HEAD_SHA)
        with self.assertRaises(AuthorizationError):
            select_pull_request(
                [pull_request(), copy.deepcopy(pull_request())],
                RELEASE_SHA,
                HEAD_SHA,
            )

    def test_rejects_check_name_spoof_and_wrong_workflow(self):
        check = ordinary_check()
        run = ordinary_run()
        job = ordinary_job()
        self.assertEqual(
            validate_ordinary_evidence(
                head_sha=HEAD_SHA,
                check=check,
                workflow_run=run,
                job=job,
                head_branch="jonathonrp-track-zed-stable",
            ),
            (ORDINARY_RUN_ID, ORDINARY_JOB_ID),
        )
        for source, mutation in (
            ("check", {"app": {"slug": "external-ci"}}),
            ("check", {"head_sha": "a" * 40}),
            ("check", {"conclusion": "failure"}),
            ("check", {"details_url": "https://example.com/trusted-looking"}),
            ("run", {"path": ".github/workflows/another.yml"}),
            ("run", {"event": "workflow_dispatch"}),
            ("run", {"head_branch": "another-branch"}),
            ("run", {"head_repository": {"full_name": "someone/zed"}}),
            ("job", {"name": "spoofed"}),
            ("job", {"head_sha": "a" * 40}),
        ):
            values = {
                "check": copy.deepcopy(check),
                "run": copy.deepcopy(run),
                "job": copy.deepcopy(job),
            }
            values[source].update(mutation)
            with self.subTest(source=source, mutation=mutation), self.assertRaises(
                AuthorizationError
            ):
                validate_ordinary_evidence(
                    head_sha=HEAD_SHA,
                    check=values["check"],
                    workflow_run=values["run"],
                    job=values["job"],
                    head_branch="jonathonrp-track-zed-stable",
                )


class MergeIdentityTests(unittest.TestCase):
    def initialize_repository(self, root: pathlib.Path) -> tuple[str, str]:
        subprocess.run(["git", "init", "-q", root], check=True)
        subprocess.run(
            ["git", "-C", root, "config", "user.email", "rp@example.invalid"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", root, "config", "user.name", "RP Test"], check=True
        )
        workflow = root / ORDINARY_WORKFLOW
        workflow.parent.mkdir(parents=True)
        workflow.write_text("name: trusted\n", encoding="utf-8")
        subprocess.run(["git", "-C", root, "add", "."], check=True)
        subprocess.run(["git", "-C", root, "commit", "-qm", "base"], check=True)
        base = subprocess.check_output(
            ["git", "-C", root, "rev-parse", "HEAD"], text=True
        ).strip()
        subprocess.run(["git", "-C", root, "switch", "-qc", "feature"], check=True)
        (root / "product.txt").write_text("tested\n", encoding="utf-8")
        subprocess.run(["git", "-C", root, "add", "."], check=True)
        subprocess.run(["git", "-C", root, "commit", "-qm", "product"], check=True)
        head = subprocess.check_output(
            ["git", "-C", root, "rev-parse", "HEAD"], text=True
        ).strip()
        subprocess.run(["git", "-C", root, "switch", "-q", "-"], check=True)
        return base, head

    def test_requires_two_parent_tree_identical_merge(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            _, head = self.initialize_repository(repo)
            subprocess.run(
                ["git", "-C", repo, "merge", "--no-ff", "-qm", "merge", "feature"],
                check=True,
            )
            release = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            blob = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", f"{head}:{ORDINARY_WORKFLOW}"],
                text=True,
            ).strip()
            self.assertEqual(validate_merge_identity(repo, release, head, blob), blob)

    def test_rejects_merge_with_untested_base_content(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            _, head = self.initialize_repository(repo)
            (repo / "base-change.txt").write_text("untested\n", encoding="utf-8")
            subprocess.run(["git", "-C", repo, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", repo, "commit", "-qm", "base advanced"], check=True
            )
            subprocess.run(
                ["git", "-C", repo, "merge", "--no-ff", "-qm", "merge", "feature"],
                check=True,
            )
            release = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            with self.assertRaisesRegex(AuthorizationError, "tree differs"):
                validate_merge_identity(repo, release, head)

    def test_rejects_squash_merge_and_self_modified_workflow(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            _, head = self.initialize_repository(repo)
            subprocess.run(["git", "-C", repo, "merge", "--squash", "feature"], check=True)
            subprocess.run(["git", "-C", repo, "commit", "-qm", "squash"], check=True)
            release = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            with self.assertRaisesRegex(AuthorizationError, "two-parent merge"):
                validate_merge_identity(repo, release, head)

        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            _, _ = self.initialize_repository(repo)
            subprocess.run(["git", "-C", repo, "switch", "-q", "feature"], check=True)
            (repo / ORDINARY_WORKFLOW).write_text(
                "name: self-approved\n", encoding="utf-8"
            )
            subprocess.run(["git", "-C", repo, "commit", "-qam", "change check"], check=True)
            head = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            subprocess.run(["git", "-C", repo, "switch", "-q", "-"], check=True)
            subprocess.run(
                ["git", "-C", repo, "merge", "--no-ff", "-qm", "merge", "feature"],
                check=True,
            )
            release = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            modified_blob = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", f"{head}:{ORDINARY_WORKFLOW}"],
                text=True,
            ).strip()
            with self.assertRaisesRegex(AuthorizationError, "own ordinary"):
                validate_merge_identity(repo, release, head)
            with self.assertRaisesRegex(AuthorizationError, "own ordinary"):
                validate_merge_identity(repo, release, head, modified_blob)

    def test_release_history_requires_live_tip_and_first_parent(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            repo = root / "repo"
            remote = root / "remote.git"
            repo.mkdir()
            subprocess.run(["git", "init", "-q", repo], check=True)
            subprocess.run(["git", "init", "-q", "--bare", remote], check=True)
            subprocess.run(
                ["git", "-C", repo, "config", "user.email", "rp@example.invalid"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", repo, "config", "user.name", "RP Test"], check=True
            )
            (repo / "base.txt").write_text("base\n", encoding="utf-8")
            subprocess.run(["git", "-C", repo, "add", "."], check=True)
            subprocess.run(["git", "-C", repo, "commit", "-qm", "base"], check=True)
            subprocess.run(["git", "-C", repo, "switch", "-qc", "feature"], check=True)
            (repo / "feature.txt").write_text("feature\n", encoding="utf-8")
            subprocess.run(["git", "-C", repo, "add", "."], check=True)
            subprocess.run(["git", "-C", repo, "commit", "-qm", "feature"], check=True)
            side_parent = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            subprocess.run(["git", "-C", repo, "switch", "-q", "-"], check=True)
            subprocess.run(
                ["git", "-C", repo, "merge", "--no-ff", "-qm", "merge", "feature"],
                check=True,
            )
            control_sha = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            subprocess.run(
                ["git", "-C", repo, "remote", "add", "origin", str(remote)],
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    repo,
                    "push",
                    "-q",
                    "origin",
                    f"{control_sha}:refs/heads/release/rp-stable",
                ],
                check=True,
            )

            validate_release_history(repo, control_sha, control_sha)
            with self.assertRaisesRegex(AuthorizationError, "first-parent"):
                validate_release_history(repo, control_sha, side_parent)
            with self.assertRaisesRegex(AuthorizationError, "live RP stable tip"):
                validate_release_history(repo, "a" * 40, control_sha)


class CopiedProfileEvidenceTests(unittest.TestCase):
    def metadata_files(self, profile: dict) -> dict[str, bytes]:
        manifest = {
            "commit": HEAD_SHA,
            "upstream_tag": profile["metadata"]["upstream_tag"],
            "upstream_tag_commit": profile["metadata"]["upstream_tag_commit"],
            "upstream_version": profile["metadata"]["upstream_version"],
        }
        manifest_bytes = json.dumps(manifest).encode()
        notes_bytes = b"runtime evidence"
        profile["metadata"]["manifest_sha256"] = hashlib.sha256(
            manifest_bytes
        ).hexdigest()
        profile["metadata"]["release_notes_sha256"] = hashlib.sha256(
            notes_bytes
        ).hexdigest()
        return {
            profile["metadata"]["manifest_file"]: manifest_bytes,
            profile["metadata"]["release_notes_file"]: notes_bytes,
        }

    def test_accepts_source_bound_profile_evidence(self):
        record = authorization_record()
        profile = record["copied_profile_attestation"]
        files = self.metadata_files(profile)
        validate_profile_evidence(
            profile=profile,
            head_sha=HEAD_SHA,
            workflow_blob_sha=WORKFLOW_BLOB_SHA,
            workflow_run=profile_run(profile),
            jobs=profile_jobs(profile),
            artifacts=profile_artifacts(profile),
            metadata_files=files,
        )

    def test_rejects_changed_run_job_artifact_or_manifest(self):
        mutations = ("run", "job", "artifact", "manifest")
        for mutation in mutations:
            record = authorization_record()
            profile = record["copied_profile_attestation"]
            files = self.metadata_files(profile)
            run = profile_run(profile)
            jobs = profile_jobs(profile)
            artifacts = profile_artifacts(profile)
            if mutation == "run":
                run["head_sha"] = "a" * 40
            elif mutation == "job":
                jobs[0]["conclusion"] = "failure"
            elif mutation == "artifact":
                artifacts[0]["expired"] = True
            else:
                files[profile["metadata"]["manifest_file"]] += b"changed"
            with self.subTest(mutation=mutation), self.assertRaises(
                AuthorizationError
            ):
                validate_profile_evidence(
                    profile=profile,
                    head_sha=HEAD_SHA,
                    workflow_blob_sha=WORKFLOW_BLOB_SHA,
                    workflow_run=run,
                    jobs=jobs,
                    artifacts=artifacts,
                    metadata_files=files,
                )

    def test_record_is_exact_and_source_keyed(self):
        record = authorization_record()
        validate_authorization_record(record, RELEASE_SHA, HEAD_SHA)
        self.assertEqual(
            record["ordinary_compatibility"]["workflow_blob_sha"],
            "dbdb85382fa5a8a529313ca6c4d49e6eb5fec0f5",
        )
        self.assertEqual(
            record["copied_profile_attestation"]["metadata"]["manifest_sha256"],
            "c4fa662d38c2dce7d74950e56bbce8aad2ef91c29ca7bf25b0f1c2719bc2cee7",
        )
        for mutation in (
            {"release_merge_sha": "a" * 40},
            {"repository": "someone/zed"},
            {
                "pull_request": {
                    "number": 15,
                    "head_sha": "a" * 40,
                }
            },
        ):
            with self.subTest(mutation=mutation), self.assertRaises(
                AuthorizationError
            ):
                validate_authorization_record(
                    copy.deepcopy(record) | mutation, RELEASE_SHA, HEAD_SHA
                )

    def test_committed_record_filename_matches_release_sha(self):
        record = authorization_record()
        with tempfile.TemporaryDirectory() as temporary_directory:
            control = pathlib.Path(temporary_directory)
            directory = control / ".github/rp-release-authorizations"
            directory.mkdir(parents=True)
            (directory / f"{RELEASE_SHA}.json").write_text(
                json.dumps(record), encoding="utf-8"
            )
            validate_committed_records(control)
            (directory / f"{RELEASE_SHA}.json").rename(directory / f"{'a' * 40}.json")
            with self.assertRaises(AuthorizationError):
                validate_committed_records(control)


if __name__ == "__main__":
    unittest.main()
