#!/usr/bin/env python3
"""Compare complete like-for-like NetGet eval reports; do not merge partial runs."""
import argparse
import json
import statistics
from pathlib import Path


def duration(runs):
    values = [r["elapsed_secs"] for r in runs]
    return {"runs": len(values), "median_seconds": statistics.median(values) if values else None,
            "mean_seconds": statistics.fmean(values) if values else None,
            "total_seconds": sum(values)}


def compare(before, after):
    for key in ("model", "runs_per_case", "seed", "temperature"):
        assert before[key] == after[key], (key, before[key], after[key])
    old = {c["id"]: c for c in before["cases"]}
    new = {c["id"]: c for c in after["cases"]}
    assert len(old) == len(before["cases"]) and len(new) == len(after["cases"]), "Duplicate case IDs"
    assert old.keys() == new.keys(), (old.keys() - new.keys(), new.keys() - old.keys())
    assert before["totals"]["runs_total"] == after["totals"]["runs_total"]
    for report in (before, after):
        for case in report["cases"]:
            assert len(case["runs"]) == case["attempts"], (case["id"], "Incomplete runs")
            assert sum(run["verdict"] == "pass" for run in case["runs"]) == case["passes"], case["id"]
    regressions, gains, rows = [], [], []
    for case_id in old:
        b, a = old[case_id], new[case_id]
        assert b["attempts"] == a["attempts"], (case_id, b["attempts"], a["attempts"])
        assert b["status"] == a["status"], (case_id, b["status"], a["status"])
        for field in ("protocol", "instruction", "client", "independence", "expectation", "status_reason"):
            assert b.get(field) == a.get(field), (case_id, field, b.get(field), a.get(field))
        row = {"case": case_id, "before_passes": b["passes"], "after_passes": a["passes"],
               "runs": a["attempts"], "before_latency": duration(b["runs"]),
               "after_latency": duration(a["runs"]), "before_failure": b["dominant_failure"],
               "after_failure": a["dominant_failure"]}
        rows.append(row)
        if a["passes"] < b["passes"]:
            regressions.append(row)
        elif a["passes"] > b["passes"]:
            gains.append(row)
    imap = []
    for case_id in old:
        if old[case_id]["protocol"] != "imap":
            continue
        counts = {}
        for label, result in (("before", old[case_id]), ("after", new[case_id])):
            counts[label] = [sum("send_imap_greeting" in line for line in run["executed_actions"])
                             for run in result["runs"]]
        imap.append({"case": case_id, "greeting_actions_per_run": counts})
    return {"imap_greetings": imap, "settings": {k: before[k] for k in ("model", "runs_per_case", "seed", "temperature")},
            "before": {"totals": before["totals"], "latency": duration([r for c in old.values() for r in c["runs"]])},
            "after": {"totals": after["totals"], "latency": duration([r for c in new.values() for r in c["runs"]])},
            "gains": gains, "regressions": regressions, "cases": rows}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    result = compare(json.loads(args.before.read_text()), json.loads(args.after.read_text()))
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({k: v for k, v in result.items() if k != "cases"}, indent=2))
