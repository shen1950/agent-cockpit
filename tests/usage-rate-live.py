"""Check real local token timestamps without reading or exporting message contents.

Run from the project's agsync build test batch. Account histories and credentials
are opened read-only; output consists only of an aggregate local time range,
event count and token count. Machines with no recent ChatGPT event are skipped.
"""
import datetime as dt
import importlib.util
import json
import os
from pathlib import Path

PROJECT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("usage_local", PROJECT / "src-tauri/scripts/usage-local.py")
usage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(usage)


def cockpit_root():
    explicit = os.environ.get("AGENT_COCKPIT_HOME", "").strip()
    if explicit:
        return Path(explicit).expanduser()
    pointer = Path.home() / ".agent-cockpit-root"
    try:
        value = pointer.read_text(encoding="utf-8-sig").strip()
        if value:
            return Path(value).expanduser()
    except OSError:
        pass
    return Path.home() / ".agent-cockpit"


def account_homes():
    root = cockpit_root()
    folder = root / "instances" / "codex-desktop"
    allowed = (root / "chatgpt-auth").resolve()
    homes = set()
    for file in folder.glob("*.json"):
        try:
            meta = json.loads(file.read_text(encoding="utf-8-sig"))
            home = Path(meta.get("chatgpt_home") or "").resolve()
            # Mirroring the app's managed-home boundary prevents probing an
            # arbitrary path supplied by a stale metadata entry.
            if home != allowed and allowed in home.parents and (home / "sessions").is_dir():
                homes.add(home)
        except (OSError, ValueError, TypeError):
            continue
    return homes


def recent_session_files(homes):
    found = []
    for home in homes:
        for path in (home / "sessions").rglob("*.jsonl"):
            try:
                found.append((path.stat().st_mtime, home, path))
            except OSError:
                continue
    found.sort(key=lambda row: row[0], reverse=True)
    return found[:24]


def recorded_events(path, cutoff):
    """Read only independent counters and timestamps from Codex token events."""
    previous = None
    result = []
    try:
        with path.open("r", encoding="utf-8-sig", errors="replace") as stream:
            for line in stream:
                if '"token_count"' not in line:
                    continue
                try:
                    item = json.loads(line)
                    payload = item.get("payload") or {}
                    if payload.get("type") != "token_count":
                        continue
                    info = payload.get("info") or {}
                    current = info.get("total_token_usage") or {}
                    last = info.get("last_token_usage") or {}
                    total = current.get("total_tokens")
                    if not isinstance(total, (int, float)) or total < 0:
                        continue
                    if previous is not None and total == previous:
                        continue
                    increment = total - previous if previous is not None and total >= previous else last.get("total_tokens") or total
                    previous = total
                    stamp = item.get("timestamp") or ""
                    if not isinstance(stamp, str) or "T" not in stamp:
                        continue
                    seconds = dt.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
                    if seconds >= cutoff and isinstance(increment, (int, float)) and increment > 0:
                        result.append((round(seconds, 3), float(increment)))
                except (ValueError, TypeError, AttributeError, OSError, OverflowError):
                    continue
    except OSError:
        return []
    return result


def main():
    cutoff = dt.datetime.now(dt.timezone.utc).timestamp() - 7 * 24 * 60 * 60
    chosen = None
    for _, home, path in recent_session_files(account_homes()):
        events = recorded_events(path, cutoff)
        if events:
            chosen = (home, events)
            break
    if chosen is None:
        print("SKIP: no recent readable ChatGPT token_count history on this machine")
        return
    home, events = chosen
    # Avoid writing cache/evidence into any account's own history directory.
    result = usage.collect("chatgpt", home)
    if not result:
        raise AssertionError("a recent ChatGPT token event was found but the collector returned no usage")
    recorded = {(round(row["timestamp"], 3), float(row.get("exact_tokens", 0)))
                for row in result.get("_records", [])
                if isinstance(row.get("timestamp"), (int, float)) and row.get("exact_tokens", 0) > 0}
    matches = [event for event in events if event in recorded]
    if not matches:
        raise AssertionError("recent token_count timestamps and increments were not retained by the collector")
    first = dt.datetime.fromtimestamp(min(t for t, _ in matches)).astimezone().strftime("%Y-%m-%d %H:%M")
    last = dt.datetime.fromtimestamp(max(t for t, _ in matches)).astimezone().strftime("%Y-%m-%d %H:%M")
    print(f"PASS: real ChatGPT token events {first} to {last}; {len(matches)} records; {sum(n for _, n in matches):.0f} tokens")


if __name__ == "__main__":
    main()
