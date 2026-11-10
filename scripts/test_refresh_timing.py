import json
import subprocess
import sys
import unittest
from pathlib import Path

from summarize_refresh_timing import parse_event, summarize


def line(phase, us, refresh="r1", outcome="ok"):
    return f'INFO refresh_round{{refresh_id={refresh}}}:refresh_source{{source_mid=123}}:post{{post_id=456}}: phase="{phase}" elapsed_us={us} outcome="{outcome}" 刷新阶段耗时'


class TimingSummaryTests(unittest.TestCase):
    def test_text_and_json_context(self):
        text = parse_event("\x1b[32m" + line("model_request", 1200) + "\x1b[0m")
        self.assertEqual(text["elapsed_us"], 1200)
        self.assertEqual(text["post_id"], "456")
        encoded = json.dumps({"spans": [{"refresh_id": "r1"}, {"source_mid": "123", "post_id": "456"}], "fields": {"phase": "model_request", "elapsed_us": 1200, "outcome": "ok"}})
        self.assertEqual(parse_event(encoded), text)

    def test_parallel_stage_totals_are_not_wall_time(self):
        events = [parse_event(line("model_request", 2000000)), parse_event(line("model_request", 3000000)), parse_event(line("refresh_round", 3500000))]
        report = summarize(events)
        self.assertEqual(report["wall_ms"], [3500])
        model = next(row for row in report["phases"] if row["phase"] == "model_request")
        self.assertEqual(model["total_ms"], 5000)
        self.assertEqual(model["avg_ms"], 2500)
        self.assertEqual(model["p95_ms"], 3000)

    def test_latest_filter_and_errors(self):
        events = [parse_event(line("model_request", 1)), parse_event(line("model_request", 2000, "r2", "error")), parse_event(line("post_analysis", 3000, "r2", "incomplete"))]
        report = summarize(events, latest=True)
        self.assertEqual(report["refresh_id"], "r2")
        self.assertEqual(report["event_count"], 2)
        self.assertEqual(sum(row["errors"] for row in report["phases"]), 1)
        self.assertEqual(sum(row["incomplete"] for row in report["phases"]), 1)
        self.assertEqual(summarize(events, "not-found")["event_count"], 0)

    def test_incomplete_round_is_not_a_completed_wall_measurement(self):
        report = summarize([parse_event(line("refresh_round", 9000, outcome="incomplete"))])
        self.assertEqual(report["wall_ms"], [])
        self.assertEqual(report["phases"][0]["incomplete"], 1)

    def test_rejects_unrelated_and_invalid_samples(self):
        for value in ["other log", line("model_request", -1), line("model_request", "oops"), line("unknown", 1), json.dumps({"phase":"model_request","elapsed_us":True,"outcome":"ok"})]:
            self.assertIsNone(parse_event(value))

    def test_cli_real_entrypoint(self):
        script = Path(__file__).with_name("summarize_refresh_timing.py")
        run = subprocess.run([sys.executable, str(script), "--json", "--latest"], input=line("refresh_round", 2000) + "\n", text=True, encoding="utf-8", capture_output=True, check=False)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(json.loads(run.stdout)["wall_ms"], [2])
        empty = subprocess.run([sys.executable, str(script)], input="", text=True, encoding="utf-8", capture_output=True, check=False)
        self.assertEqual(empty.returncode, 2)


if __name__ == "__main__":
    unittest.main()
