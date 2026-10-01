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


def initialize_repo(repo: pathlib.Path) -> None:
    subprocess.run(["git", "init", "-q", repo], check=True)
    subprocess.run(
        ["git", "-C", repo, "config", "user.email", "rp@example.invalid"],
        check=True,
    )
    subprocess.run(
        ["git", "-C", repo, "config", "user.name", "RP Test"], check=True
    )


def control_chain_record(
    repo: pathlib.Path,
    first_workflow: str,
    second_workflow: str,
    expected_promoted_blob: str | None = None,
) -> dict[str, object]:
    initialize_repo(repo)
    workflow = repo / ".github/workflows/rp_stable_sync.yml"
    workflow.parent.mkdir(parents=True)
    workflow.write_text("old\n", encoding="utf-8")
    old_tip = commit(repo, "old")
    old_blob = git(
        repo, "rev-parse", f"{old_tip}:.github/workflows/rp_stable_sync.yml"
    )

    subprocess.run(
        ["git", "-C", repo, "switch", "-qc", "review-one"], check=True
    )
    workflow.write_text(first_workflow, encoding="utf-8")
    (repo / "review-one.txt").write_text("review one\n", encoding="utf-8")
    first_head = commit(repo, "review one")
    subprocess.run(["git", "-C", repo, "switch", "-q", "master"], check=True)
    subprocess.run(
        ["git", "-C", repo, "merge", "--no-ff", "-qm", "merge one", first_head],
        check=True,
    )
    first_merge = git(repo, "rev-parse", "HEAD")
    actual_promoted_blob = git(
        repo,
        "rev-parse",
        f"{first_head}:.github/workflows/rp_stable_sync.yml",
    )

    subprocess.run(
        ["git", "-C", repo, "switch", "-qc", "review-two"], check=True
    )
    workflow.write_text(second_workflow, encoding="utf-8")
    (repo / "review-two.txt").write_text("review two\n", encoding="utf-8")
    second_head = commit(repo, "review two")
    subprocess.run(["git", "-C", repo, "switch", "-q", "master"], check=True)
    subprocess.run(
        ["git", "-C", repo, "merge", "--no-ff", "-qm", "merge two", second_head],
        check=True,
    )
    second_merge = git(repo, "rev-parse", "HEAD")
    promoted_blob = expected_promoted_blob or actual_promoted_blob
    return {
        "schema_version": 1,
        "repository": "JonathonRP/zed",
        "workflow_path": ".github/workflows/rp_stable_sync.yml",
        "previous_workflow_blob_sha": old_blob,
        "promoted_workflow_blob_sha": promoted_blob,
        "release_transition": {
            "previous_tip": old_tip,
            "merge_sha": first_merge,
            "head_sha": first_head,
        },
        "reviewed_control": {
            "ref": "refs/heads/automation/rp-control",
            "workflow_path": ".github/workflows/rp_stable_sync.yml",
            "previous_workflow_blob_sha": old_blob,
            "promoted_workflow_blob_sha": promoted_blob,
            "reviewed_tip": second_merge,
            "pull_requests": [
                {
                    "number": 1,
                    "head_sha": first_head,
                    "merge_sha": first_merge,
                },
                {
                    "number": 2,
                    "head_sha": second_head,
                    "merge_sha": second_merge,
                },
            ],
        },
    }


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
        validate_reviewed_control_topology(
            repo, transition, "personal/automation/rp-control"
        )

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
            validate_reviewed_control_topology(
                repo, transition, "personal/automation/rp-control"
            )

    def test_unrelated_sequential_control_prs_are_rejected(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        transition = load_transition(repo / TRANSITION_PATH)
        transition["reviewed_control"]["promoted_workflow_blob_sha"] = (
            transition["reviewed_control"]["previous_workflow_blob_sha"]
        )

        with self.assertRaisesRegex(
            WorkflowTransitionError, "PR #22 did not retain"
        ):
            validate_reviewed_control_topology(
                repo, transition, "personal/automation/rp-control"
            )

    def test_old_blob_at_reviewed_head_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            transition = control_chain_record(
                repo,
                "old\n",
                "old\n",
                expected_promoted_blob="1" * 40,
            )

            with self.assertRaisesRegex(
                WorkflowTransitionError, "PR #1 did not retain"
            ):
                validate_reviewed_control_topology(
                    repo, transition, "master"
                )

    def test_promoted_blob_not_retained_by_later_merge_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            transition = control_chain_record(repo, "new\n", "newer\n")

            with self.assertRaisesRegex(
                WorkflowTransitionError, "PR #2 did not retain"
            ):
                validate_reviewed_control_topology(
                    repo, transition, "master"
                )

    def test_stale_control_ref_is_rejected(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        transition = load_transition(repo / TRANSITION_PATH)

        with self.assertRaisesRegex(
            WorkflowTransitionError, "not contained in the live control ref"
        ):
            validate_reviewed_control_topology(
                repo,
                transition,
                "f5a6fdb96b042777a8d08d70c3ef7a3d04edfc1a",
            )

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
            initialize_repo(repo)
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
                    "workflow_path": ".github/workflows/rp_stable_sync.yml",
                    "previous_workflow_blob_sha": git(
                        repo,
                        "rev-parse",
                        f"{old_tip}:.github/workflows/rp_stable_sync.yml",
                    ),
                    "promoted_workflow_blob_sha": git(
                        repo,
                        "rev-parse",
                        f"{head}:.github/workflows/rp_stable_sync.yml",
                    ),
                    "reviewed_tip": merge,
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
