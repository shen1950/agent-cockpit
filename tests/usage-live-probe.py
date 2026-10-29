"""Optional local integration evidence; run only through agsync build.

Reads histories without exporting any message text or credentials. Evidence is
ignored coordination data and contains only the dashboard's aggregate fields.
"""
import importlib.util
import json
from pathlib import Path
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("usage_local", ROOT / "src-tauri/scripts/usage-local.py")
usage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(usage)
evidence = ROOT / ".agent-sync/usage-evidence"
evidence.mkdir(parents=True, exist_ok=True)
home = Path.home()
sources = []
for app, relative in (
    ("codex", ".codex"), ("claude", ".claude"), ("zcode", ".zcode"),
    ("workbuddy", ".workbuddy"), ("workbuddy-intl", ".workbuddy-ai"),
    ("qoder", ".qoder"), ("qoder-cn", ".qoder-cn"), ("qwenwork", ".qwenworkcn"),
    ("minimax", ".minimax"), ("opencode", ".local/share/opencode"), ("marvis", ".marvis"),
):
    started = time.monotonic()
    value = usage.collect(app, home / relative, evidence / "cache")
    if not value:
        print(f"{app}: no readable records")
        continue
    value.pop("_records", None)
    for day in value["days"]:
        assert abs(day["tokens"] - day["exact_tokens"] - day["estimated_tokens"]) < 0.1
        assert len(day["date"]) == 10
    value.update(id=app, app_id=app, app_name=app, instance_name="本机共用记录",
                 status="ready", count_in_total=True)
    sources.append(value)
    print(f"{app}: {len(value['days'])} days; exact={value['exact_tokens']:.0f}; "
          f"estimated={value['estimated_tokens']:.0f}; elapsed={time.monotonic()-started:.2f}s")
(evidence / "real-usage.json").write_text(json.dumps({"generated_at":time.time(),"sources":sources,
    "monitor":{"samples":1,"tokens_per_minute":None}}, ensure_ascii=False), encoding="utf-8")
print(f"Aggregate evidence saved for {len(sources)} readable sources.")
