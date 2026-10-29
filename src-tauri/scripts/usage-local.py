"""Read-only local usage adapters. No credentials or conversation text leave this process.

Cache files contain only record identifiers, dates, model names and numeric counters.
Actual provider totals and visible-text estimates always remain separate.
"""
import datetime as dt
from contextlib import closing
import hashlib
import json
import math
import os
from pathlib import Path
import sqlite3
import shutil
import subprocess
import sys

CACHE_VERSION = 8
METRICS = ("tokens", "exact_tokens", "estimated_tokens", "input_tokens", "output_tokens",
           "cached_tokens", "requests", "failures", "timed_output_tokens", "duration_ms",
           "timing_samples", "ttft_ms", "ttft_samples")


def number(value):
    try:
        n = float(value)
        return n if math.isfinite(n) and n >= 0 else 0.0
    except (ValueError, TypeError):
        return 0.0


def first_number(obj, *keys):
    for key in keys:
        if key in obj and obj[key] is not None:
            return number(obj[key])
    return 0.0


def local_day(value):
    try:
        if isinstance(value, (float, int)):
            value = value / 1000 if value > 10**11 else value
            return dt.datetime.fromtimestamp(value).date().isoformat()
        text = str(value or "")
        if len(text) == 10:
            return dt.date.fromisoformat(text).isoformat()
        return dt.datetime.fromisoformat(text.replace("Z", "+00:00")).astimezone().date().isoformat()
    except (ValueError, TypeError, OverflowError, OSError):
        return ""


def millis(value):
    try:
        if isinstance(value, (float, int)):
            return value if value > 10**11 else value * 1000
        return dt.datetime.fromisoformat(str(value).replace("Z", "+00:00")).timestamp() * 1000
    except (ValueError, TypeError, OverflowError, OSError):
        return None


def precise_timestamp(value):
    """Seconds since epoch only if the record actually specifies a time of day.

    A date-only billing row is not a midnight event and must not create a fake
    hourly spike. Invalid, non-finite and non-positive times are unavailable.
    """
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, str):
        text = value.strip()
        if len(text) <= 10 or not ("T" in text or " " in text):
            return None
    try:
        ms = millis(value)
        if ms is None or not math.isfinite(ms) or ms <= 0:
            return None
        return ms / 1000
    except (TypeError, ValueError, OverflowError):
        return None


