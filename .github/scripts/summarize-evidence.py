#!/usr/bin/env python3
"""Publish partial evidence as well as a complete run; never infer success from silence."""

import os
from pathlib import Path
import re

results = Path("evidence-results")
rows = []
for status_file in sorted(results.glob("*.status")):
    status, name = status_file.read_text().strip().split("\t", 1)
    log = status_file.with_suffix(".log")
    counts = re.findall(r"test result: ok\. (\d+) passed;", log.read_text(errors="replace")) if log.exists() else []
    rows.append((name, status, sum(map(int, counts))))

complete = (results / "complete").exists()
passed = sum(status == "passed" for _, status, _ in rows)
verdict = "PASSED" if complete and rows and passed == len(rows) else "FAILED / INCOMPLETE"
summary = [
    f"### Real-client evidence: {verdict}",
    "",
    f"Build: {os.environ.get('EVIDENCE_BUILD', 'unknown')}. "
    f"Completed successfully: {passed}/{len(rows)} started groups, "
    f"{sum(count for _, status, count in rows if status == 'passed')} passed tests.",
    "" if complete else "The loop did not finish; unstarted groups have no evidence in this run.",
    "",
    "| Group | Result | Passed tests |",
    "| --- | --- | ---: |",
]
summary.extend(f"| `{name}` | {status} | {count} |" for name, status, count in rows)
summary.extend(["", "### Nostr real-browser evidence", "",
                f"Chromium setup: {os.environ.get('NOSTR_BROWSER_SETUP', 'not run')}. "
                f"Browser exchange: {os.environ.get('NOSTR_BROWSER', 'not run')}."])
text = "\n".join(summary) + "\n"
print(text)
with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as output:
    output.write(text)
