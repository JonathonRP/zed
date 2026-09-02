import json
import pathlib
import tempfile
import unittest

from rp_release_metadata import (
    MetadataError,
    allocate_calendar_version,
    finalize_update_manifest,
    release_asset_names,
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
                "schema_version": 1,
                "channel": "rp-stable",
                "calendar_version": "20260902.1",
                "upstream_version": "1.19.0",
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
            self.assertEqual(update_manifest["schema_version"], 1)
            self.assertEqual(update_manifest["channel"], "rp-stable")
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


if __name__ == "__main__":
    unittest.main()