def visible_text(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(part for x in value if (part := visible_text(x)))
    if isinstance(value, dict):
        # Deliberately exclude image/base64, reasoning signatures and serialized tool payloads.
        if value.get("type") in ("text", "input_text", "output_text"):
            return str(value.get("text") or "")
        if value.get("type") == "tool_result":
            return visible_text(value.get("content"))
    return ""


def estimate(text):
    # A disclosed character heuristic, not a tokenizer or reconstructed billing record.
    cjk = sum(1 for ch in text if "\u2e80" <= ch <= "\u9fff" or "\uac00" <= ch <= "\ud7af"
              or "\uf900" <= ch <= "\ufaff" or ord(ch) >= 0x20000)
    return math.ceil(cjk + (len(text) - cjk) / 4) if text else 0


def usage_tokens(usage, style="standard"):
    if not isinstance(usage, dict):
        return (0, 0, 0, 0)
    i = first_number(usage, "input_tokens", "prompt_tokens", "inputTokens", "input")
    o = first_number(usage, "output_tokens", "completion_tokens", "outputTokens", "output")
    cache = usage.get("cache") if isinstance(usage.get("cache"), dict) else {}
    read = first_number(usage, "cached_input_tokens", "cache_read_input_tokens", "cacheReadTokens", "cacheRead")
    write = first_number(usage, "cache_creation_input_tokens", "cacheWriteTokens", "cacheWrite")
    read = read or number(cache.get("read"))
    write = write or number(cache.get("write"))
    details = usage.get("prompt_tokens_details") or usage.get("inputTokensDetails") or {}
    if isinstance(details, dict):
        read = read or first_number(details, "cached_tokens", "cacheReadTokens")
    total = first_number(usage, "total_tokens", "totalTokens", "total")
    if total <= 0:
        # Anthropic/Claude and OpenCode split cache out; OpenAI and Codex include it in input.
        exclusive = style in ("claude", "opencode", "minimax")
        total = i + o + (read + write if exclusive else 0)
        # OpenCode's reasoning is a separate counter from output. The new
        # session_message table no longer stores the old table's total field.
        if style == "opencode":
            total += number(usage.get("reasoning"))
    return (i, o, read + write, total)


def event(identity, session, timestamp, model="", provider="", usage=None, style="standard",
          estimated=0, input_estimate=0, output_estimate=0, requests=1, failure=False,
          duration=0, ttft=0):
    i, o, cache, total = usage_tokens(usage, style)
    exact = total
    if total <= 0 and estimated:
        total, i, o = estimated, input_estimate, output_estimate
    row = dict.fromkeys(METRICS, 0.0)
    row.update(id=hashlib.sha256(str(identity).encode()).hexdigest(), session=str(session), date=local_day(timestamp),
               timestamp=precise_timestamp(timestamp),
               model=str(model or "未记录模型"), provider=str(provider or "未记录供应商"),
               tokens=total, exact_tokens=exact, estimated_tokens=total if not exact else 0,
               input_tokens=i, output_tokens=o, cached_tokens=cache,
               requests=requests, failures=int(failure))
    if exact > 0 and o > 0 and number(duration) > 0:
        row.update(timed_output_tokens=o, duration_ms=number(duration), timing_samples=1)
    if number(ttft) > 0:
        row.update(ttft_ms=number(ttft), ttft_samples=1)
    return row


def json_lines(path):
    try:
        with path.open(encoding="utf-8-sig", errors="replace") as handle:
            for line in handle:
                try:
                    item = json.loads(line)
                    if isinstance(item, dict):
                        yield item
                except (ValueError, TypeError):
                    continue
    except OSError:
        return


def codex_file(path):
    records = []
    session = path.stem
    previous = None
    model = ""
    for index, item in enumerate(json_lines(path)):
        payload = item.get("payload") or {}
        if not isinstance(payload, dict):
            continue
        if item.get("type") == "session_meta":
            session = payload.get("id") or session
        if item.get("type") == "turn_context":
            model = payload.get("model") or model
        if payload.get("type") != "token_count":
            continue
        info = payload.get("info") or {}
        current = info.get("total_token_usage")
        last = info.get("last_token_usage")
        current_tokens = usage_tokens(current, "codex") if isinstance(current, dict) else None
        if current_tokens is not None and current_tokens == previous:
            # Quota/rate-limit refreshes repeat the last usage without another model call.
            continue
        if current_tokens is not None and previous is not None and current_tokens[3] >= previous[3]:
            values = [max(0, a - b) for a, b in zip(current_tokens, previous)]
            increment = dict(zip(("input_tokens", "output_tokens", "cached_input_tokens", "total_tokens"), values))
        elif isinstance(last, dict) and usage_tokens(last)[3] > 0:
            increment = last
        elif current_tokens is not None:
            increment = current
        else:
            continue
        previous = current_tokens if current_tokens is not None else previous
        if usage_tokens(increment)[3] <= 0:
            continue
        identity = "%s:%s:%s" % (session, item.get("timestamp", index), json.dumps(current or increment, sort_keys=True))
        records.append(event(identity, session, item.get("timestamp"), model, usage=increment))
    return records


def message_file(path, app):
    groups = {}
    session = path.stem if path.name != "messages.jsonl" else path.parent.name
    model = ""
    pending_input = 0
    is_wb = app in ("workbuddy", "workbuddy-intl", "codebuddy", "codebuddy-cn")
    for index, item in enumerate(json_lines(path)):
        session = item.get("sessionId") or item.get("session_id") or session
        typ = item.get("type")
        message = item.get("message") if isinstance(item.get("message"), dict) else item
        provider = item.get("providerData") or {}
        if not isinstance(provider, dict):
            provider = {}
        if typ == "runtime-config":
            model = item.get("model") or model
            continue
        role = message.get("role") or item.get("role") or typ
        if role == "user":
            pending_input += estimate(visible_text(message.get("content")))
            continue
        raw_usage = message.get("usage") or provider.get("usage") or item.get("usage")
        if role not in ("assistant", "model") and not (is_wb and typ in ("function_call", "model-usage")):
            continue
        if is_wb:
            message_id = provider.get("messageId") or item.get("id") or index
        else:
            message_id = message.get("id") or message.get("responseId") or item.get("message_id") or item.get("uuid") or item.get("id") or index
        key = str(session) + ":" + str(message_id)
        group = groups.setdefault(key, {"input_estimate": pending_input, "fragments": {}, "usage": {},
                                       "timestamp": item.get("timestamp") or message.get("timestamp"),
                                       "model": message.get("model") or provider.get("model") or provider.get("requestModelId") or model,
                                       "provider": message.get("provider") or ""})
        pending_input = 0
        text = visible_text(message.get("content") or item.get("content"))
        fragment_key = item.get("uuid") or item.get("id") or hashlib.sha256(text.encode()).hexdigest()
        group["fragments"][str(fragment_key)] = estimate(text)
        style = "standard" if is_wb else "minimax" if app == "minimax" else "claude"
        if usage_tokens(raw_usage, style)[3] >= usage_tokens(group["usage"], style)[3]:
            group["usage"] = raw_usage
        group["style"] = style
    records = []
    for key, group in groups.items():
        out = sum(group["fragments"].values())
        inp = group["input_estimate"]
        records.append(event(key, key.rsplit(":", 1)[0], group["timestamp"], group["model"], group["provider"],
                             group["usage"], group["style"], estimated=inp + out,
                             input_estimate=inp, output_estimate=out))
    return records


def connect(path):
    connection = sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=2)
    connection.row_factory = sqlite3.Row
    connection.execute("PRAGMA query_only=ON")
    return connection


