import pathlib
import re
import unittest


WORKFLOW_NAME = "rp_profile_compatibility_build.yml"
SOURCE_REF = "jonathonrp-track-zed-stable"
FULL_SHA = re.compile(r"^[0-9a-f]{40}$")


class ProfileCompatibilityWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.repo = pathlib.Path(__file__).resolve().parents[1]
        cls.contents = (
            cls.repo / ".github" / "workflows" / WORKFLOW_NAME
        ).read_text(encoding="utf-8")

    def test_trigger_is_manual_only_with_fixed_source_and_required_sha(self):
        trigger = self.contents.split("\npermissions:", 1)[0]
        self.assertIn("  workflow_dispatch:", trigger)
        self.assertNotRegex(
            trigger,
            r"(?m)^  (?:push|pull_request|pull_request_target|schedule|workflow_run):",
        )
        self.assertRegex(
            trigger,
            rf"(?ms)^      source_ref:\n.*?^        required: true\n"
            rf".*?^        type: choice\n.*?^        options:\n"
            rf"          - {re.escape(SOURCE_REF)}$",
        )
        self.assertRegex(
            trigger,
            r"(?ms)^      expected_sha:\n.*?^        required: true\n"
            r".*?^        type: string$",
        )

    def test_repository_source_and_commit_are_strictly_guarded(self):
        self.assertIn('test "$GITHUB_REPOSITORY" = "JonathonRP/zed"', self.contents)
        self.assertIn(f'test "$SOURCE_REF" = "{SOURCE_REF}"', self.contents)
        self.assertIn(
            '[[ "$EXPECTED_SHA" =~ ^[0-9a-fA-F]{40}$ ]]', self.contents
        )
        self.assertIn(f"          ref: {SOURCE_REF}", self.contents)
        self.assertIn(
            '"refs/heads/${SOURCE_REF}:refs/remotes/compatibility-source/${SOURCE_REF}"',
            self.contents,
        )
        self.assertIn(
            'git merge-base --is-ancestor "$expected_sha" "$source_branch"',
            self.contents,
        )
        self.assertIn('test "$source_sha" = "$expected_sha"', self.contents)

    def test_permissions_and_checkout_credentials_are_read_only(self):
        self.assertRegex(
            self.contents,
            r"(?m)^permissions:\n  contents: read\n\nconcurrency:",
        )
        self.assertNotRegex(
            self.contents,
            r"(?m)^\s+(?:contents|actions|deployments|packages|pull-requests): write$",
        )
        checkout_count = self.contents.count("uses: actions/checkout@")
        self.assertEqual(checkout_count, 3)
        self.assertEqual(
            self.contents.count("          persist-credentials: false"),
            checkout_count,
        )

    def test_builds_share_exact_source_sha_and_generated_manifest(self):
        self.assertIn(
            "group: rp-profile-compatibility-${{ inputs.expected_sha }}",
            self.contents,
        )
        self.assertIn("cancel-in-progress: false", self.contents)
        self.assertEqual(
            self.contents.count(
                "          ref: ${{ needs.metadata.outputs.source_sha }}"
            ),
            2,
        )
        self.assertEqual(
            self.contents.count(
                "name: rp-profile-compatibility-metadata-"
                "${{ needs.metadata.outputs.source_sha }}"
            ),
            2,
        )
        self.assertIn("$metadata.commit -ne $env:EXPECTED_SHA", self.contents)
        self.assertIn('test "$manifest_sha" = "$EXPECTED_SHA"', self.contents)
        self.assertIn("target/windows-source-identity.json", self.contents)
        self.assertIn("target/linux-source-identity.json", self.contents)

    def test_workflow_only_uploads_requested_build_artifacts(self):
        self.assertIn("script/bundle-windows.ps1 -Architecture x86_64", self.contents)
        self.assertIn(
            "--target x86_64-unknown-linux-musl",
            self.contents,
        )
        self.assertIn("windows-x86_64-portable.zip", self.contents)
        self.assertIn("remote-server-linux-x86_64.gz", self.contents)
        self.assertNotIn(
            "remote-server-windows-x86_64.zip",
            self.contents,
        )
        self.assertEqual(self.contents.count("          retention-days: 7"), 3)
        self.assertNotIn("\n  assemble:", self.contents)
        self.assertNotIn("\n  publish:", self.contents)
        self.assertNotRegex(self.contents, r"(?m)^\s+environment:")

    def test_workflow_cannot_publish_or_mutate_repository_state(self):
        forbidden = (
            "git push",
            "gh release",
            "gh pr ",
            "gh api ",
            "create-release",
            "pull_request_target",
            "workflow_run:",
            "repository_dispatch:",
        )
        for value in forbidden:
            with self.subTest(value=value):
                self.assertNotIn(value, self.contents)

    def test_all_actions_are_pinned_to_full_commit_shas(self):
        uses = re.findall(r"(?m)^\s*uses:\s*([^@\s]+)@([^\s]+)\s*$", self.contents)
        self.assertGreater(len(uses), 0)
        for action, revision in uses:
            with self.subTest(action=action):
                self.assertRegex(revision, FULL_SHA)


if __name__ == "__main__":
    unittest.main()
