import json
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

from rp_release_metadata import (
    CALENDAR_POLICY_PATH,
    MetadataError,
    allocate_calendar_version,
    finalize_update_manifest,
    generate_metadata,
    read_calendar_floor,
    release_asset_names,
    verify_published_assets,
)


class AllocateCalendarVersionTests(unittest.TestCase):
    def test_first_calendar_release_starts_at_one(self):
        allocation = allocate_calendar_version(
            "current", "20260902", [("rp-stable-deadbee", "legacy")]
        )
        self.assertEqual(allocation.version, "20260902.1")
        self.assertFalse(allocation.reused)

    def test_same_day_increments_highest_patch(self):
        allocation = allocate_calendar_version(
            "current",
            "20260902",
            [
                ("rp-stable-20260902.1", "one"),
                ("rp-stable-20260902.3", "three"),
                ("rp-stable-20260901.9", "yesterday"),
            ],
        )
        self.assertEqual(allocation.version, "20260902.4")

    def test_cross_day_resets_patch(self):
        allocation = allocate_calendar_version(
            "current", "20260903", [("rp-stable-20260902.8", "yesterday")]
        )
        self.assertEqual(allocation.version, "20260903.1")

    def test_rerun_reuses_current_sha_identity_even_on_later_date(self):
        allocation = allocate_calendar_version(
            "current",
            "20260910",
            [
                ("rp-stable-20260902.2", "current"),
                ("rp-stable-20260910.1", "other"),
            ],
        )
        self.assertEqual(allocation.version, "20260902.2")
        self.assertTrue(allocation.reused)

    def test_legacy_sha_tags_are_ignored(self):
        allocation = allocate_calendar_version(
            "current",
            "20260902",
            [
                ("rp-stable-current", "current"),
                ("rp-stable-abcdef0", "other"),
            ],
        )
        self.assertEqual(allocation.version, "20260902.1")

    def test_conflicting_versions_on_current_sha_fail(self):
        with self.assertRaisesRegex(MetadataError, "inconsistent RP calendar identities"):
            allocate_calendar_version(
                "current",
                "20260902",
                [
                    ("rp-stable-20260901.1", "current"),
                    ("rp-stable-20260902.1", "current"),
                ],
            )

    def test_version_collision_across_commits_fails(self):
        with self.assertRaisesRegex(MetadataError, "points to multiple commits"):
            allocate_calendar_version(
                "current",
                "20260902",
                [
                    ("rp-stable-20260902.1", "one"),
                    ("rp-stable-20260902.1", "other"),
                ],
            )

    def test_unpublished_installed_calendar_is_excluded(self):
        allocation = allocate_calendar_version(
            "corrected",
            "20261028",
            [("rp-stable-20260905.1", "published")],
            minimum_exclusive="20261028.1",
        )
        self.assertEqual(allocation.version, "20261028.2")
        self.assertFalse(allocation.reused)

    def test_later_installed_patch_is_compared_numerically(self):
        allocation = allocate_calendar_version(
            "corrected",
            "20261028",
            [("rp-stable-20261028.9", "published")],
            minimum_exclusive="20261028.10",
        )
        self.assertEqual(allocation.version, "20261028.11")

    def test_published_patch_above_installed_floor_wins(self):
        allocation = allocate_calendar_version(
            "corrected",
            "20261028",
            [("rp-stable-20261028.12", "published")],
            minimum_exclusive="20261028.1",
        )
        self.assertEqual(allocation.version, "20261028.13")

    def test_next_utc_day_is_newer_than_installed_calendar(self):
        allocation = allocate_calendar_version(
            "corrected", "20261029", [], minimum_exclusive="20261028.12"
        )
        self.assertEqual(allocation.version, "20261029.1")

    def test_existing_source_tag_at_or_below_floor_cannot_be_rewritten(self):
        for version in ("20261027.9", "20261028.1"):
            with self.subTest(version=version):
                with self.assertRaisesRegex(MetadataError, "use a new reviewed source"):
                    allocate_calendar_version(
                        "current",
                        "20261029",
                        [(f"rp-stable-{version}", "current")],
                        minimum_exclusive="20261028.1",
                    )

    def test_existing_source_tag_above_floor_is_reused_immutably(self):
        allocation = allocate_calendar_version(
            "current",
            "20261029",
            [("rp-stable-20261028.2", "current")],
            minimum_exclusive="20261028.1",
        )
        self.assertEqual(allocation.version, "20261028.2")
        self.assertTrue(allocation.reused)

    def test_clock_regression_does_not_allocate_an_older_release(self):
        for tags, floor in (
            ([], "20261029.1"),
            ([("rp-stable-20261029.1", "published")], "20261028.1"),
        ):
            with self.subTest(tags=tags, floor=floor):
                with self.assertRaisesRegex(MetadataError, "UTC allocation date"):
                    allocate_calendar_version(
                        "current", "20261028", tags, minimum_exclusive=floor
                    )

    def test_invalid_floors_fail_closed(self):
        for floor in (
            "",
            "20261028",
            "20261028.0",
            "20261028.01",
            "20260229.1",
            "20261301.1",
            "20261028.4294967296",
        ):
            with self.subTest(floor=floor):
                with self.assertRaises(MetadataError):
                    allocate_calendar_version(
                        "current", "20261028", [], minimum_exclusive=floor
                    )

    def test_client_patch_limit_does_not_overflow(self):
        with self.assertRaisesRegex(MetadataError, "client limit"):
            allocate_calendar_version(
                "current", "20261028", [], minimum_exclusive="20261028.4294967295"
            )

    def test_invalid_utc_date_is_rejected(self):
        with self.assertRaisesRegex(MetadataError, "invalid RP calendar date"):
            allocate_calendar_version("current", "20260229", [])


