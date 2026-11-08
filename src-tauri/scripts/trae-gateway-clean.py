"""Remove AgentCockpit gateway models from Trae's cached model list.

Trae keeps its custom OpenAI-compatible providers in the SQLite item table
`<uid>:AI.agent.model.model_list_map`. Every entry carries its base URL, so the
gateway is identifiable from the data itself. Only entries that point at the
local gateway are removed; custom providers the user added by hand stay.
Selection records that still name a removed model are pruned as well, otherwise
Trae would keep a model id that no longer exists.
"""
import json
import sqlite3
import sys
from contextlib import closing

MAP_SUFFIX = "AI.agent.model.model_list_map"
SELECT_SUFFIXES = (
    "AI.agent.model.session_selected_model",
    "AI.agent.model.recent_user_selection_by_agent_label",
    "AI.agent.model.reasoning_effort_level_by_agent_model_v2",
    "AI.agent.model.max_mode_by_agent_model",
)


def is_gateway(entry, hosts):
    if not isinstance(entry, dict):
        return False
    base = str(entry.get("base_url") or "").strip().lower()
    return bool(base) and any(host in base for host in hosts)


def model_tokens(entry):
    """Identifiers Trae may use to remember a selection of this model."""
    out = set()
    for key in ("name", "config_name", "display_name", "custom_model_id"):
        value = entry.get(key)
        if isinstance(value, str) and value.strip():
            out.add(value.strip())
    return out


def has_token(text, tokens):
    """Match a whole identifier, not a prefix of a longer model name."""
    if not isinstance(text, str) or not text:
        return False
    for token in tokens:
        start = text.find(token)
        while start != -1:
            end = start + len(token)
            after = text[end:end + 1]
            before = text[start - 1:start] if start else ""
            right_ok = not after or not (after.isalnum() or after in "./")
            left_ok = not before or not (before.isalnum() or before in "./")
            if right_ok and left_ok:
                return True
            start = text.find(token, start + 1)
    return False


def references(value, tokens):
    if isinstance(value, str):
        return has_token(value, tokens)
    if isinstance(value, list):
        return any(references(item, tokens) for item in value)
    if isinstance(value, dict):
        return any(references(item, tokens) for item in value.values())
    return False


def prune_selection(value, tokens):
    """Drop only the entries that point at a removed gateway model."""
    if isinstance(value, dict):
        if "modelId" in value and references(value.get("modelId"), tokens):
            return None
        out = {}
        for key, item in value.items():
            if isinstance(item, (str, int, float, bool)) and references(key, tokens):
                continue
            kept = prune_selection(item, tokens)
            if kept is not None:
                out[key] = kept
        return out or None
    if isinstance(value, list):
        return [item for item in value if not references(item, tokens)]
    return value


def run(path, hosts):
    removed = 0
    changed_keys = []
    with closing(sqlite3.connect(path, timeout=5)) as con:
        con.execute("PRAGMA busy_timeout = 5000")
        rows = con.execute("SELECT key, value FROM ItemTable").fetchall()
        updates = []
        tokens = set()
        for key, raw in rows:
            if not key or not key.endswith(MAP_SUFFIX):
                continue
            try:
                doc = json.loads(raw or "{}")
            except (ValueError, TypeError):
                continue
            if not isinstance(doc, dict):
                continue
            touched = False
            for channel, entries in list(doc.items()):
                if not isinstance(entries, list):
                    continue
                kept = []
                for entry in entries:
                    if is_gateway(entry, hosts):
                        tokens |= model_tokens(entry)
                        removed += 1
                        touched = True
                    else:
                        kept.append(entry)
                if kept:
                    doc[channel] = kept
                else:
                    doc.pop(channel, None)
            if touched:
                updates.append((json.dumps(doc, ensure_ascii=False, separators=(",", ":")), key))
                changed_keys.append(key)
        if tokens:
            for key, raw in rows:
                if not key or not key.endswith(SELECT_SUFFIXES):
                    continue
                try:
                    doc = json.loads(raw or "{}")
                except (ValueError, TypeError):
                    continue
                pruned = prune_selection(doc, tokens)
                if pruned is None:
                    pruned = {}
                if pruned != doc:
                    updates.append((json.dumps(pruned, ensure_ascii=False, separators=(",", ":")), key))
                    changed_keys.append(key)
        if updates:
            con.executemany("UPDATE ItemTable SET value = ? WHERE key = ?", updates)
            con.commit()
    return {"removed": removed, "keys": changed_keys}


if __name__ == "__main__":
    request = json.loads(sys.stdin.read() or "{}")
    try:
        result = run(request["path"], request.get("hosts") or ["127.0.0.1:8788", "localhost:8788"])
        print(json.dumps(result, ensure_ascii=False))
    except Exception as error:
        print(json.dumps({"error": str(error)}))
        sys.exit(1)
