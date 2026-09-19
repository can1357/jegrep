import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("runner", Path(__file__).with_name("run.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class Scoring(unittest.TestCase):
    def setUp(self):
        self.case = dict(expected=["a.c", "b.c"], irrelevant=["wrong.c"],
                         spans=[dict(file="a.c", start=10, end=19, symbol="a"),
                                dict(file="a.c", start=30, end=39, symbol="b"),
                                dict(file="b.c", start=1, end=10, symbol="c")])

    def test_partial_recall_unknowns_and_ranking(self):
        result = {"hits": [dict(path="extra.c"), dict(path="a.c"), dict(path="wrong.c")]}
        s = runner.score(self.case, result)
        self.assertEqual((s["tp"], s["fp"], s["fn"], s["unjudged"]), (1, 1, 1, 1))
        self.assertEqual(s["file_recall"], .5)
        self.assertEqual(s["reciprocal_rank"], .5)
        self.assertIsNone(s["precision"])
        self.assertIsNone(s["f2"])
        self.assertAlmostEqual(s["precision_lower_bound"], 1/3)
        self.assertEqual(s["span_recall"], 0)

    def test_closed_world(self):
        s = runner.score(self.case, {"hits": [dict(path="a.c"), dict(path="extra.c")]}, closed_world=True)
        self.assertEqual((s["fp"], s["unjudged"], s["precision"], s["f2"]), (1, 0, .5, .5))

    def test_ranges_deduplicate_overlaps_and_use_ranking(self):
        result = {"hits": [dict(path="a.c", ranges=[dict(start=30, end=39, p=.1),
                              dict(start=10, end=14, p=.8), dict(start=13, end=19, p=.7)])]}
        s = runner.score(self.case, result, top_ranges=2)
        self.assertEqual(s["predicted_lines"], 10)
        self.assertEqual(s["overlapping_lines"], 10)
        self.assertAlmostEqual(s["span_recall"], 1/3)
        self.assertAlmostEqual(s["line_recall"], 1/3)
        self.assertAlmostEqual(s["mean_span_coverage"], 1/3)
        self.assertEqual(s["line_precision_lower_bound"], 1)

    def test_no_heat_does_not_get_region_credit(self):
        s = runner.score(self.case, {"hits": [dict(path="a.c", lines_seen=2000)]})
        self.assertEqual(s["span_recall"], 0)
        self.assertEqual(s["file_recall"], .5)

    def test_inclusive_lines_and_overlapping_gold(self):
        case = dict(expected=["a.c"], irrelevant=[], spans=[dict(file="a.c", start=1, end=2), dict(file="a.c", start=2, end=3)])
        s = runner.score(case, {"hits": [dict(path="a.c", ranges=[dict(start=2, end=2, p=1)])]})
        self.assertEqual(s["gold_lines"], 3)
        self.assertEqual(s["overlapping_lines"], 1)
        self.assertEqual(s["span_recall"], 1)
        self.assertAlmostEqual(s["line_recall"], 1/3)

    def test_empty_negative_case(self):
        case = dict(expected=[], irrelevant=[], spans=[], exhaustive=True)
        s = runner.score(case, {"hits": []})
        self.assertTrue(s["query_success"])
        self.assertEqual(s["precision"], 1)
        self.assertIsNone(s["span_recall"])
        s = runner.score(case, {"hits": [dict(path="a.c")]})
        self.assertFalse(s["query_success"])
        self.assertEqual(s["fp"], 1)

    def test_duplicate_hits_do_not_inflate_tp(self):
        s = runner.score(self.case, {"hits": [dict(path="a.c"), dict(path="a.c")]})
        self.assertEqual(s["tp"], 1)

    def test_directory_prefix_boundary(self):
        case = dict(expected=["src/foo/"], irrelevant=[], spans=[])
        s = runner.score(case, {"hits": [dict(path="src/foobar/a.c")]})
        self.assertEqual(s["file_recall"], 0)

    def test_partial_experiment_cannot_be_pareto(self):
        rows = []
        for name, cost, status in [("good", .1, "ok"), ("worse", .2, "ok"), ("broken", 0, "failed")]:
            for i in range(2):
                rows.append(dict(config=dict(name=name, strategy="baseline"), status=status,
                                 score=runner.score(self.case, {"hits": []}), wall_seconds=1,
                                 result=dict(stats=dict(usd=cost))))
        rows.append(dict(rows[0], config=dict(name="partial", strategy="baseline")))
        summaries = {s["name"]: s for s in runner.aggregate(rows, 2)}
        self.assertTrue(summaries["good"]["pareto"])
        self.assertFalse(summaries["worse"]["pareto"])
        self.assertFalse(summaries["broken"]["pareto"])
        self.assertFalse(summaries["partial"]["pareto"])

    def test_micro_recall_counts_targets_instead_of_weighting_each_query_equally(self):
        rows = []
        for files, found in [(["a.c"], ["a.c"]), (["b.c", "c.c", "d.c"], ["b.c"])]:
            case = dict(expected=files, irrelevant=[], spans=[dict(file=f, start=1, end=1) for f in files])
            hits = [dict(path=f, ranges=[dict(start=1, end=1, p=1)]) for f in found]
            rows.append(dict(config=dict(name="mixed", strategy="window"), status="ok",
                             score=runner.score(case, dict(hits=hits)), wall_seconds=1,
                             result=dict(stats=dict(usd=.001))))
        summary = runner.aggregate(rows, 2)[0]
        self.assertAlmostEqual(summary["file_recall"], 2/3)
        self.assertEqual(summary["micro_file_recall"], .5)
        self.assertAlmostEqual(summary["span_recall"], 2/3)
        self.assertEqual(summary["micro_span_recall"], .5)
        self.assertEqual((summary["span_tp"], summary["span_fn"]), (2, 2))


class EndToEnd(unittest.TestCase):
    def test_folder_all_strategies_repeats_resume_and_manifest_guard(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            suite = root / "suite"
            suite.mkdir()
            target = root / "target"
            target.mkdir()
            (target / "a.c").write_text("one\ntwo\nthree\n")
            (suite / "tag.json").write_text(json.dumps(dict(local=str(target))))
            (suite / "query_a.json").write_text(json.dumps(dict(query="find a", keywords=["SECRET_ORACLE"],
                                                               matches=[dict(file="a.c", start_line=2, end_line=3)])))
            fake = root / "fake"
            fake.write_text('#!/usr/bin/env python3\nimport sys,json\n'
                            'if "--list-strategies" in sys.argv: print("baseline\\nbeam")\n'
                            'else:\n'
                            ' assert "SECRET_ORACLE" not in " ".join(sys.argv)\n'
                            ' print(json.dumps({"hits":[{"path":"a.c","ranges":[{"start":2,"end":3,"p":1}]}],"stats":{"usd":.001,"errors":0}}))\n')
            fake.chmod(0o755)
            command = [sys.executable, str(Path(runner.__file__)), str(suite), "--binary", str(fake),
                       "--out", str(root / "out"), "--strategies", "all", "--repeat", "2", "--allow-concurrent"]
            first = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            rows = (root / "out/rows.jsonl").read_text().splitlines()
            self.assertEqual(len(rows), 4)
            self.assertTrue(all(json.loads(r)["score"]["line_recall"] == 1 for r in rows))
            resumed = subprocess.run(command + ["--resume"], capture_output=True, text=True)
            self.assertEqual(resumed.returncode, 0, resumed.stderr)
            self.assertEqual((root / "out/rows.jsonl").read_text().splitlines(), rows)
            mismatch = subprocess.run(command + ["--resume", "--ranges", "32"], capture_output=True, text=True)
            self.assertEqual(mismatch.returncode, 2)
            self.assertIn("identical settings", mismatch.stderr)

    def test_invalid_ground_truth_and_paths(self):
        for path in ("/absolute", "../escape", "a/../../escape", "a\\b"):
            with self.assertRaises(ValueError):
                runner.relative_path(path)
        with tempfile.TemporaryDirectory() as temp:
            p = Path(temp)
            (p / "a.c").write_text("x\n")
            suite = p / "cases.json"
            suite.write_text(json.dumps([dict(name="a", query="a", matches=[dict(file="a.c", start_line=1, end_line=2)])]))
            with self.assertRaisesRegex(ValueError, "invalid span"):
                runner.load_suite(suite, p)


if __name__ == "__main__":
    unittest.main()
