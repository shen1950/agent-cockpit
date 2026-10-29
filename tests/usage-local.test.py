"""Accounting regression cases. Run through the repository's agsync build gate."""
import datetime as dt
from contextlib import closing
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("usage_local", Path(__file__).resolve().parents[1] / "src-tauri/scripts/usage-local.py")
usage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(usage)


class UsageAccounting(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cockpit-usage-accounting-")
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def jsonl(self, name, rows):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("\n".join(json.dumps(row, ensure_ascii=False) for row in rows), encoding="utf8")
        return path

    def codex(self, at, total, last=None):
        info = {"total_token_usage": {"total_tokens": total}}
        if last is not None:
            info["last_token_usage"] = {"total_tokens": last}
        return {"type": "event_msg", "timestamp": at, "payload": {"type": "token_count", "info": info}}

    def test_codex_cross_day_cumulative_and_quota_refresh_are_not_double_counted(self):
        self.jsonl("sessions/a.jsonl", [
            self.codex("2026-10-01T12:00:00+08:00", 100, 100),
            self.codex("2026-10-01T12:01:00+08:00", 100, 100),
            self.codex("2026-10-02T12:00:00+08:00", 160),
            self.codex("2026-10-02T12:01:00+08:00", 20, 20),
        ])
        result = usage.collect("codex", self.root)
        self.assertEqual(result["total"], 180)
        self.assertEqual([day["tokens"] for day in result["days"]], [100, 80])
        self.assertEqual(result["requests"], 3)

    def test_codex_without_explicit_total_does_not_add_cache_or_reasoning_twice(self):
        values = usage.usage_tokens({"input_tokens": 100, "output_tokens": 25,
                                     "cached_input_tokens": 80, "reasoning_output_tokens": 20})
        self.assertEqual(values, (100, 25, 80, 125))

    def test_history_retains_real_event_time_and_never_invents_date_only_time(self):
        self.jsonl("sessions/a.jsonl", [self.codex("2026-10-01T12:00:00+08:00", 100, 100)])
        result = usage.collect("codex", self.root)
        self.assertEqual(result["_records"][0]["timestamp"], 1790827200)
        self.assertIsNone(usage.precise_timestamp("2026-10-01"))
        self.assertEqual(usage.precise_timestamp(1790827200000), 1790827200)
        self.assertIsNone(usage.precise_timestamp(0))
        self.assertIsNone(usage.precise_timestamp(float("nan")))

    def test_claude_duplicate_fragments_use_largest_usage_and_split_cache(self):
        rows = []
        for uid, output in [("a", 5), ("b", 20), ("b", 20)]:
            rows.append({"type": "assistant", "uuid": uid, "timestamp": "2026-10-01T12:00:00+08:00",
                         "message": {"id": "one-response", "role": "assistant", "model": "model-a",
                                     "usage": {"input_tokens": 100, "output_tokens": output,
                                               "cache_read_input_tokens": 30, "cache_creation_input_tokens": 5}}})
        self.jsonl("projects/session.jsonl", rows)
        result = usage.collect("claude", self.root)
        self.assertEqual(result["total"], 155)
        self.assertEqual(result["requests"], 1)
        self.assertEqual(result["estimated_tokens"], 0)

    def test_workbuddy_function_and_message_share_provider_message_id(self):
        self.jsonl("projects/session.jsonl", [
            {"id": "tool-block", "type": "function_call", "timestamp": 1790827200000,
             "providerData": {"messageId": "same", "model": "custom-model", "usage": {
                 "input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 90, "total_tokens": 120}}},
            {"id": "message-block", "type": "message", "role": "assistant", "timestamp": 1790827200000,
             "providerData": {"messageId": "same", "model": "custom-model", "usage": {
                 "input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 90, "total_tokens": 120}}},
        ])
        result = usage.collect("workbuddy", self.root)
        self.assertEqual(result["total"], 120)
        self.assertEqual(result["requests"], 1)

    def test_visible_text_is_explicitly_estimated_and_images_are_excluded(self):
        self.jsonl("projects/session.jsonl", [
            {"type": "user", "message": {"role": "user", "content": "你好abcd"}},
            {"type": "assistant", "timestamp": "2026-10-01T12:00:00+08:00", "message": {
                "id": "reply", "role": "assistant", "model": "byok", "content": [
                    {"type": "text", "text": "测试1234"}, {"type": "image", "data": "x" * 10000}]}}
        ])
        result = usage.collect("qoder", self.root)
        self.assertEqual(result["exact_tokens"], 0)
        self.assertEqual(result["estimated_tokens"], 6)
        self.assertIn("可见", result["estimate_method"])
        self.assertIn("隐藏", result["coverage"])

    def test_zcode_provider_total_and_real_timing_preserve_byok(self):
        path = self.root / "cli/db/db.sqlite"
        path.parent.mkdir(parents=True)
        with closing(sqlite3.connect(path)) as con, con:
            con.execute("CREATE TABLE model_usage(id TEXT,session_id TEXT,started_at INTEGER,model_id TEXT,provider_id TEXT, input_tokens INTEGER,output_tokens INTEGER,cache_read_input_tokens INTEGER,provider_total_tokens INTEGER,computed_total_tokens INTEGER,duration_ms INTEGER,time_to_first_token_ms INTEGER,status TEXT)")
            con.execute("INSERT INTO model_usage VALUES('r','s',1790827200000,'custom-model','custom-provider',100,20,90,120,210,2000,500,'completed')")
        result = usage.collect("zcode", self.root)
        self.assertEqual(result["total"], 120)
        self.assertEqual(result["timed_output_tokens"], 20)
        self.assertEqual(result["duration_ms"], 2000)
        self.assertEqual(result["ttft_ms"], 500)
        self.assertEqual(result["models"][0]["provider"], "custom-provider")

    def test_opencode_migrated_records_use_new_table_once_and_reasoning_is_separate(self):
        path = self.root / "opencode.db"
        with closing(sqlite3.connect(path)) as con, con:
            for table in ("message", "session_message"):
                con.execute("CREATE TABLE " + table + "(id TEXT,session_id TEXT,type TEXT,time_created INTEGER,data TEXT)")
            old = {"role": "assistant", "modelID": "model", "tokens": {"total": 999}}
            new = {"model": {"id": "model", "providerID": "custom"}, "tokens": {
                "input": 100, "output": 20, "reasoning": 5, "cache": {"read": 30, "write": 10}}}
            for table, obj in (("message", old), ("session_message", new)):
                con.execute("INSERT INTO " + table + " VALUES('same-id','s','assistant',1790827200000,?)", (json.dumps(obj),))
        result = usage.collect("opencode", self.root)
        self.assertEqual(result["total"], 165)
        self.assertEqual(result["requests"], 1)

    def test_minimax_uses_inner_timestamp_and_token_total(self):
        self.jsonl("v2/sessions/s/messages.jsonl", [{"message_id": "m", "message": {
            "role": "assistant", "timestamp": 1790827200000, "model": "custom",
            "usage": {"input": 100, "output": 20, "cacheRead": 30, "totalTokens": 150}}}])
        result = usage.collect("minimax", self.root)
        self.assertEqual(result["days"][0]["tokens"], 150)
        self.assertEqual(result["input_tokens"], 100)

    def test_cache_changes_after_append_and_never_contains_message_text(self):
        path = self.jsonl("projects/s.jsonl", [{"type": "assistant", "timestamp": "2026-10-01T12:00:00Z",
            "message": {"id": "first", "role": "assistant", "content": "private-message-marker"}}])
        directory = self.root / "cache"
        before = usage.collect("qoder", self.root, directory)
        self.assertEqual(before, usage.collect("qoder", self.root, directory))
        cache_text = next(directory.glob("*.json")).read_text(encoding="utf8")
        self.assertNotIn("private-message-marker", cache_text)
        with path.open("a", encoding="utf8") as f:
            f.write("\n" + json.dumps({"type": "assistant", "timestamp": "2026-10-02T12:00:00Z",
                "message": {"id": "second", "role": "assistant", "content": "abcd"}}))
        self.assertEqual(usage.collect("qoder", self.root, directory)["total"], before["total"] + 1)

    def test_blank_isolated_or_unsupported_roots_do_not_fall_back_to_native(self):
        self.assertIsNone(usage.collect("codex", ""))
        self.assertIsNone(usage.collect("codex", self.root))
        self.jsonl("projects/session.jsonl", [{"type": "assistant", "message": {"content": "not a full conversation"}}])
        self.assertIsNone(usage.collect("traework-cn", self.root))


if __name__ == "__main__":
    unittest.main()
