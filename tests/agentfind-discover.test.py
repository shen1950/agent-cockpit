"""discover_sources() 本体必须能跑通。

2026-11-19 实录：AStudio 路径重构把 astudio_sessions 改名成 astudio_home，
agent_roots 里漏改一处引用，/api/meta 每次 NameError 回 500；启动自证探活
恰好用 /api/meta，于是服务把每个端口都误判成"影子绑定"、漂完 10 口自杀。
既有测试把 discover_sources 整个替身掉，真函数从未被执行过。这里调真的。
"""
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "agentfind_discover",
    Path(__file__).resolve().parents[1] / "src-tauri/scripts/agentfind.py")
agentfind = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = agentfind
SPEC.loader.exec_module(agentfind)


class DiscoverSources(unittest.TestCase):
    def test_real_discover_sources_runs(self):
        sources = agentfind.discover_sources()
        self.assertIsInstance(sources, list)
        sids = [s[0] for s in sources]
        self.assertEqual(len(sids), len(set(sids)), "源 id 不许重复")

    def test_overview_with_real_sources(self):
        with tempfile.TemporaryDirectory(prefix="cockpit-agentfind-discover-") as temp:
            root = Path(temp)
            saved = (agentfind.APP_DIR, agentfind.DB_PATH)
            agentfind.APP_DIR, agentfind.DB_PATH = root, root / "index.sqlite"
            try:
                con = agentfind.connect()
                try:
                    ov = agentfind.overview(con)
                    self.assertIn("sources", ov)
                finally:
                    con.close()
            finally:
                agentfind.APP_DIR, agentfind.DB_PATH = saved


if __name__ == "__main__":
    unittest.main()