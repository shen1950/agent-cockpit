"""AgentFind source counts must follow the index, not the live data directory.

A source whose data root is momentarily gone (client upgrade rotating its SQLite
file) still has indexed sessions. Reporting zero there tells the user their
history vanished. Run through the repository's agsync build gate.
"""
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "agentfind_overview",
    Path(__file__).resolve().parents[1] / "src-tauri/scripts/agentfind.py")
agentfind = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = agentfind
SPEC.loader.exec_module(agentfind)


class SourceCounts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cockpit-agentfind-counts-")
        self.root = Path(self.temp.name)
        self.saved = (agentfind.APP_DIR, agentfind.DB_PATH, agentfind.discover_sources)
        agentfind.APP_DIR = self.root
        agentfind.DB_PATH = self.root / "index.sqlite"
        self.con = agentfind.connect()
        self.con.execute(
            "insert into session(sid,source,label,title,project,project_name,started,ended,"
            "day,n_msg,models,branch,src_file,artifacts) "
            "values('zcode|s1','zcode','ZCode','一场对话','','',1,2,'2026-11-09',5,'','','','')")

    def tearDown(self):
        self.con.close()
        agentfind.APP_DIR, agentfind.DB_PATH, agentfind.discover_sources = self.saved
        self.temp.cleanup()

    def counts(self, discovered):
        agentfind.discover_sources = lambda: discovered
        return {s["source"]: s for s in agentfind.overview(self.con)["sources"]}

    def test_indexed_sessions_count_even_when_data_root_is_missing(self):
        sources = self.counts([])
        self.assertEqual(sources["zcode"]["n"], 1)
        self.assertEqual(sources["zcode"]["b"], 2)

    def test_discovered_source_keeps_its_count(self):
        sources = self.counts([("zcode", "ZCode", "opencode_db", [self.root])])
        self.assertEqual(sources["zcode"]["n"], 1)

    def test_agent_without_any_indexed_session_still_shows_zero(self):
        sources = self.counts([])
        self.assertEqual(sources["cursor"]["n"], 0)
        self.assertIsNone(sources["cursor"]["b"])

    def test_sparse_index_still_produces_example_terms(self):
        # Only the three fallbacks are available; topping up must not spin forever.
        terms = agentfind.example_terms(self.con)
        self.assertEqual(terms, ["报错", "总结", "README"])


if __name__ == "__main__":
    unittest.main()
