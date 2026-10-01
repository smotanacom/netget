"""Exercise reporting with files left behind by interrupted evidence steps."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("summarize-evidence.py").resolve()


class EvidenceSummaryTest(unittest.TestCase):
    def report(self, status=None, log=None, complete=True):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            results = root / "evidence-results"
            results.mkdir()
            (results / "healthy.status").write_text("passed\thttp::real_client_test\n")
            (results / "healthy.log").write_text("test result: ok. 3 passed; 0 failed;\n")
            if status is not None:
                (results / "interrupted.status").write_text(status)
            if log is not None:
                (results / "interrupted.log").write_text(log)
            if complete:
                (results / "complete").touch()
            summary = root / "summary.md"
            run = subprocess.run(
                [sys.executable, str(SCRIPT)], cwd=root,
                env={**os.environ, "GITHUB_STEP_SUMMARY": str(summary), "EVIDENCE_BUILD": "success"},
                capture_output=True, text=True, check=True,
            )
            self.assertEqual(run.stdout, summary.read_text() + "\n")
            return summary.read_text()

    def test_complete_success_requires_positive_test_evidence(self):
        report = self.report()
        self.assertIn("### Real-client evidence: PASSED", report)
        self.assertIn("1/1 started groups, 3 passed tests", report)

    def test_partial_status_files_do_not_discard_healthy_groups(self):
        for status in ["", "passed", "passed\t", "unexpected\tsmb::real_client_test", "passed\tsmb\textra"]:
            with self.subTest(status=status):
                report = self.report(status)
                self.assertIn("### Real-client evidence: FAILED / INCOMPLETE", report)
                self.assertIn("| `http::real_client_test` | passed | 3 |", report)
                self.assertIn("INCOMPLETE (malformed status)", report)

    def test_missing_zero_or_truncated_logs_are_not_success(self):
        for log in [None, "", "test result: ok. 0 passed; 0 failed;", "test result: ok. 2"]:
            with self.subTest(log=log):
                report = self.report("passed\tsmb::real_client_test", log)
                self.assertIn("### Real-client evidence: FAILED / INCOMPLETE", report)
                self.assertIn("INCOMPLETE (no passing test result)", report)

    def test_a_loop_without_its_completion_marker_is_incomplete(self):
        report = self.report(complete=False)
        self.assertIn("### Real-client evidence: FAILED / INCOMPLETE", report)
        self.assertIn("unstarted groups have no evidence", report)


if __name__ == "__main__":
    unittest.main()