class CalendarPolicyTests(unittest.TestCase):
    def commit_policy(self, repo, policy):
        subprocess.run(["git", "init", "--quiet", str(repo)], check=True)
        path = repo / CALENDAR_POLICY_PATH
        if policy is None:
            (repo / "product.txt").write_text("RP product source\n", encoding="utf-8")
        else:
            path.parent.mkdir(exist_ok=True)
            path.write_text(json.dumps(policy), encoding="utf-8")
        subprocess.run(["git", "add", "."], cwd=repo, check=True)
        subprocess.run(
            [
                "git",
                "-c",
                "user.name=RP test",
                "-c",
                "user.email=rp-test@example.com",
                "commit",
                "--quiet",
                "-m",
                "Record installed calendar floor",
            ],
            cwd=repo,
            check=True,
        )
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=repo, text=True
        ).strip()

    def test_missing_policy_does_not_fall_back_to_public_tags(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            commit = self.commit_policy(repo, None)
            with self.assertRaisesRegex(MetadataError, "git show"):
                read_calendar_floor(repo, commit)

    def test_floor_comes_from_exact_product_commit_not_working_tree(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            commit = self.commit_policy(
                repo, {"schema_version": 1, "minimum_exclusive": "20261028.1"}
            )
            later = self.commit_policy(
                repo, {"schema_version": 1, "minimum_exclusive": "20261028.5"}
            )
            (repo / CALENDAR_POLICY_PATH).write_text(
                '{"schema_version":1,"minimum_exclusive":"20261028.99"}',
                encoding="utf-8",
            )
            self.assertEqual(read_calendar_floor(repo, commit), "20261028.1")
            self.assertEqual(read_calendar_floor(repo, later), "20261028.5")

    def test_invalid_policy_is_rejected(self):
        for policy in (
            {},
            {"schema_version": 2, "minimum_exclusive": "20261028.1"},
            {"schema_version": True, "minimum_exclusive": "20261028.1"},
            {"schema_version": 1, "minimum_exclusive": None},
            {"schema_version": 1, "minimum_exclusive": "20260229.1"},
            {
                "schema_version": 1,
                "minimum_exclusive": "20261028.1",
                "unexpected": True,
            },
        ):
            with self.subTest(policy=policy):
                with tempfile.TemporaryDirectory() as temporary_directory:
                    repo = pathlib.Path(temporary_directory)
                    commit = self.commit_policy(repo, policy)
                    with self.assertRaises(MetadataError):
                        read_calendar_floor(repo, commit)

    def test_malformed_committed_json_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            self.commit_policy(
                repo, {"schema_version": 1, "minimum_exclusive": "20261028.1"}
            )
            (repo / CALENDAR_POLICY_PATH).write_text("{", encoding="utf-8")
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=RP test",
                    "-c",
                    "user.email=rp-test@example.com",
                    "commit",
                    "--quiet",
                    "-am",
                    "Truncate calendar policy",
                ],
                cwd=repo,
                check=True,
            )
            with self.assertRaises(json.JSONDecodeError):
                read_calendar_floor(repo, "HEAD")

    def test_generation_uses_the_committed_floor_and_consistent_asset_names(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = pathlib.Path(temporary_directory)
            policy_path = repo / CALENDAR_POLICY_PATH
            policy_path.parent.mkdir()
            policy_path.write_text(
                json.dumps({"schema_version": 1, "minimum_exclusive": "20261028.10"}),
                encoding="utf-8",
            )
            fragments_path = repo / "script" / "rp_release_notes" / "manifest.json"
            fragments_path.parent.mkdir(parents=True)
            fragments_path.write_text('{"fragments": []}', encoding="utf-8")
            output = repo / "metadata"
            github_output = repo / "github-output"
            base = {
                "upstream_tag": "v1.22.0",
                "upstream_tag_commit": "a" * 40,
                "upstream_version": "1.22.0",
            }
            with (
                mock.patch("rp_release_metadata.verify_base", return_value=base),
                mock.patch(
                    "rp_release_metadata.run_git",
                    side_effect=lambda repo, *args: (
                        policy_path.read_text(encoding="utf-8")
                        if args == ("show", f"{'b' * 40}:{CALENDAR_POLICY_PATH.as_posix()}")
                        else "b" * 40
                    ),
                ),
                mock.patch("rp_release_metadata.read_tags", return_value=[]),
            ):
                generate_metadata(repo, output, "20261028", github_output)
            manifest = json.loads(
                (output / "rp-release-manifest.json").read_text(encoding="utf-8")
            )
            self.assertEqual(manifest["calendar_version"], "20261028.11")
            self.assertEqual(manifest["tag"], "rp-stable-20261028.11")
            self.assertEqual(manifest["asset_names"], release_asset_names("20261028.11"))
            self.assertIn("RP 20261028.11", (output / "release-notes.md").read_text())
            self.assertEqual(
                github_output.read_text(),
                "version=20261028.11\ntag=rp-stable-20261028.11\nreused=false\n",
            )


class FinalizeUpdateManifestTests(unittest.TestCase):
    def test_records_strict_asset_metadata(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            dist = root / "dist"
            dist.mkdir()
            asset_names = release_asset_names("20260902.1")
            for index, name in enumerate(asset_names.values(), start=1):
                (dist / name).write_bytes(bytes([index]) * index)

            metadata = {
                "schema_version": 2,
                "channel": "rp-stable",
                "calendar_version": "20260902.1",
                "upstream_version": "1.19.0",
                "upstream_tag": "v1.19.0",
                "upstream_tag_commit": "c" * 40,
                "commit": "a" * 40,
                "tag": "rp-stable-20260902.1",
                "trust": {"signed": False, "label": "unsigned"},
                "notes_identity": f"sha256:{'b' * 64}",
                "asset_names": asset_names,
            }
            metadata_path = root / "metadata.json"
            metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
            output_path = root / "rp-update.json"

            finalize_update_manifest(
                metadata_path, dist, "JonathonRP/zed", output_path
            )

            update_manifest = json.loads(output_path.read_text(encoding="utf-8"))
            self.assertEqual(update_manifest["schema_version"], 2)
            self.assertEqual(update_manifest["channel"], "rp-stable")
            self.assertEqual(update_manifest["upstream_tag"], "v1.19.0")
            self.assertEqual(update_manifest["upstream_tag_commit"], "c" * 40)
            self.assertEqual(
                update_manifest["assets"]["windows_x86_64_installer"],
                {
                    "name": asset_names["windows_x86_64_installer"],
                    "size": 1,
                    "sha256": (
                        "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7c"
                        "ce23c7785459a"
                    ),
                    "url": (
                        "https://github.com/JonathonRP/zed/releases/download/"
                        "rp-stable-20260902.1/"
                        f"{asset_names['windows_x86_64_installer']}"
                    ),
                },
            )


class VerifyPublishedAssetsTests(unittest.TestCase):
    def test_identical_published_release_is_a_no_op(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            expected = root / "expected"
            published = root / "published"
            expected.mkdir()
            published.mkdir()
            for directory in (expected, published):
                (directory / "asset.zip").write_bytes(b"same bytes")
                (directory / "rp-update.json").write_bytes(b"same manifest")

            verify_published_assets(expected, published)

    def test_changed_published_release_fails(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            expected = root / "expected"
            published = root / "published"
            expected.mkdir()
            published.mkdir()
            (expected / "asset.zip").write_bytes(b"new bytes")
            (published / "asset.zip").write_bytes(b"published bytes")

            with self.assertRaisesRegex(
                MetadataError, "published RP release is immutable"
            ):
                verify_published_assets(expected, published)

    def test_changed_asset_set_fails(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = pathlib.Path(temporary_directory)
            expected = root / "expected"
            published = root / "published"
            expected.mkdir()
            published.mkdir()
            (expected / "asset.zip").write_bytes(b"bytes")
            (published / "other.zip").write_bytes(b"bytes")

            with self.assertRaisesRegex(MetadataError, "missing published assets"):
                verify_published_assets(expected, published)


if __name__ == "__main__":
    unittest.main()