def sql_rows(connection, table, optional=False):
    # Only constant adapter-owned identifiers are accepted by callers.
    try:
        yield from connection.execute('SELECT * FROM "' + table + '"')
    except sqlite3.Error:
        if not optional:
            raise


def zcode_db(path):
    records = []
    with closing(connect(path)) as con:
        for row in sql_rows(con, "model_usage"):
            row = dict(row)
            total = first_number(row, "provider_total_tokens", "computed_total_tokens")
            if total <= 0:
                total = first_number(row, "computed_total_tokens")
            u = dict(row)
            if total > 0:
                u["total_tokens"] = total
            records.append(event(row.get("id"), row.get("session_id"), row.get("started_at"),
                                 row.get("model_id"), row.get("provider_id"), u,
                                 failure=row.get("status") in ("error", "failed"),
                                 duration=row.get("duration_ms"), ttft=row.get("time_to_first_token_ms")))
    return records


def opencode_db(path):
    records = {}
    with closing(connect(path)) as con:
        # The new table supersedes migrated IDs from the old table.
        for table in ("message", "session_message"):
            for raw in sql_rows(con, table, optional=True):
                row = dict(raw)
                try:
                    data = json.loads(row.get("data") or "{}")
                except (TypeError, ValueError):
                    continue
                if (row.get("type") or data.get("role")) != "assistant":
                    continue
                model = data.get("model") or {}
                if not isinstance(model, dict):
                    model = {"id": model}
                timing = data.get("time") or {}
                start = number(timing.get("created"))
                end = number(timing.get("completed"))
                first = number(timing.get("streamed"))
                records[str(row.get("id"))] = event(row.get("id"), row.get("session_id"), row.get("time_created"),
                    model.get("id") or data.get("modelID"), model.get("providerID") or data.get("providerID"),
                    data.get("tokens"), "opencode", failure=bool(data.get("error")),
                    duration=end - start if end > start > 0 else 0,
                    ttft=first - start if first > start > 0 else 0)
    return list(records.values())


def marvis_db(path):
    records = []
    with closing(connect(path)) as con:
        for index, raw in enumerate(sql_rows(con, "llm_token_usage")):
            row = dict(raw)
            records.append(event(row.get("id", index), row.get("conversation_id"), row.get("usage_date"),
                                 row.get("model_id") or row.get("model") or row.get("model_name"),
                                 usage={"total_tokens": row.get("total_tokens"),
                                        "input_tokens": row.get("input_tokens"),
                                        "output_tokens": row.get("output_tokens"),
                                        "cached_input_tokens": row.get("cached_tokens")}))
    return records


