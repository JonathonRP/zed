import json
import pathlib
import subprocess
import tempfile
import unittest

from rp_stable_sync import (
    RebaseRequest,
    StableSyncError,
    attempt_rebase,
    classify_handoff,
    manual_port_body,
    parse_port_request_marker,
    port_request_identity,
    port_request_marker,
)


def git(repo: pathlib.Path, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", repo, *args], text=True
    ).strip()


def commit_file(repo: pathlib.Path, path: str, contents: str, message: str) -> str:
    file = repo / path
    file.parent.mkdir(parents=True, exist_ok=True)
    file.write_text(contents, encoding="utf-8")
    subprocess.run(["git", "-C", repo, "add", path], check=True)
    subprocess.run(["git", "-C", repo, "commit", "-qm", message], check=True)
    return git(repo, "rev-parse", "HEAD")


class StableSyncTests(unittest.TestCase):
    def setUp(self):
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.repo = pathlib.Path(self.temporary_directory.name)
        subprocess.run(["git", "init", "-q", self.repo], check=True)
        subprocess.run(
            ["git", "-C", self.repo, "config", "user.email", "rp@example.invalid"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", self.repo, "config", "user.name", "RP Test"], check=True
        )
        commit_file(self.repo, "shared.txt", "base\n", "base")
        self.old_sha = git(self.repo, "rev-parse", "HEAD")

    def tearDown(self):
        self.temporary_directory.cleanup()

    def request(self, new_sha: str) -> RebaseRequest:
        return RebaseRequest(
            repo=self.repo,
            release_ref="release/rp-stable",
            old_tag="v1.18.0",
            old_sha=self.old_sha,
            old_version="1.18.0",
            new_tag="v1.22.0",
            new_sha=new_sha,
            new_version="1.22.0",
        )

    def handoff_report(
        self, release_tip: str, candidate: str
    ) -> dict[str, object]:
        return {
            "schema_version": 1,
            "outcome": "manual_port_required",
            "base_branch": "release/rp-stable",
            "release_tip": release_tip,
            "previous": {
                "tag": "v1.18.0",
                "commit": self.old_sha,
                "version": "1.18.0",
            },
            "candidate": {
                "tag": "v1.22.0",
                "commit": candidate,
                "version": "1.22.0",
            },
            "rebase": {
                "failed_commit": release_tip,
                "failed_subject": "RP patch",
                "conflicted_paths": ["shared.txt"],
            },
        }

    def handoff_pr(
        self,
        branch: str,
        head: str,
        body: str,
        head_repository: str = "JonathonRP/zed",
    ) -> dict[str, object]:
        return {
            "number": 23,
            "state": "open",
            "merged_at": None,
            "html_url": "https://github.com/JonathonRP/zed/pull/23",
            "body": body,
            "head": {
                "ref": branch,
                "sha": head,
                "repo": {"full_name": head_repository},
            },
            "base": {
                "ref": "release/rp-stable",
                "repo": {"full_name": "JonathonRP/zed"},
            },
        }

    def make_handoff_commits(self) -> tuple[str, str]:
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-qc", "release/rp-stable"],
            check=True,
        )
        release_tip = commit_file(self.repo, "rp.txt", "rp\n", "RP patch")
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-q", "--detach", self.old_sha],
            check=True,
        )
        candidate = commit_file(
            self.repo, "upstream.txt", "upstream\n", "new stable"
        )
        return release_tip, candidate

    def make_release_and_candidate(
        self, release_path: str, candidate_path: str
    ) -> tuple[str, str]:
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-qc", "release/rp-stable"],
            check=True,
        )
        release_tip = commit_file(self.repo, release_path, "rp\n", "RP patch")
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-q", "--detach", self.old_sha],
            check=True,
        )
        candidate = commit_file(
            self.repo, candidate_path, "upstream\n", "new stable"
        )
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-q", "release/rp-stable"],
            check=True,
        )
        return release_tip, candidate

    def test_clean_rebase_reports_rebased(self):
        release_tip, candidate = self.make_release_and_candidate(
            "rp.txt", "upstream.txt"
        )
        output = self.repo / "github-output"
        report = self.repo / "report.json"

        outcome = attempt_rebase(self.request(candidate), report, output)

        self.assertEqual(outcome, "rebased")
        self.assertFalse(report.exists())
        self.assertIn("outcome=rebased\n", output.read_text(encoding="utf-8"))
        self.assertIn(
            f"release_tip={release_tip}\n", output.read_text(encoding="utf-8")
        )
        self.assertTrue(
            subprocess.run(
                [
                    "git",
                    "-C",
                    self.repo,
                    "merge-base",
                    "--is-ancestor",
                    candidate,
                    "HEAD",
                ],
                check=False,
            ).returncode
            == 0
        )

    def test_conflict_writes_report_and_restores_release_tip(self):
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-qc", "release/rp-stable"],
            check=True,
        )
        release_tip = commit_file(
            self.repo, "shared.txt", "rp\n", "RP conflicting patch"
        )
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-q", "--detach", self.old_sha],
            check=True,
        )
        candidate = commit_file(
            self.repo, "shared.txt", "upstream\n", "new conflicting stable"
        )
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-q", "release/rp-stable"],
            check=True,
        )
        output = self.repo / "github-output"
        report_path = self.repo / "report.json"

        outcome = attempt_rebase(self.request(candidate), report_path, output)

        self.assertEqual(outcome, "conflict")
        self.assertEqual(git(self.repo, "rev-parse", "HEAD"), release_tip)
        self.assertEqual(
            git(self.repo, "status", "--porcelain", "--untracked-files=no"), ""
        )
        report = json.loads(report_path.read_text(encoding="utf-8"))
        self.assertEqual(report["release_tip"], release_tip)
        self.assertEqual(report["candidate"]["commit"], candidate)
        self.assertEqual(report["rebase"]["failed_commit"], release_tip)
        self.assertEqual(report["rebase"]["conflicted_paths"], ["shared.txt"])
        self.assertIn("outcome=conflict\n", output.read_text(encoding="utf-8"))

        body = manual_port_body(report, "https://example.invalid/run/1")
        self.assertIn("single handoff", body)
        self.assertIn("`shared.txt`", body)
        self.assertIn("https://example.invalid/run/1", body)
        self.assertIn(".github/rp-stable-port-request.json", body)
        self.assertIn("# Objective", body)
        self.assertIn("## Self-Review Checklist:", body)
        self.assertIn("Release Notes:", body)

    def test_invalid_port_report_is_rejected(self):
        with self.assertRaisesRegex(StableSyncError, "conflicted paths"):
            manual_port_body(
                {
                    "previous": {},
                    "candidate": {},
                    "rebase": {"conflicted_paths": []},
                },
                "https://example.invalid/run/1",
            )

    def test_port_request_marker_accepts_lf_and_crlf(self):
        release_tip, candidate = self.make_handoff_commits()
        report = self.handoff_report(release_tip, candidate)
        marker = port_request_marker(report)
        expected = port_request_identity(report)

        self.assertEqual(
            parse_port_request_marker(f"{marker}\n\n# Manual port\n"),
            expected,
        )
        self.assertEqual(
            parse_port_request_marker(f"{marker}\r\n\r\n# Manual port\r\n"),
            expected,
        )

    def test_port_request_marker_rejects_multiple_markers(self):
        release_tip, candidate = self.make_handoff_commits()
        marker = port_request_marker(
            self.handoff_report(release_tip, candidate)
        )

        with self.assertRaisesRegex(StableSyncError, "exactly one"):
            parse_port_request_marker(f"{marker}\n{marker}\n")

    def test_port_request_marker_rejects_malformed_identity(self):
        with self.assertRaisesRegex(
            StableSyncError, "invalid request identity"
        ):
            parse_port_request_marker(
                "<!-- rp-stable-port-request: {not json} -->"
            )

    def test_port_request_marker_rejects_near_matches_and_trailing_junk(self):
        release_tip, candidate = self.make_handoff_commits()
        marker = port_request_marker(
            self.handoff_report(release_tip, candidate)
        )

        for body in (
            f" {marker}\n",
            f"{marker} trailing junk\n",
            f"prefix {marker}\n",
            marker.replace("rp-stable-port-request", "rp-stable-port-requests"),
            marker.replace(" -->", " extra -->"),
            "<!-- rp-stable-port-request: [] -->",
        ):
            with self.subTest(body=body):
                with self.assertRaisesRegex(StableSyncError, "exactly one"):
                    parse_port_request_marker(body)

    def test_prepositioned_unrelated_handoff_branch_is_rejected(self):
        release_tip, candidate = self.make_handoff_commits()
        unrelated_tip = commit_file(
            self.repo, "unrelated.txt", "unrelated\n", "unrelated"
        )
        report = self.handoff_report(release_tip, candidate)

        with self.assertRaisesRegex(
            StableSyncError, "no durable request evidence"
        ):
            classify_handoff(
                self.repo,
                unrelated_tip,
                "automation/rp-stable-v1.22.0-manual-port",
                report,
                [],
                "JonathonRP/zed",
            )

    def test_handoff_pr_from_wrong_head_repository_is_rejected(self):
        release_tip, candidate = self.make_handoff_commits()
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-q", "release/rp-stable"],
            check=True,
        )
        report = self.handoff_report(release_tip, candidate)
        marker = self.repo / ".github/rp-stable-port-request.json"
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(json.dumps(report), encoding="utf-8")
        subprocess.run(
            ["git", "-C", self.repo, "add", ".github/rp-stable-port-request.json"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", self.repo, "commit", "-qm", "request port"], check=True
        )
        branch_tip = git(self.repo, "rev-parse", "HEAD")
        pr = self.handoff_pr(
            "automation/rp-stable-v1.22.0-manual-port",
            branch_tip,
            manual_port_body(report, "https://example.invalid/run/1"),
            head_repository="attacker/zed",
        )

        with self.assertRaisesRegex(
            StableSyncError, "head repository is not trusted"
        ):
            classify_handoff(
                self.repo,
                branch_tip,
                "automation/rp-stable-v1.22.0-manual-port",
                report,
                [pr],
                "JonathonRP/zed",
            )

    def test_stale_generated_request_identity_is_rejected(self):
        release_tip, candidate = self.make_handoff_commits()
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-q", "release/rp-stable"],
            check=True,
        )
        report = self.handoff_report(release_tip, candidate)
        stale_report = self.handoff_report(release_tip, candidate)
        stale_report["candidate"]["version"] = "1.21.0"
        marker = self.repo / ".github/rp-stable-port-request.json"
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(json.dumps(stale_report), encoding="utf-8")
        subprocess.run(
            ["git", "-C", self.repo, "add", ".github/rp-stable-port-request.json"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", self.repo, "commit", "-qm", "stale request"], check=True
        )

        with self.assertRaisesRegex(StableSyncError, "stale request identity"):
            classify_handoff(
                self.repo,
                "HEAD",
                "automation/rp-stable-v1.22.0-manual-port",
                report,
                [],
                "JonathonRP/zed",
            )

    def test_valid_generated_marker_is_reused(self):
        release_tip, candidate = self.make_handoff_commits()
        subprocess.run(
            ["git", "-C", self.repo, "switch", "-q", "release/rp-stable"],
            check=True,
        )
        report = self.handoff_report(release_tip, candidate)
        marker = self.repo / ".github/rp-stable-port-request.json"
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(json.dumps(report), encoding="utf-8")
        subprocess.run(
            ["git", "-C", self.repo, "add", ".github/rp-stable-port-request.json"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", self.repo, "commit", "-qm", "request port"], check=True
        )

        handoff = classify_handoff(
            self.repo,
            "HEAD",
            "automation/rp-stable-v1.22.0-manual-port",
            report,
            [],
            "JonathonRP/zed",
        )

        self.assertEqual(handoff.state, "generated_marker")
        self.assertEqual(handoff.pr_number, "")

    def test_valid_human_progress_branch_is_reused(self):
        release_tip, candidate = self.make_handoff_commits()
        report = self.handoff_report(release_tip, candidate)
        human_tip = commit_file(
            self.repo, "manual-port.txt", "curated\n", "curate port"
        )
        branch = "automation/rp-stable-v1.22.0-manual-port"
        pr = self.handoff_pr(
            branch,
            human_tip,
            manual_port_body(report, "https://example.invalid/run/1"),
        )

        handoff = classify_handoff(
            self.repo,
            human_tip,
            branch,
            report,
            [pr],
            "JonathonRP/zed",
        )

        self.assertEqual(handoff.state, "human_progress")
        self.assertEqual(handoff.pr_number, "23")
        self.assertEqual(
            handoff.pr_url, "https://github.com/JonathonRP/zed/pull/23"
        )

    def test_non_conflict_rebase_failure_stays_an_error(self):
        release_tip, _ = self.make_release_and_candidate(
            "rp.txt", "upstream.txt"
        )
        report = self.repo / "report.json"

        with self.assertRaisesRegex(StableSyncError, "without a conflict"):
            attempt_rebase(self.request("0" * 40), report)

        self.assertEqual(git(self.repo, "rev-parse", "HEAD"), release_tip)
        self.assertFalse(report.exists())

    def test_workflow_keeps_conflict_handoff_fail_closed(self):
        repo = pathlib.Path(__file__).resolve().parents[1]
        contents = (repo / ".github/workflows/rp_stable_sync.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("steps.rebase.outputs.outcome == 'rebased'", contents)
        self.assertIn("steps.rebase.outputs.outcome == 'conflict'", contents)
        self.assertIn("rp-stable-port-request.json", contents)
        self.assertIn("--draft", contents)
        self.assertIn("pulls?state=all&per_page=100", contents)
        self.assertNotIn("gh pr merge", contents)
        self.assertNotIn("gh release", contents)
        handoff = contents.split("- name: Surface manual stable port", 1)[1].split(
            "- name: Dispatch read-only compatibility validation", 1
        )[0]
        self.assertIn("-manual-port", handoff)
        self.assertIn("gh pr reopen", handoff)
        self.assertNotIn("--force", handoff)
        self.assertIn("expected-repository", handoff)
        self.assertIn("human_progress", handoff)


if __name__ == "__main__":
    unittest.main()
