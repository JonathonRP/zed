import copy
import json
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

import rp_release_authorization as authorization
from rp_release_authorization import (
    AuthorizationError,
    REPOSITORY,
    load_workflow_promotions,
    release_workflow_transitions,
    validate_control_pull_request,
    validate_workflow_promotion_record,
    validate_workflow_promotions,
)
from rp_workflow_transition import (
    WorkflowTransitionError,
    validate_reviewed_control_topology,
    validate_transition_topology,
)


RELEASE_MERGE = "b92faf43e29a167c1b82c45c578f0c259ea146c4"
RELEASE_HEAD = "50e987863da13c0982df225027314ce3de40b20a"
PREVIOUS_TIP = "fb8f0d5fae8c8e800e627cc5127500ae33ba692d"
PREVIOUS_BLOB = "dbdb85382fa5a8a529313ca6c4d49e6eb5fec0f5"
PROMOTED_BLOB = "a3423fbb7d1e7aeaaff6be5ecc26d97cc6f8e181"
AUTHORIZATION_SHA = "ede30afe7a74872426fa2c9ba206d587da900a4e"
AUTHORIZATION_MERGE = "c94541bef53a5f30425f8e65da0b161aad421374"
LIVE_CONTROL = "d" * 40


def production_promotion() -> dict:
    root = pathlib.Path(__file__).resolve().parents[1]
    return json.loads(
        (
            root
            / ".github/rp-workflow-promotions"
            / f"{RELEASE_MERGE}.json"
        ).read_text(encoding="utf-8")
    )


def production_manifest() -> dict:
    return {
        "schema_version": 1,
        "repository": REPOSITORY,
        "workflow_path": authorization.ORDINARY_WORKFLOW,
        "previous_workflow_blob_sha": PREVIOUS_BLOB,
        "promoted_workflow_blob_sha": PROMOTED_BLOB,
        "release_transition": {
            "previous_tip": PREVIOUS_TIP,
            "merge_sha": RELEASE_MERGE,
            "head_sha": RELEASE_HEAD,
        },
        "reviewed_control": {
            "ref": authorization.PROFILE_CONTROL_REF,
            "workflow_path": authorization.ORDINARY_WORKFLOW,
            "previous_workflow_blob_sha": PREVIOUS_BLOB,
            "promoted_workflow_blob_sha": PROMOTED_BLOB,
            "control_workflow_blob_sha": (
                "4c3da74f1e2121a100682a7b391849135e06d1b2"
            ),
            "reviewed_tip": "03df71a45065de0eddb14c71d2de0ae9554a2450",
            "pull_requests": [
                {
                    "number": 22,
                    "head_sha": "16c0fc0b656fe298090791dc091e0cc9c8698622",
                    "merge_sha": "8d082209d4aba8c44fd6b740f58391078ba45b94",
                },
                {
                    "number": 24,
                    "head_sha": "88e4f7baa7e5b8f40b5f85e20ff6617a9dd7e86d",
                    "merge_sha": "03df71a45065de0eddb14c71d2de0ae9554a2450",
                },
            ],
        },
    }


def control_pull(number: int, head_sha: str, merge_sha: str) -> dict:
    return {
        "number": number,
        "merged_at": "2026-10-01T00:00:00Z",
        "merge_commit_sha": merge_sha,
        "base": {
            "ref": "automation/rp-control",
            "repo": {"full_name": REPOSITORY},
        },
        "head": {
            "sha": head_sha,
            "repo": {"full_name": REPOSITORY},
        },
    }


