import json
import pathlib
import subprocess
import tempfile
import unittest

from rp_stable_base import StableBaseError, candidate_base, read_base, verify_base


PINNED_SHA = "a" * 40


class StableBaseTests(unittest.TestCase):
    def test_rejects_extra_or_mismatched_identity_fields(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = pathlib.Path(temporary_directory) / "base.json"
            valid = {
                "schema_version": 1,
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
                        "schema_version": 1,
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


if __name__ == "__main__":
    unittest.main()
