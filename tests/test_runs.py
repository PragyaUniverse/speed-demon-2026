import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "board"))
import runs  # noqa: E402


def row(slug, commit, ok=True, stage="scored", pts=50.0, track="a", **kw):
    r = {"team_slug": slug, "commit": commit, "track": track, "ok": ok, "stage": stage, "points": pts if ok else None,
         "K": 10.0, "T_s": 0.2, "M_bytes": 1e8, "S": 300, "ended_at": f"2026-10-31T12:{commit[-2:]}:00Z",
         "pushed_at": "2026-10-31T12:00:00Z", "gates": {"correctness": ok}, "failure": None if ok else "hash mismatch " + "x" * 500,
         "log_tail": "secret"}
    r.update(kw)
    return r


class RunsTest(unittest.TestCase):
    def test_newest_first_only_board_teams_no_starter(self):
        rows = [row("a", "c01"), row("b", "c02"), row("a", "c03", stage="unchanged", ok=False), row("a", "c04", ok=False, stage="run")]
        out = runs.collect(rows, {"a"})
        self.assertEqual(list(out), ["a"])
        self.assertEqual([r["c"] for r in out["a"]], ["c04", "c01"])
        fail, good = out["a"]
        self.assertEqual(fail["g"], "correctness")
        self.assertEqual(len(fail["f"]), runs.FAILURE_MAX)
        self.assertNotIn("pts", fail)
        self.assertEqual(good["pts"], 50.0)
        self.assertNotIn("log_tail", json.dumps(out))

    def test_rewrites_only_on_change(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "a.json")
            r = runs.collect([row("a", "c01")], {"a"})["a"]
            self.assertTrue(runs.write_if_changed(p, r))
            self.assertFalse(runs.write_if_changed(p, r))
            self.assertTrue(runs.write_if_changed(p, runs.collect([row("a", "c01"), row("a", "c02")], {"a"})["a"]))


if __name__ == "__main__":
    unittest.main()