def dsh_file(path):
    raw = path.read_bytes()
    try:
        from compression import zstd
        raw = zstd.decompress(raw)
    except ImportError:
        try:
            import zstandard
            raw = zstandard.ZstdDecompressor().decompress(raw, max_output_size=128 * 1024 * 1024)
        except ImportError:
            executable = shutil.which("zstd")
            if not executable:
                raise ValueError("zstd unavailable")
            flags = {"creationflags": 0x08000000} if os.name == "nt" else {}
            result = subprocess.run([executable, "-dc"], input=raw, stdout=subprocess.PIPE,
                                    stderr=subprocess.DEVNULL, timeout=20, **flags)
            if result.returncode:
                raise ValueError("zstd failed")
            raw = result.stdout
    records = []
    for index, line in enumerate(raw.decode("utf-8", "replace").splitlines()):
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if not isinstance(item, dict) or item.get("type") != "assistant/message":
            continue
        data = item.get("data") or {}
        if not isinstance(data, dict) or not isinstance(data.get("usage"), dict):
            continue
        records.append(event(data.get("id") or item.get("id") or index, path.parent.name,
                             item.get("time"), data.get("model"), usage=data["usage"]))
    return records


def gemini_file(path):
    records = []
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return records
    pending_input = 0
    session = data.get("sessionId") or path.stem
    for index, row in enumerate(data.get("messages") or []):
        if row.get("type") == "user":
            pending_input += estimate(visible_text(row.get("content")))
        elif row.get("type") in ("gemini", "model", "assistant"):
            tokens = row.get("tokens") or {}
            u = {"input_tokens": tokens.get("input"), "output_tokens": tokens.get("output"),
                 "total_tokens": tokens.get("total"), "cached_input_tokens": tokens.get("cached")}
            out = estimate(visible_text(row.get("content")))
            records.append(event(row.get("id", index), session, row.get("timestamp"), row.get("model"),
                                 "google", u, estimated=pending_input + out, input_estimate=pending_input,
                                 output_estimate=out))
            pending_input = 0
    return records


def paths_for(app, root):
    if app in ("codex", "chatgpt"):
        return [(p, "codex") for folder in ("sessions", "archived_sessions") for p in (root / folder).glob("**/*.jsonl")]
    if app in ("claude", "claude-code", "qoder", "qoder-cn", "qoderwork", "qwenwork", "qwenworkcn",
               "workbuddy", "workbuddy-intl", "codebuddy", "codebuddy-cn"):
        return [(p, "messages") for p in (root / "projects").glob("**/*.jsonl")]
    if app == "minimax":
        return [(p, "messages") for p in (root / "v2" / "sessions").glob("**/messages.jsonl")]
    if app == "zcode":
        return [(root / "cli" / "db" / "db.sqlite", "zcode")]
    if app == "opencode":
        candidates = [root / "opencode.db", root / "data" / "opencode.db"]
        return [(p, "opencode") for p in candidates if p.is_file()][:1]
    if app == "marvis":
        return [(root / "database" / "data.db", "marvis")]
    if app == "dsh":
        return [(p, "dsh") for p in (root / "sessions").glob("**/session.v4.jsonl.zstd")]
    if app in ("gemini", "gemini-cli"):
        return [(p, "gemini") for p in (root / "tmp").glob("**/chats/session-*.json")]
    return []


def fingerprint(path):
    values = []
    for p in (path, Path(str(path) + "-wal")):
        try:
            stat = p.stat()
            values.extend([stat.st_mtime_ns, stat.st_size])
        except OSError:
            values.extend([0, 0])
    return values


def blank_bucket():
    bucket = dict.fromkeys(METRICS, 0.0)
    bucket.update(_sessions=set(), models={})
    return bucket