class ProductionTransitionTests(unittest.TestCase):
    def test_exact_production_transition_is_promoted(self):
        root = pathlib.Path(__file__).resolve().parents[1]
        promotion = production_promotion()
        self.assertEqual(
            release_workflow_transitions(root, RELEASE_MERGE),
            [promotion["release_transition"]],
        )
        validate_transition_topology(root, production_manifest())

    def test_record_binds_exact_control_artifacts(self):
        promotion = validate_workflow_promotion_record(production_promotion())
        self.assertEqual(
            promotion["control_authorization"],
            {
                "ref": "refs/heads/automation/rp-control",
                "commit_sha": AUTHORIZATION_SHA,
                "pull_request": {
                    "number": 25,
                    "head_sha": AUTHORIZATION_SHA,
                    "merge_sha": AUTHORIZATION_MERGE,
                },
            },
        )
        self.assertEqual(
            promotion["transition_manifest"]["blob_sha"],
            "4dc071b99c88cba3797261db58c910ffab76a107",
        )
        self.assertEqual(
            promotion["transition_validator"]["blob_sha"],
            "770583eca400e6b40818b9ef51b6ff310fa4b224",
        )

    def test_reviewed_topology_rejects_mismatched_resolved_ref(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            subprocess.run(["git", "init", "-q", "-b", "base", root], check=True)
            subprocess.run(
                ["git", "-C", root, "config", "user.email", "rp@example.invalid"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", root, "config", "user.name", "RP Test"],
                check=True,
            )
            workflow = root / authorization.ORDINARY_WORKFLOW
            workflow.parent.mkdir(parents=True)
            workflow.write_text("name: previous\n", encoding="utf-8")
            subprocess.run(["git", "-C", root, "add", "."], check=True)
            subprocess.run(["git", "-C", root, "commit", "-qm", "base"], check=True)
            base = subprocess.check_output(
                ["git", "-C", root, "rev-parse", "HEAD"], text=True
            ).strip()
            previous_blob = subprocess.check_output(
                [
                    "git",
                    "-C",
                    root,
                    "rev-parse",
                    f"HEAD:{authorization.ORDINARY_WORKFLOW}",
                ],
                text=True,
            ).strip()
            pulls = []
            previous_merge = base
            for number in (22, 24):
                subprocess.run(
                    ["git", "-C", root, "switch", "-qc", f"pr-{number}"],
                    check=True,
                )
                if number == 22:
                    workflow.write_text("name: promoted\n", encoding="utf-8")
                else:
                    (root / "retained.txt").write_text("retained\n", encoding="utf-8")
                subprocess.run(["git", "-C", root, "add", "."], check=True)
                subprocess.run(
                    ["git", "-C", root, "commit", "-qm", f"head {number}"],
                    check=True,
                )
                head = subprocess.check_output(
                    ["git", "-C", root, "rev-parse", "HEAD"], text=True
                ).strip()
                subprocess.run(
                    ["git", "-C", root, "switch", "-q", "base"], check=True
                )
                subprocess.run(
                    [
                        "git",
                        "-C",
                        root,
                        "merge",
                        "--no-ff",
                        "-qm",
                        f"merge {number}",
                        f"pr-{number}",
                    ],
                    check=True,
                )
                merge = subprocess.check_output(
                    ["git", "-C", root, "rev-parse", "HEAD"], text=True
                ).strip()
                pulls.append(
                    {"number": number, "head_sha": head, "merge_sha": merge}
                )
                previous_merge = merge
            promoted_blob = subprocess.check_output(
                [
                    "git",
                    "-C",
                    root,
                    "rev-parse",
                    f"HEAD:{authorization.ORDINARY_WORKFLOW}",
                ],
                text=True,
            ).strip()
            manifest = production_manifest()
            manifest["previous_workflow_blob_sha"] = previous_blob
            manifest["reviewed_control"] |= {
                "previous_workflow_blob_sha": previous_blob,
                "promoted_workflow_blob_sha": manifest[
                    "promoted_workflow_blob_sha"
                ],
                "control_workflow_blob_sha": promoted_blob,
                "reviewed_tip": previous_merge,
                "pull_requests": pulls,
            }
            subprocess.run(
                ["git", "-C", root, "branch", "live-control", previous_merge],
                check=True,
            )
            subprocess.run(
                ["git", "-C", root, "branch", "wrong-control", base],
                check=True,
            )
            validate_reviewed_control_topology(root, manifest, "live-control")
            with self.assertRaisesRegex(
                WorkflowTransitionError, "not contained in the live control ref"
            ):
                validate_reviewed_control_topology(root, manifest, "wrong-control")


class PromotionHistoryTests(unittest.TestCase):
    def write_promotions(self, root: pathlib.Path, records: list[dict]) -> None:
        directory = root / authorization.WORKFLOW_PROMOTION_DIRECTORY
        directory.mkdir(parents=True)
        for record in records:
            merge_sha = record["release_transition"]["merge_sha"]
            (directory / f"{merge_sha}.json").write_text(
                json.dumps(record), encoding="utf-8"
            )

    def assert_history_rejected(
        self, promotion: dict, transition: dict, message: str
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            self.write_promotions(root, [promotion])
            with mock.patch.object(
                authorization,
                "release_workflow_transitions",
                return_value=[transition],
            ), self.assertRaisesRegex(AuthorizationError, message):
                validate_workflow_promotions(root, RELEASE_MERGE)

    def test_rejects_wrong_old_or_new_blob_and_stale_target(self):
        expected = production_promotion()["release_transition"]
        for key in (
            "previous_workflow_blob_sha",
            "promoted_workflow_blob_sha",
            "previous_tip",
        ):
            promotion = production_promotion()
            promotion["release_transition"][key] = "e" * 40
            if key == "promoted_workflow_blob_sha":
                promotion["transition_manifest"]["path"] = (
                    f".github/rp-workflow-transitions/{'e' * 40}.json"
                )
            with self.subTest(key=key):
                self.assert_history_rejected(
                    promotion,
                    expected,
                    "does not have exactly one promotion",
                )

    def test_rejects_duplicate_or_replayed_promotion(self):
        first = production_promotion()
        second = copy.deepcopy(first)
        second["release_transition"]["merge_sha"] = "e" * 40
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            self.write_promotions(root, [first, second])
            with mock.patch.object(
                authorization,
                "release_workflow_transitions",
                return_value=[first["release_transition"]],
            ), self.assertRaisesRegex(AuthorizationError, "duplicated or replayed"):
                validate_workflow_promotions(root, RELEASE_MERGE)

    def test_rejects_unlisted_historical_transition(self):
        promotion = production_promotion()
        unlisted = copy.deepcopy(promotion["release_transition"])
        unlisted["previous_tip"] = RELEASE_MERGE
        unlisted["merge_sha"] = "e" * 40
        unlisted["head_sha"] = "f" * 40
        unlisted["previous_workflow_blob_sha"] = PROMOTED_BLOB
        unlisted["promoted_workflow_blob_sha"] = "1" * 40
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            self.write_promotions(root, [promotion])
            with mock.patch.object(
                authorization,
                "release_workflow_transitions",
                return_value=[promotion["release_transition"], unlisted],
            ), self.assertRaisesRegex(
                AuthorizationError, "does not have exactly one promotion"
            ):
                validate_workflow_promotions(root, RELEASE_MERGE)


class ControlEvidenceTests(unittest.TestCase):
    def test_rejects_unmerged_wrong_repo_wrong_base_or_wrong_head_pr(self):
        trusted = control_pull(25, AUTHORIZATION_SHA, AUTHORIZATION_MERGE)
        for mutation in (
            {"merged_at": None},
            {"base": {"ref": "main", "repo": {"full_name": REPOSITORY}}},
            {
                "base": {
                    "ref": "automation/rp-control",
                    "repo": {"full_name": "someone/zed"},
                }
            },
            {
                "head": {
                    "sha": AUTHORIZATION_SHA,
                    "repo": {"full_name": "someone/zed"},
                }
            },
            {
                "head": {
                    "sha": "e" * 40,
                    "repo": {"full_name": REPOSITORY},
                }
            },
        ):
            with self.subTest(mutation=mutation), self.assertRaisesRegex(
                AuthorizationError, "identity is not trusted"
            ):
                validate_control_pull_request(
                    trusted | mutation,
                    number=25,
                    head_sha=AUTHORIZATION_SHA,
                    merge_sha=None,
                )

    def run_full_validation(
        self,
        *,
        unreachable: bool = False,
        manifest_blob: str | None = None,
        validator_blob: str | None = None,
        local_validator_blob: str | None = None,
    ) -> None:
        promotion = production_promotion()
        manifest = production_manifest()
        pulls = {
            25: control_pull(25, AUTHORIZATION_SHA, AUTHORIZATION_MERGE),
            22: control_pull(
                22,
                manifest["reviewed_control"]["pull_requests"][0]["head_sha"],
                manifest["reviewed_control"]["pull_requests"][0]["merge_sha"],
            ),
            24: control_pull(
                24,
                manifest["reviewed_control"]["pull_requests"][1]["head_sha"],
                manifest["reviewed_control"]["pull_requests"][1]["merge_sha"],
            ),
        }

        def fake_git(_root: pathlib.Path, *arguments: str) -> str:
            if arguments[:3] == ("merge-base", "--is-ancestor", AUTHORIZATION_SHA):
                if unreachable:
                    raise AuthorizationError("not an ancestor")
                return ""
            if arguments[0] == "merge-base":
                return ""
            if arguments[0] == "fetch":
                return ""
            if arguments[:2] == ("rev-parse", "--verify"):
                return LIVE_CONTROL
            if arguments[0] == "rev-parse":
                target = arguments[1]
                if target == (
                    f"{RELEASE_MERGE}:"
                    f"{promotion['transition_validator']['path']}"
                ):
                    return (
                        local_validator_blob
                        or promotion["transition_validator"]["blob_sha"]
                    )
                if target.endswith(promotion["transition_manifest"]["path"]):
                    return manifest_blob or promotion["transition_manifest"]["blob_sha"]
                if target.endswith(promotion["transition_validator"]["path"]):
                    return validator_blob or promotion["transition_validator"]["blob_sha"]
            if arguments[0] == "hash-object":
                return (
                    local_validator_blob
                    or promotion["transition_validator"]["blob_sha"]
                )
            if arguments[0] == "show":
                return json.dumps(manifest)
            raise AssertionError(arguments)

        def fake_gh(endpoint: str) -> dict:
            return pulls[int(endpoint.rsplit("/", 1)[1])]

        with mock.patch.object(
            authorization, "load_workflow_promotions", return_value=[promotion]
        ), mock.patch.object(
            authorization,
            "release_workflow_transitions",
            return_value=[promotion["release_transition"]],
        ), mock.patch.object(
            authorization, "git", side_effect=fake_git
        ), mock.patch.object(
            authorization, "gh_json", side_effect=fake_gh
        ), mock.patch.object(
            authorization, "validate_transition_topology"
        ), mock.patch.object(
            authorization, "validate_reviewed_control_topology"
        ) as reviewed_topology:
            validate_workflow_promotions(pathlib.Path("."), RELEASE_MERGE)
            reviewed_topology.assert_called_once_with(
                pathlib.Path("."),
                manifest,
                "refs/remotes/origin/rp-reviewed-control-workflow-promotions",
            )

    def test_accepts_exact_control_evidence(self):
        self.run_full_validation()

    def test_rejects_authorization_not_reachable_from_live_control(self):
        with self.assertRaisesRegex(AuthorizationError, "not reachable"):
            self.run_full_validation(unreachable=True)

    def test_rejects_manifest_or_validator_blob_mismatch(self):
        for argument in ("manifest_blob", "validator_blob"):
            with self.subTest(argument=argument), self.assertRaisesRegex(
                AuthorizationError, "blob changed"
            ):
                self.run_full_validation(**{argument: "e" * 40})

    def test_rejects_modified_current_local_validator(self):
        with self.assertRaisesRegex(
            AuthorizationError, "local workflow transition validator blob changed"
        ):
            self.run_full_validation(local_validator_blob="e" * 40)


if __name__ == "__main__":
    unittest.main()
