import json
import pathlib
import subprocess
import tempfile
import unittest

from rp_stable_base import (
    StableBaseError,
    candidate_base,
    read_base,
    verify_base,
    verify_transition,
)


PINNED_SHA = "a" * 40
AUTOMATION = {
    "fork_main_sync_cron_utc": "0 0 * * *",
    "stable_sync_cron_utc": "0 0 * * *",
}
SOURCE_CORRECTION = {
    "previous_rp_tip": "b" * 40,
    "previous_upstream_commit": "c" * 40,
    "previous_upstream_version": "1.20.0",
}


class StableBaseTests(unittest.TestCase):
    def test_rejects_extra_or_mismatched_identity_fields(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = pathlib.Path(temporary_directory) / "base.json"
            valid = {
                "schema_version": 2,
                "automation": AUTOMATION,
                "initial_source_correction": SOURCE_CORRECTION,
                "upstream_repository": "zed-industries/zed",
                "upstream_tag": "v1.18.0",
                "upstream_tag_commit": PINNED_SHA,
                "upstream_version": "1.18.0",
            }
            for mutation in (
                {"unexpected": True},
                {"upstream_repository": "JonathonRP/zed"},
                {"upstream_tag": "v1.20.0"},
                {"upstream_tag_commit": "short"},
                {"upstream_version": "1.18.0-pre.1"},
                {
                    "automation": {
                        "fork_main_sync_cron_utc": "0 0 * * 1",
                        "stable_sync_cron_utc": "0 0 * * *",
                    }
                },
            ):
                candidate = valid | mutation
                path.write_text(json.dumps(candidate), encoding="utf-8")
                with self.assertRaises(StableBaseError):
                    read_base(path)

    def test_candidate_rejects_prerelease_and_version_mismatch(self):
        current = {
            "upstream_version": "1.18.0",
        }
        with self.assertRaisesRegex(StableBaseError, "draft or prerelease"):
            candidate_base(
                pathlib.Path.cwd(),
                current,
                {"draft": False, "prerelease": True, "tag_name": "v1.19.0"},
            )

    def test_equal_or_older_candidate_is_not_an_update(self):
        current = {"upstream_version": "1.18.0"}
        for tag in ("v1.18.0", "v1.17.9"):
            self.assertIsNone(
                candidate_base(
                    pathlib.Path.cwd(),
                    current,
                    {"draft": False, "prerelease": False, "tag_name": tag},
                )
            )

    def test_workflows_share_documented_midnight_utc_schedule(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        base = read_base(repo / ".github/rp-stable-base.json")
        for workflow, metadata_key in (
            ("rp_stable_sync.yml", "stable_sync_cron_utc"),
            ("rp_upstream_main_sync.yml", "fork_main_sync_cron_utc"),
        ):
            contents = (repo / ".github/workflows" / workflow).read_text(
                encoding="utf-8"
            )
            schedule = base["automation"][metadata_key]
            self.assertIn(f'cron: "{schedule}"', contents)

    def test_main_sync_is_fast_forward_only_and_never_runs_upstream_code(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        contents = (
            repo / ".github/workflows/rp_upstream_main_sync.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("github.repository == 'JonathonRP/zed'", contents)
        self.assertIn('git merge-base --is-ancestor "$old_sha" "$new_sha"', contents)
        self.assertIn('git push fork "${NEW_SHA}:refs/heads/main"', contents)
        self.assertNotIn("--force", contents)
        self.assertNotIn("actions/checkout", contents)


class RepositoryVerificationTests(unittest.TestCase):
    def test_repository_pins_tag_commit_cargo_version_and_app_version(self):
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
            (repo / "crates/zed").mkdir(parents=True)
            (repo / "crates/zed/Cargo.toml").write_text(
                '[package]\nname = "zed"\nversion = "1.18.0"\n',
                encoding="utf-8",
            )
            subprocess.run(["git", "-C", repo, "add", "."], check=True)
            subprocess.run(["git", "-C", repo, "commit", "-qm", "base"], check=True)
            sha = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            subprocess.run(["git", "-C", repo, "tag", "v1.18.0"], check=True)
            base_path = repo / ".github/rp-stable-base.json"
            base_path.parent.mkdir()
            base_path.write_text(
                json.dumps(
                    {
                        "schema_version": 2,
                        "automation": AUTOMATION,
                        "initial_source_correction": SOURCE_CORRECTION,
                        "upstream_repository": "zed-industries/zed",
                        "upstream_tag": "v1.18.0",
                        "upstream_tag_commit": sha,
                        "upstream_version": "1.18.0",
                    }
                ),
                encoding="utf-8",
            )
            verify_base(repo, base_path, "1.18.0+stable.test")
            with self.assertRaisesRegex(StableBaseError, "built AppVersion base"):
                verify_base(repo, base_path, "1.20.0+stable.test")
            subprocess.run(["git", "-C", repo, "tag", "v1.19.0"], check=True)
            with self.assertRaisesRegex(StableBaseError, "does not contain Zed version"):
                candidate_base(
                    repo,
                    {"upstream_version": "1.18.0"},
                    {
                        "draft": False,
                        "prerelease": False,
                        "tag_name": "v1.19.0",
                    },
                )
            subprocess.run(["git", "-C", repo, "tag", "-d", "v1.18.0"], check=True)
            with self.assertRaisesRegex(StableBaseError, "git rev-parse"):
                verify_base(repo, base_path)

    def test_transition_rejects_regression_after_metadata_exists(self):
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
            (repo / ".github").mkdir()
            previous = {
                "schema_version": 2,
                "automation": AUTOMATION,
                "initial_source_correction": SOURCE_CORRECTION,
                "upstream_repository": "zed-industries/zed",
                "upstream_tag": "v1.19.0",
                "upstream_tag_commit": PINNED_SHA,
                "upstream_version": "1.19.0",
            }
            (repo / ".github/rp-stable-base.json").write_text(
                json.dumps(previous), encoding="utf-8"
            )
            subprocess.run(["git", "-C", repo, "add", "."], check=True)
            subprocess.run(["git", "-C", repo, "commit", "-qm", "previous"], check=True)
            previous_ref = subprocess.check_output(
                ["git", "-C", repo, "rev-parse", "HEAD"], text=True
            ).strip()
            current = previous | {
                "upstream_tag": "v1.18.0",
                "upstream_version": "1.18.0",
            }
            with self.assertRaisesRegex(StableBaseError, "regresses"):
                verify_transition(repo, current, previous_ref)


if __name__ == "__main__":
    unittest.main()