def aggregate(records, source, source_key, files, errors=0):
    days = {}
    summary = blank_bucket()
    unique = {}
    for row in records:
        identity = str(row["session"]) + "|" + str(row["id"])
        old = unique.get(identity)
        if old is None or (row["exact_tokens"], row["tokens"]) >= (old["exact_tokens"], old["tokens"]):
            unique[identity] = row
    for row in unique.values():
        if row["tokens"] <= 0 and not row["requests"]:
            continue
        buckets = [summary]
        if row["date"]:
            buckets.append(days.setdefault(row["date"], blank_bucket()))
        for bucket in buckets:
            for field in METRICS:
                bucket[field] += row[field]
            bucket["_sessions"].add(row["session"])
            key = (row["model"], row["provider"])
            model = bucket["models"].setdefault(key, dict.fromkeys(METRICS, 0.0))
            for field in METRICS:
                model[field] += row[field]
    def finish(bucket):
        bucket["sessions"] = len(bucket.pop("_sessions"))
        bucket["models"] = [dict(model=k[0], provider=k[1], **v) for k, v in sorted(bucket["models"].items())]
        return bucket
    out = [dict(date=day, credits=bucket["tokens"], **finish(bucket)) for day, bucket in sorted(days.items())]
    finish(summary)
    if not unique:
        return None
    peak = max(out, key=lambda x: x["tokens"]) if out else {"tokens": 0, "date": ""}
    has_estimate = summary["estimated_tokens"] > 0
    return dict(unit="tokens", days=out, total=summary["tokens"], peak=peak["tokens"], peak_date=peak["date"],
                source=source, source_key=source_key, files=files, **summary,
                coverage="本机该数据目录中可读的模型记录；自定义接口仅在客户端保存记录时计入。" +
                ("部分记录没有 usage，按可见输入输出文本估算，不含隐藏提示、重复上下文和缓存。" if has_estimate else ""),
                estimate_method="CJK 字符约 1 token，其余字符约 4 字符/token；仅估算可见文本" if has_estimate else "",
                partial=errors > 0, unreadable_files=errors,
                undated_tokens=sum(r["tokens"] for r in unique.values() if not r["date"]),
                attribution="local-profile", duration_basis="request-wall-time",
                _records=[{k: v for k, v in r.items() if k not in METRICS or v != 0}
                          for r in unique.values()],
                requests_basis="模型用量记录数；日志事件未必与 HTTP 请求或用户轮次一一对应")


SOURCES = {"codex": "本机 Codex 会话日志", "claude": "本机 Claude Code 会话日志",
           "workbuddy": "本机 WorkBuddy 会话日志", "workbuddy-intl": "本机 WorkBuddy 国际版会话日志",
           "zcode": "本机 ZCode model_usage 请求库", "opencode": "本机 OpenCode 请求库",
           "minimax": "本机 MiniMax 会话日志", "marvis": "本机 Marvis 日用量库",
           "dsh": "本机 DSH 会话日志逐条 usage"}


def collect(app, root, cache_dir=None):
    if not root or not str(root).strip():
        return None
    root = Path(root).expanduser().resolve()
    if not root.is_dir():
        return None
    source_key = os.path.normcase(str(root))
    cache = {}
    cache_path = None
    if cache_dir:
        digest = hashlib.sha256((app + "|" + source_key).encode()).hexdigest()
        cache_path = Path(cache_dir) / (digest + ".json")
        try:
            cache = json.loads(cache_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            pass
    previous = cache.get("files", {}) if cache.get("version") == CACHE_VERSION else {}
    updated = {}
    records = []
    errors = 0
    files = 0
    for path, kind in sorted(paths_for(app, root)):
        if not path.is_file():
            continue
        key = str(path.relative_to(root))
        stamp = fingerprint(path)
        cached = previous.get(key, {})
        try:
            if cached.get("stamp") == stamp:
                rows = cached["records"]
            elif kind == "codex":
                rows = codex_file(path)
            elif kind == "messages":
                rows = message_file(path, app)
            elif kind == "zcode":
                rows = zcode_db(path)
            elif kind == "opencode":
                rows = opencode_db(path)
            elif kind == "marvis":
                rows = marvis_db(path)
            elif kind == "dsh":
                rows = dsh_file(path)
            else:
                rows = gemini_file(path)
            updated[key] = {"stamp": stamp, "records": rows}
            records.extend(rows)
            files += 1
        except (OSError, sqlite3.Error, ValueError, TypeError, KeyError, subprocess.TimeoutExpired):
            errors += 1
    if cache_path:
        try:
            cache_path.parent.mkdir(parents=True, exist_ok=True)
            temporary = cache_path.with_suffix(".%s.tmp" % os.getpid())
            temporary.write_text(json.dumps({"version": CACHE_VERSION, "files": updated}, ensure_ascii=False,
                                             separators=(",", ":")), encoding="utf-8")
            temporary.replace(cache_path)
        except OSError:
            pass
    return aggregate(records, SOURCES.get(app, "本机 " + app + " 会话日志"), source_key, files, errors)


if __name__ == "__main__":
    request = json.load(sys.stdin)
    try:
        result = collect(request.get("app_id", ""), request.get("data_dir", ""), request.get("cache_dir"))
        print(json.dumps(result, ensure_ascii=False, allow_nan=False))
    except Exception:
        # Do not expose source rows, paths or snippets in an error response.
        print("null")
