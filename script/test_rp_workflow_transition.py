import copy
import pathlib
import subprocess
import tempfile
import unittest

from rp_workflow_transition import (
    WorkflowTransitionError,
    load_transition,
    validate_reviewed_control_topology,
    validate_transition_record,
    validate_transition_topology,
)


TRANSITION_PATH = pathlib.Path(
    ".github/rp-workflow-transitions/"
    "a3423fbb7d1e7aeaaff6be5ecc26d97cc6f8e181.json"
)


def git(repo: pathlib.Path, *arguments: str) -> str:
    return subprocess.check_output(
        ["git", "-C", repo, *arguments], text=True
    ).strip()


def commit(repo: pathlib.Path, message: str) -> str:
    subprocess.run(["git", "-C", repo, "add", "."], check=True)
    subprocess.run(["git", "-C", repo, "commit", "-qm", message], check=True)
    return git(repo, "rev-parse", "HEAD")


class WorkflowTransitionTests(unittest.TestCase):
    def test_committed_transition_binds_exact_production_identity(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        transition = load_transition(repo / TRANSITION_PATH)

        self.assertEqual(
            transition["previous_workflow_blob_sha"],
            "dbdb85382fa5a8a529313ca6c4d49e6eb5fec0f5",
        )
        self.assertEqual(
            transition["promoted_workflow_blob_sha"],
            "a3423fbb7d1e7aeaaff6be5ecc26d97cc6f8e181",
        )
        self.assertEqual(
            transition["release_transition"],
            {
                "previous_tip": "fb8f0d5fae8c8e800e627cc5127500ae33ba692d",
                "merge_sha": "b92faf43e29a167c1b82c45c578f0c259ea146c4",
                "head_sha": "50e987863da13c0982df225027314ce3de40b20a",
            },
        )
        validate_transition_topology(repo, transition)
        validate_reviewed_control_topology(repo, transition)

    def test_arbitrary_workflow_blob_is_rejected(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        transition = load_transition(repo / TRANSITION_PATH)
        transition["promoted_workflow_blob_sha"] = "0" * 40

        with self.assertRaisesRegex(
            WorkflowTransitionError, "promoted workflow blob changed"
        ):
            validate_transition_topology(repo, transition)

    def test_stale_release_transition_is_rejected(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        transition = load_transition(repo / TRANSITION_PATH)
        transition["release_transition"]["previous_tip"] = "0" * 40

        with self.assertRaisesRegex(
            WorkflowTransitionError, "merge topology changed"
        ):
            validate_transition_topology(repo, transition)

    def test_unreviewed_control_commit_is_rejected(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        transition = load_transition(repo / TRANSITION_PATH)
        transition["reviewed_control"]["pull_requests"][1]["head_sha"] = "0" * 40

        with self.assertRaisesRegex(
            WorkflowTransitionError, "PR #24 topology changed"
        ):
            validate_reviewed_control_topology(repo, transition)

    def test_record_rejects_extra_fields_and_duplicate_reviews(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        transition = load_transition(repo / TRANSITION_PATH)
        extra = copy.deepcopy(transition)
        extra["self_authorized"] = True
        with self.assertRaisesRegex(WorkflowTransitionError, "exactly"):
            validate_transition_record(extra)

        duplicate = copy.deepcopy(transition)
        duplicate["reviewed_control"]["pull_requests"].append(
            copy.deepcopy(
                duplicate["reviewed_control"]["pull_requests"][0]
            )
        )
        with self.assertRaisesRegex(WorkflowTransitionError, "number is invalid"):
            validate_transition_record(duplicate)

    def test_transition_cannot_be_replayed_for_another_merge(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            subprocess.run(["git", "init", "-q", repo], check=True)
            subprocess.run(
                ["git", "-C", repo, "config", "user.email", "rp@example.invalid"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", repo, "config", "user.name", "RP Test"], check=True
            )
            workflow = repo / ".github/workflows/rp_stable_sync.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text("old\n", encoding="utf-8")
            old_tip = commit(repo, "old")
            subprocess.run(
                ["git", "-C", repo, "switch", "-qc", "candidate"], check=True
            )
            workflow.write_text("new\n", encoding="utf-8")
            head = commit(repo, "new")
            subprocess.run(
                ["git", "-C", repo, "switch", "-q", "master"], check=True
            )
            subprocess.run(
                ["git", "-C", repo, "merge", "--no-ff", "-qm", "merge", head],
                check=True,
            )
            merge = git(repo, "rev-parse", "HEAD")
            record = {
                "schema_version": 1,
                "repository": "JonathonRP/zed",
                "workflow_path": ".github/workflows/rp_stable_sync.yml",
                "previous_workflow_blob_sha": git(
                    repo, "rev-parse", f"{old_tip}:.github/workflows/rp_stable_sync.yml"
                ),
                "promoted_workflow_blob_sha": git(
                    repo, "rev-parse", f"{head}:.github/workflows/rp_stable_sync.yml"
                ),
                "release_transition": {
                    "previous_tip": old_tip,
                    "merge_sha": merge,
                    "head_sha": head,
                },
                "reviewed_control": {
                    "ref": "refs/heads/automation/rp-control",
                    "pull_requests": [
                        {
                            "number": 1,
                            "head_sha": head,
                            "merge_sha": merge,
                        }
                    ],
                },
            }
            validate_transition_topology(repo, record)
            replay = copy.deepcopy(record)
            replay["release_transition"]["merge_sha"] = old_tip
            with self.assertRaisesRegex(
                WorkflowTransitionError, "merge topology changed"
            ):
                validate_transition_topology(repo, replay)


if __name__ == "__main__":
    unittest.main()
