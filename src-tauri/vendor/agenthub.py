#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""AgentHub — 多 Agent 共享记忆 + 子 Agent 派工 + 角色协作（单文件零依赖）

每个能跑命令行的 Agent（Qoder/Codex/WorkBuddy/QwenWork/Claude Code/ZCode...）都可以：
  agenthub mem write "结论..."  --agent codex --tags rul,数据     把结论沉淀给所有 Agent
  agenthub mem search "关键词"  查其他 Agent 的记忆与工作记录
  agenthub mem context "查询"   生成可直接粘进提示词的记忆上下文块
  agenthub call codex "任务"    把任务派给另一个 Agent 无头执行，自动带上相关记忆
  agenthub call role:developer "任务"   按角色派工（候选里挑第一个可用的）
  agenthub call workbuddy "任务"        桌面端 Agent 走共享目录桥接（inbox/outbox）
  agenthub role list            角色 → Agent 候选（roles.json，可改）
  agenthub bridge ls            桥接收件箱/回执箱现状
  agenthub log                  查看派工记录

工作流引擎（Phase 2）：一份 YAML 描述 DAG，多 Agent 按波次并行协作。
  agenthub workflow new         生成工作流模板（YAML，含「测试 ⇄ 修复」闭环示例）
  agenthub workflow run 文件 --dry-run   先看执行计划（波次/团队/循环）再真跑
  agenthub workflow run 文件    真跑：并行派工 + 重试 + verdict 打回 + 断点续跑
  agenthub workflow status 项目 看每次运行各步骤的状态
  agenthub project new 名字     建项目目录并放一份工作流模板

递归派工：子 Agent 里可以继续 agenthub call 派给更下层。链路经环境变量 AGENTHUB_CHAIN
自动传递（默认最多 5 层，同一条链路里同一个 Agent 不出现两次，防循环）。

mem search / context / list 在查之前会自动增量同步各产品原生记忆（45s 节流），不用手动 sync。
索引放 ~/.agenthub/，与 AgentFind 互不干扰。
"""

import argparse
import base64
import contextlib
import datetime as dt
import hashlib
import io
import json
import os
import platform
import queue
import re
import shutil
import socket
import ssl
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from urllib.error import HTTPError, URLError
from urllib.parse import parse_qs, quote, urlencode, urlsplit
from urllib.request import (
    Request as UrlRequest, HTTPSHandler, build_opener, ProxyHandler)


def direct_open(req, timeout=15):
    """不走系统代理地打开请求；TLS 1.3 握手被本机拦截层吞掉时退回 TLS 1.2 再试一次。

    为什么要有这个退回：OpenSSL 3.4+ 默认在 ClientHello 里带后量子混合密钥share
    （X25519MLKEM768），包体变大之后本机的 TLS 拦截层（电脑管家 / 校园网关那一类）
    会直接把它丢掉，表现是「_ssl.c: handshake operation timed out」而不是握手报错，
    看上去像断网。实测系统 python（OpenSSL 3.0）不带那个扩展一直是好的，
    ~/.agenthub/venv 那个 python（OpenSSL 3.5）就握手超时。
    所以先按默认握手，只有确实卡在这种超时上才降一档，不做无条件降级。

    2026-11-05：退回本身是对的，但**每个 agenthub 调用都是新进程**，不记住就每次
    重交一次 15 秒学费。本机实测 www.workbuddy.ai 默认握手 15.11s 超时、TLS 1.2
    只要 0.27s；一轮 `agenthub checkin` 51 秒里有 45 秒是白等的，驾驶舱的
    「签到/读额度」因此慢到几分钟才反应。所以把"这个 host 会停等"落盘记住，
    下次直接走 1.2。
    """
    host = (urlsplit(req.full_url).hostname or "").lower()
    if host and host in _tls12_hosts():
        try:
            return _open_tls12(req, timeout)
        except URLError as e:
            # 记错了（这个 host 其实只认 1.3）：把记忆删掉再按默认握一次，
            # 否则一条错的记忆会让这个域名永久读不到。
            if not _handshake_stalled(e) and not isinstance(
                    getattr(e, "reason", None), ConnectionResetError):
                raise
            _forget_tls12(host)
            return _direct_opener().open(req, timeout=timeout)
    try:
        return _direct_opener().open(req, timeout=timeout)
    except URLError as e:
        reason = getattr(e, "reason", None)
        if _handshake_stalled(e):
            _remember_tls12(host)
            return _open_tls12(req, timeout)
        # 连接被中间层掐断（RemoteDisconnected / RST）时，只读请求再走 1.2 试一次：
        # 写请求重放会造成重复领取/重复发消息，宁可把错误原样报上去。
        if isinstance(reason, ConnectionResetError) and req.get_method() == "GET":
            return _open_tls12(req, timeout)
        raise


def _open_tls12(req, timeout=15):
    ctx = ssl.create_default_context()
    ctx.maximum_version = ssl.TLSVersion.TLSv1_2
    return build_opener(ProxyHandler({}), HTTPSHandler(context=ctx)).open(
        req, timeout=timeout)


# 哪些 host 在这台机器上"赌 TLS 1.3 会白等"。每次调用都是新进程，所以必须落盘。
_TLS12_MEMO = None


def _tls12_memo_path():
    base = Path(os.environ.get("AGENTHUB_HOME") or (Path.home() / ".agenthub"))
    return base / "tls12-hosts.json"


def _tls12_hosts():
    global _TLS12_MEMO
    if _TLS12_MEMO is None:
        try:
            raw = json.loads(_tls12_memo_path().read_text(encoding="utf-8"))
            _TLS12_MEMO = {str(h).lower() for h in raw if str(h).strip()}
        except (OSError, ValueError, TypeError):
            _TLS12_MEMO = set()
    return _TLS12_MEMO


def _remember_tls12(host):
    """记下会卡默认握手的 host。写失败不算错误——只是下次再白等一次。"""
    if not host or host in _tls12_hosts():
        return
    _tls12_hosts().add(host)
    _save_tls12()


def _forget_tls12(host):
    if not host or host not in _tls12_hosts():
        return
    _tls12_hosts().discard(host)
    _save_tls12()


def _save_tls12():
    try:
        p = _tls12_memo_path()
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(sorted(_tls12_hosts()), ensure_ascii=False),
                       encoding="utf-8")
        tmp.replace(p)
    except OSError:
        pass


def _direct_opener():
    return build_opener(ProxyHandler({}))


def _handshake_stalled(err):
    """区分"握手被丢包"和真的网络不通/证书错：只有前者值得退回 1.2 再试。"""
    reason = getattr(err, "reason", None) or err
    if isinstance(reason, (ssl.SSLError, socket.timeout, TimeoutError)):
        return True
    return "handshake" in str(reason).lower()


HOME = Path.home()
# AGENTHUB_HOME：整体搬家（测试隔离 / 多套独立实例）。设了它，索引/记忆/桥接/角色全用它。
APP_DIR = Path(os.environ.get("AGENTHUB_HOME") or (HOME / ".agenthub"))
DB_PATH = APP_DIR / "memory.sqlite"
DISPATCH_DIR = APP_DIR / "dispatches"
SYNC_STAMP = APP_DIR / ".last_sync"
SYNC_MIN_INTERVAL = 45     # 秒。读路径节流：查的时候才同步，保证「查到时不滞后」
def _agentfind_base():
    """AgentFind 的地址。端口是服务自己挑的（首选 8790，撞上别的程序在听的端口就往后
    顺延），启动时写进 ~/.agentfind/running.port，这里跟着读——写死端口会在服务换口后
    把深链指到别人身上（2026-10-29 那次 8765 撞车就是这个形状）。"""
    try:
        port = int((HOME / ".agentfind" / "running.port").read_text(encoding="ascii").strip())
    except (OSError, ValueError):
        port = 8790
    return f"http://127.0.0.1:{port}" if 1024 <= port <= 65535 else "http://127.0.0.1:8790"


AGENTFIND = _agentfind_base()   # 本机 Agent 对话检索服务，用来把结论接回原始对话
DOWN_STAMP = APP_DIR / ".agentfind_down"
DOWN_COOLDOWN = 60         # 秒。服务探到没起就冷却，别每次查询都干等连接超时

TRIGRAM_MIN = 3          # fts5 trigram 分词器要求的最短子串长度
CHUNK_LIMIT = 5000       # 原生记忆入库分段上限（字符）
NOTE_CAP = 20000         # 单条记忆硬上限
DEFAULT_TIMEOUT = 900    # 子 Agent 默认超时（秒）

# ---- 分层记忆（2026-11-04）------------------------------------------------
# 参考 harness 的分层做法：作用域 scope + 置顶 pinned + 新旧覆盖 superseded +
# 已蒸馏 digested。目标是把「一个扁平池 + 检索 top-N」改成「分层装配 + 逐级覆盖」。
MEM_SCHEMA_VERSION = 1   # 改分层规则要 bump，否则老库不重算（同 AgentFind 的 TITLE_SCHEMA_VERSION）
SCOPES = ("global", "project", "session")
CONTEXT_TOTAL = 6000     # 上下文块总预算（字符），超了按层级从低到高砍
CONTEXT_L0 = 2400        # 全局硬规则
CONTEXT_L2 = 1600        # 项目上下文
CONTEXT_SNIPPET = 900    # 单条检索片段上限
STYLE_FILE = HOME / ".agents" / "style.md"   # 输出偏好层唯一事实源（kind=style）

# 内置清单只当"第一次运行时该同步哪些"的种子；生效的是 ~/.agenthub/sources.json，
# 接入新 Agent 改那个文件就行，不用碰代码（agenthub mem source add/scan --fix 会写它）。
BUILTIN_SOURCES = [
    ("qoder-cn",   ".qoder-cn/memory/**/*.md"),
    ("qoder-cn",   ".qoder-cn/memories/**/*.md"),
    ("qoder",      ".qoder/memory/**/*.md"),
    ("qoder",      ".qoder/projects/*/memory/*.md"),
    ("codex",      ".codex/memories/*.md"),
    ("codex",      ".codex/AGENTS.md"),
    ("workbuddy",  ".workbuddy/memory/*.md"),
    ("qwenworkcn", ".qwenworkcn/awareness/main/MEMORY.md"),
    ("qwenworkcn", ".qwenworkcn/awareness/main/memory/*.md"),
    ("claude",     ".claude/CLAUDE.md"),
]
SOURCES_FILE = APP_DIR / "sources.json"
_src_cache = (0, [])


def _seed_rows():
    return [{"product": p, "glob": g, "enabled": True} for p, g in BUILTIN_SOURCES]


def source_rows():
    """sources.json 的原始条目；文件不存在时先按内置清单落一份，让配置成为唯一事实源。"""
    if not SOURCES_FILE.exists():
        save_source_rows(_seed_rows())
    try:
        data = json.loads(SOURCES_FILE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return _seed_rows()
    rows = data.get("sources") if isinstance(data, dict) else None
    return rows if isinstance(rows, list) else _seed_rows()


def save_source_rows(rows):
    APP_DIR.mkdir(parents=True, exist_ok=True)
    SOURCES_FILE.write_text(
        json.dumps({"sources": rows}, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def load_sources():
    """[(产品名, 相对 home 的 glob)]，跳过 disabled；按 mtime 缓存，改完文件下次查询就生效。"""
    global _src_cache
    try:
        mt = SOURCES_FILE.stat().st_mtime_ns
    except OSError:
        mt = 0
    if mt and _src_cache[0] == mt:
        return _src_cache[1]
    out = []
    for r in source_rows():
        if not isinstance(r, dict) or r.get("enabled", True) is False:
            continue
        p = str(r.get("product") or "").strip()
        g = str(r.get("glob") or "").strip()
        if p and g:
            out.append((p, g))
    _src_cache = (mt, out)
    return out


def count_glob(pattern):
    """glob 现在能命中几个文件——加源的时候用来当场验证，别写个查不到的规则进去。"""
    return len(_glob_files(pattern))


# 自动发现新 Agent 的记忆目录：只认这几种形状，避免把 skills/文档/缓存当记忆。
# 前三个是"产品把记忆放在自己目录下一层"，中间三个是"按项目分文件夹"的嵌套形状。
MEM_SHAPES = (
    "memory/**/*.md", "memories/**/*.md", "awareness/**/*.md",
    "projects/*/memory/*.md", "projects/*/memories/*.md", "automations/*/memory.md",
)
MEM_FILES = ("MEMORY.md", "AGENTS.md", "CLAUDE.md", "USER.md", "IDENTITY.md", "SOUL.md")
MD_SUFFIXES = (".md", ".markdown", ".txt")
SCAN_BUDGET = 12.0         # 秒。宁可下次再报，也不让一次查询卡住终端


def _glob_files(pattern):
    root = HOME / pattern.split("/")[0]
    if not root.exists():
        return set()
    return {str(f) for f in HOME.glob(pattern) if f.is_file()}


def scan_sources():
    """扫 home 的点开头的目录 + %APPDATA%，返回还没进 sources.json 的候选记忆源。
    判重看实际命中的文件而不是 glob 字符串——否则 .codex/memories/**/*.md 这种
    "已有规则的超集"会被当成新源，加进去只是重复劳动。"""
    have = {g for _, g in load_sources()}
    covered = set()
    for _, g in load_sources():
        covered |= _glob_files(g)
    roots = [HOME]
    if os.environ.get("APPDATA"):
        roots.append(Path(os.environ["APPDATA"]))
    out, deadline = [], time.time() + SCAN_BUDGET
    for root in roots:
        if not root.is_dir():
            continue
        try:
            top = sorted(root.iterdir())
        except OSError:
            continue
        for d in top:
            if time.time() > deadline:
                break
            if not d.is_dir():
                continue
            if root == HOME and not d.name.startswith("."):
                continue          # home 下只认 .产品 目录，别把用户自己的文件夹当 Agent
            name = d.name.lstrip(".").strip().lower().replace(" ", "-")
            if not name or name in ("agents", "agenthub", "agentfind"):
                continue          # agents 是共享 skill 目录，后两个是工具自己的家
            try:
                rel = d.relative_to(HOME).as_posix()
            except ValueError:
                continue
            cands = [f"{rel}/{f}" for f in MEM_FILES if (d / f).is_file()]
            cands += [f"{rel}/{s}" for s in MEM_SHAPES if _glob_files(f"{rel}/{s}")]
            for g in cands:
                if g in have:
                    continue
                files = _glob_files(g)
                new = files - covered
                if not new:
                    continue
                covered |= files
                out.append({"product": name, "glob": g, "files": len(new)})
    return sorted(out, key=lambda r: (-r["files"], r["product"]))

SCHEMA = """
create table if not exists notes(
  id integer primary key autoincrement,
  ts text not null, day text not null,
  agent text not null default 'user',
  kind text not null default 'note',
  topic text default '', tags text default '',
  content text not null,
  src text default '', sig text default '',
  scope text default 'global', project text default '',
  mkey text default '', pinned integer default 0,
  superseded integer default 0, digested integer default 0);
create index if not exists notes_day on notes(day);
create index if not exists notes_src on notes(src);
create virtual table if not exists notes_fts using fts5(
  body, nid unindexed, tokenize='trigram');
create table if not exists files(path text primary key, sig text);
create table if not exists dispatches(
  id integer primary key autoincrement,
  ts text not null, product text not null, cwd text, prompt text,
  mem_query text, exit_code int, dur real, transcript text, output text,
  chain text default '', role text default '');
create table if not exists run_events(
  run_id text not null, seq integer not null, ts text not null,
  type text not null, step_id text default '', payload text default '',
  primary key(run_id, seq));
create index if not exists run_events_run on run_events(run_id, seq);
create table if not exists step_checkpoints(
  run_id text not null, step_id text not null, attempt integer not null,
  status text not null, input_hash text default '', output_path text default '',
  error text default '', started_at text default '', ended_at text default '',
  primary key(run_id, step_id, attempt));
create table if not exists run_budgets(
  run_id text primary key, token_limit integer default 0,
  cost_limit real default 0, time_limit_s real default 0,
  max_parallel integer default 0, max_depth integer default 0,
  used_tokens integer default 0, used_cost real default 0,
  started_at text default '', updated_at text default '');
create table if not exists approvals(
  id text primary key, run_id text not null, step_id text default '',
  kind text default '', request text default '', decision text default '',
  decided_by text default '', decided_at text default '',
  created_at text default '');
create table if not exists tickets(
  id text primary key,
  source text not null default 'cli',
  source_event_id text default '',
  requester text default '',
  title text not null,
  scope text default '',
  stage text not null default 'working_queued',
  mandate_id text default '',
  run_id text default '',
  project text default '',
  workspace text default '',
  priority integer default 0,
  attention text default '',
  attention_kind text default '',
  latest_report text default '',
  latest_report_at text default '',
  latest_input_at text default '',
  outcome text default '',
  created_at text not null,
  updated_at text not null,
  ended_at text default '');
create unique index if not exists tickets_event
  on tickets(source, source_event_id) where source_event_id != '';
create index if not exists tickets_stage on tickets(stage);
create index if not exists tickets_run on tickets(run_id);
create table if not exists mandates(
  id text primary key,
  ticket_id text not null,
  policy_version text not null,
  strength text not null default 'explicit',
  matched_rules text default '',
  actor text default '',
  scope text default '',
  created_at text not null);
create index if not exists mandates_ticket on mandates(ticket_id);
create table if not exists assignments(
  id text primary key,
  ticket_id text not null,
  executor text not null default '',
  status text not null default 'pending',
  attempt integer default 1,
  workspace text default '',
  run_id text default '',
  report_state text default '',
  report_text text default '',
  reported_at text default '',
  started_at text default '',
  ended_at text default '',
  error text default '');
create index if not exists assignments_ticket on assignments(ticket_id);
create table if not exists ticket_inputs(
  id integer primary key autoincrement,
  ticket_id text not null,
  kind text not null default 'note',
  body text not null,
  human integer default 1,
  read_at text default '',
  created_at text not null);
create index if not exists ticket_inputs_ticket on ticket_inputs(ticket_id);
create table if not exists effects(
  kind text not null,
  key text not null,
  payload text default '',
  seq integer not null,
  due real default 0,
  attempts integer default 0,
  error text default '',
  generation integer default 1,
  created_at text not null,
  updated_at text not null,
  primary key(kind, key));
create index if not exists effects_due on effects(kind, due, seq);
create table if not exists receipts(
  id text primary key,
  ticket_id text default '',
  channel text default '',
  status text default 'pending',
  attempts integer default 0,
  error text default '',
  payload text default '',
  created_at text not null,
  sent_at text default '',
  updated_at text not null);
create index if not exists receipts_ticket on receipts(ticket_id);
create table if not exists health(
  kind text primary key,
  state text default 'ready',
  detail text default '',
  updated_at text not null);
create table if not exists source_events(
  source text not null,
  event_id text not null,
  actor text default '',
  title text default '',
  body text default '',
  addressed integer default 1,
  disposition text default '',
  ticket_id text default '',
  reason text default '',
  created_at text not null,
  primary key(source, event_id));
create table if not exists assessments(
  id text primary key,
  ticket_id text default '',
  source text default '',
  event_id text default '',
  direction text default '',
  intent_evidence text default '',
  rule_assessments text default '',
  decision text default '',
  created_at text not null);
"""

NOTE_COLS = (
    ("scope", "text default 'global'"),
    ("project", "text default ''"),
    ("mkey", "text default ''"),
    ("pinned", "integer default 0"),
    ("superseded", "integer default 0"),
    ("digested", "integer default 0"),
)
NOTE_LAYER_COLS = {c for c, _ in NOTE_COLS}


def project_from_path(p):
    """记忆文件路径 → 项目名。只认 <产品>/projects/<项目>/... 这一种形态，认不出就空。

    qoder 的 `projects/*/memory/*.md` 天生带项目名；`.workbuddy/memory/MEMORY.md` 没有，
    就老老实实归到 global，不编造。"""
    parts = Path(p).parts
    for i, seg in enumerate(parts[:-1]):
        if seg == "projects":
            nxt = parts[i + 1]
            if nxt and nxt not in ("memory", "memories"):
                return nxt
    return ""


def _migrate(con):
    """老库补列：SCHEMA 只管新库，已存在的表靠这里补上后加的字段。"""
    cols = {r[1] for r in con.execute("pragma table_info(dispatches)")}
    for col in ("chain", "role"):
        if col not in cols:
            con.execute(f"alter table dispatches add column {col} text default ''")
    ncols = {r[1] for r in con.execute("pragma table_info(notes)")}
    for col, decl in NOTE_COLS:
        if col not in ncols:
            con.execute(f"alter table notes add column {col} {decl}")
    tcols = {r[1] for r in con.execute("pragma table_info(tickets)")}
    if tcols and "workspace" not in tcols:
        con.execute("alter table tickets add column workspace text default ''")
    con.execute("create index if not exists notes_scope on notes(scope, project)")
    con.execute("create index if not exists notes_key on notes(mkey)")
    ver = con.execute("pragma user_version").fetchone()[0]
    if ver < MEM_SCHEMA_VERSION:
        backfill_layers(con)
        con.execute(f"pragma user_version={MEM_SCHEMA_VERSION}")


def backfill_layers(con):
    """一次性回填：把带 projects/<名>/ 的原生记忆认领到项目层，其余留 global。

    只改 scope/project 两个新列，不动内容，跑几遍结果一样（幂等）。"""
    rows = con.execute(
        "select id, src from notes where kind='native' and src!='' "
        "and (project is null or project='')").fetchall()
    n = 0
    for r in rows:
        pr = project_from_path(r["src"])
        if pr:
            con.execute("update notes set project=?, scope='project' where id=?", (pr, r["id"]))
            n += 1
    return n


def now_iso():
    return dt.datetime.now().isoformat(timespec="seconds")


def day_of(ts=None):
    return (dt.datetime.now() if ts is None
            else dt.datetime.fromisoformat(ts)).strftime("%Y-%m-%d")


def connect():
    APP_DIR.mkdir(parents=True, exist_ok=True)
    con = sqlite3.connect(str(DB_PATH), timeout=60, isolation_level=None)
    con.row_factory = sqlite3.Row
    con.executescript(SCHEMA)
    _migrate(con)
    con.execute("pragma journal_mode=WAL")
    con.execute("pragma synchronous=NORMAL")
    return con


# ---------------------------------------------------------------- 检索层 ---

def split_terms(q):
    return [a or b for a, b in re.findall(r'"([^"]+)"|(\S+)', q or "") if (a or b)]


def loose_terms(term, size=TRIGRAM_MIN):
    """长中文串整串查不到时退化成 3 字块（逐字滑动，带重叠）。

    trigram 分词只认「整串原样子串」，而提示词里的查询往往是一整句
    「打包后前端没更新」——不退化成 3 字块就必然 0 命中。非中文不走这条路。
    滑动步长必须是 1：步长 2 会跳过「忆分层」这类偏移块，两字重叠的句子就凑不够共现块。"""
    if len(term) <= size + 2 or not re.search(r"[\u4e00-\u9fff]", term):
        return []
    return [term[i:i + size] for i in range(len(term) - size + 1)][:8]


def loose_cand(con, term, agent, kind, since_day, tag, conds):
    """3 字块各自查，要求至少两块共同命中（单块命中噪声太大）。"""
    chunks = loose_terms(term)
    if len(chunks) < 2:
        return {}
    total = {}
    for t in chunks:
        for nid, c in term_nids(con, t, agent, kind, since_day, tag, conds).items():
            total[nid] = total.get(nid, 0) + 1
    return {nid: c for nid, c in total.items() if c >= 2}


def fts_match(term):
    return '"' + term.replace('"', '""') + '"'


def tag_list(s):
    return [t.strip().lower() for t in (s or "").split(",") if t.strip()]


def term_nids(con, term, agent=None, kind=None, since_day=None, tag=None, conds=None):
    """单个关键词 → {nid: 命中数}。≥3 字走 trigram 索引，1–2 字退回子串。"""
    cond, params = [], []
    if agent:
        cond.append("n.agent = ?"); params.append(agent)
    if kind:
        cond.append("n.kind = ?"); params.append(kind)
    if since_day:
        cond.append("n.day >= ?"); params.append(since_day)
    tlist = tag_list(tag) if isinstance(tag, str) else (tag or [])
    if tlist:
        tc = [f"(','||replace(replace(lower(n.tags),' ',''),';',',')||',') like ?"
              for _ in tlist]
        cond.append("(" + " or ".join(tc) + ")")
        params.extend(f"%,{t},%" for t in tlist)
    for sql, vals in (conds or ()):
        cond.append(sql)
        params.extend(vals)
    where = (" and " + " and ".join(cond)) if cond else ""
    if len(term) >= TRIGRAM_MIN:
        sql = (f"select f.nid nid, count(*) c from notes_fts f "
               f"join notes n on n.id = f.nid where f.body match ?{where} group by f.nid")
        try:
            rows = con.execute(sql, (fts_match(term), *params)).fetchall()
        except sqlite3.OperationalError:
            rows = []
    else:
        sql = (f"select f.nid nid, count(*) c from notes_fts f "
               f"join notes n on n.id = f.nid "
               f"where instr(lower(f.body),?)>0{where} group by f.nid")
        rows = con.execute(sql, (term.lower(), *params)).fetchall()
    return {r["nid"]: r["c"] for r in rows}


def search_notes(con, q, agent=None, kind=None, since=None, limit=15, sort="relevance",
                 tag=None, scope=None, project=None, include_superseded=False,
                 include_digested=False):
    terms = split_terms(q)
    if not terms:
        return []
    since_day = since_to_day(since)
    conds = []
    if not include_superseded:
        conds.append(("n.superseded=0", ()))          # 已被新结论覆盖的旧条目默认不出
    if not include_digested:
        conds.append(("n.digested=0", ()))            # 已入摘要的原生切块默认不出，避免重复
    if scope:
        conds.append(("n.scope=?", (scope,)))
    if project:
        conds.append((f"((n.scope='project' and n.project=?) or n.scope='global')", (project,)))
    per_term = [term_nids(con, t, agent, kind, since_day, tag, conds) for t in terms]
    cand = set(per_term[0])
    for counts in per_term[1:]:
        cand &= set(counts)
    if not cand:
        # 整串没命中就退化，否则提示词里的一句自然语言几乎必然 0 命中：
        # 单句 → 拆 3 字块；多词 → 从最长的词开始，谁有命中就用谁（交集太严，一个词挂就整条空）
        tries = [terms[0]] if len(terms) == 1 else sorted(terms, key=len, reverse=True)
        for t in tries:
            single = term_nids(con, t, agent, kind, since_day, tag, conds) \
                or loose_cand(con, t, agent, kind, since_day, tag, conds)
            if single:
                cand, per_term = set(single), [single]
                break
    if not cand:
        return []
    marks = ",".join("?" * len(cand))
    meta = {r["id"]: r for r in con.execute(
        f"select id, pinned, kind, project from notes where id in ({marks})", tuple(cand))}

    def score(nid):
        base = sum(c[nid] for c in per_term)
        m = meta.get(nid)
        if not m:
            return base
        if m["kind"] == "style":
            base += 6
        if m["pinned"]:
            base += 4
        if m["kind"] != "native":
            base += 2                    # 结论优先于原文切块
        if project and m["project"] == project:
            base += 3
        return base

    scored = sorted(cand, key=lambda nid: -score(nid)) if sort == "relevance" \
        else sorted(cand, reverse=True)
    scored = scored[:max(1, min(limit, 100))]
    marks = ",".join("?" * len(scored))
    rows = con.execute(f"select * from notes where id in ({marks})", scored).fetchall()
    by_id = {r["id"]: r for r in rows}
    return [by_id[nid] for nid in scored if nid in by_id]


def since_to_day(since):
    if not since:
        return None
    m = re.fullmatch(r"(\d+)\s*d", since.strip(), re.I)
    if m:
        d = dt.date.today() - dt.timedelta(days=int(m.group(1)))
        return d.isoformat()
    return since.strip()


def fmt_note(r, with_body=True):
    marks = []
    if "pinned" in r.keys() and r["pinned"]:
        marks.append("置顶")
    if "kind" in r.keys() and r["kind"] == "digest":
        marks.append("蒸馏")
    if "superseded" in r.keys() and r["superseded"]:
        marks.append("已作废")
    if "digested" in r.keys() and r["digested"]:
        marks.append("已入摘要")
    head = (f"#{r['id']} [{r['agent']}/{r['kind']}] {r['day']}"
            + (f" · {r['topic']}" if r["topic"] else "")
            + (f" · @{r['project']}" if "project" in r.keys() and r["project"] else "")
            + (f" · #{','.join(t for t in r['tags'].split(',') if t)}" if r["tags"] else "")
            + (" · " + "/".join(marks) if marks else ""))
    body = r["content"] if with_body else ""
    return head + (("\n" + body) if body else "")


def detect_project(cwd=None):
    """当前该用哪个项目名：AGENTHUB_PROJECT > 最近的 .git 根目录名 > 空。

    不调子进程（git rev-parse 在 Windows 上要 100ms+），只往上找 .git 标记。
    找不到 .git 就返回空——宁可不分层，也不拿临时目录名编一个项目出来。"""
    env = (os.environ.get("AGENTHUB_PROJECT") or "").strip()
    if env:
        return env
    p = Path(cwd or os.getcwd()).resolve()
    for cur in (p, *p.parents):
        if (cur / ".git").exists():
            return cur.name
    return ""


def _layer_head(r):
    bits = [f"[{r['agent']}/{r['kind']}]"]
    if "project" in r.keys() and r["project"]:
        bits.append(f"@{r['project']}")
    if r["topic"]:
        bits.append(r["topic"])
    bits.append(r["day"])
    return " · ".join(bits)


def _clip(body, cap):
    body = (body or "").strip()
    return body if len(body) <= cap else body[:cap] + " …（截断）"


def pinned_rows(con, limit=20):
    """L0/L1：置顶层。style 排最前；其余按 mkey 字母序（规则天然稳定、可预测），
    没有 mkey 的按新到旧排在后面。"""
    return con.execute(
        "select * from notes where pinned=1 and superseded=0 "
        "order by case kind when 'style' then 0 else 1 end, (mkey='') asc, mkey asc, id desc "
        "limit ?", (limit,)).fetchall()


def project_rows(con, project, limit=12):
    """L2：同一项目的自写记忆（原生切块不算，那是原文不是结论）。置顶的排前面。"""
    if not project:
        return []
    return con.execute(
        "select * from notes where scope='project' and project=? and superseded=0 "
        "and kind in ('note','fact','decision','handoff','digest') "
        "order by pinned desc, id desc limit ?", (project, limit)).fetchall()


def build_context(con, query, n=5, agent=None, project=None, include_superseded=False,
                  total=CONTEXT_TOTAL):
    """分层装配上下文块：L0 全局硬规则 → L1 输出偏好 → L2 项目上下文 → L3 相关记忆。

    与旧的「一次检索取 top-N」的区别：L0/L1/L2 是**必注入**的，不依赖查询词能否命中；
    只有 L3 走检索。这样硬规则和格式偏好不会因为关键词没撞上而整段消失。
    层次由 scope 决定，pinned 只表示"这一层里必须带上"，不改变它属于哪一层。"""
    project = (project or "").strip()
    used, seen_ids, blocks = 0, set(), []

    def render(rows, cap, head_cap=CONTEXT_SNIPPET):
        nonlocal used
        out, took = [], 0
        skipped = 0
        for r in rows:
            if r["id"] in seen_ids:
                continue
            body = _clip(r["content"], head_cap)
            if out and used + took + len(body) > cap:
                skipped = len(rows) - len(out)
                break
            seen_ids.add(r["id"])
            took += len(body)
            out.append(f"### {_layer_head(r)}\n{body}")
        used += took
        if skipped:
            out.append(f"（还有 {skipped} 条，预算到顶已省略 —— 要就 agenthub mem search 精确查）")
        return out

    pin = pinned_rows(con)
    l1 = [r for r in pin if r["kind"] == "style"]
    # 没有当前项目时，项目层的置顶条目没地方落，兜到 L0 并标出它属于哪个项目，别静默丢掉
    l0 = [r for r in pin if r["kind"] != "style"
          and (r["scope"] == "global" or (r["scope"] == "project" and not project))]
    l2 = project_rows(con, project)

    def add(title, rows, cap):
        body = render(rows, cap)
        if body:
            blocks.append(title + "\n" + "\n\n".join(body))
        return bool(body)

    add("## L0 全局硬规则（必须遵守，优先级高于下面各层）", l0, CONTEXT_L0)
    add("## L1 输出偏好（只约束形式，不改变事实）", l1, max(600, CONTEXT_L0 // 3))
    add(f"## L2 项目上下文 @{project}（比 L0 更具体，冲突时以此为准）", l2, CONTEXT_L2)
    if query:
        l3 = search_notes(con, query, agent=agent, limit=max(n, 1),
                          include_superseded=include_superseded, project=project or None)
        add("## L3 相关记忆（按需参考，可能是别的 Agent 的历史结论）",
            l3, max(600, total - used))

    if not blocks:
        return ""
    head = ("以下摘自本机多 Agent 共享记忆库（AgentHub），按层级从泛到具体排列："
            "L0 全局硬规则 → L1 输出偏好 → L2 项目上下文 → L3 相关记忆。\n"
            "覆盖规则：越具体的层越优先（L2 可覆盖 L0 的事实判断）；L1 只管格式与语气，"
            "不改变任何事实；与当前任务直接冲突时，以当前任务为准并在回复里说明。")
    return head + "\n\n" + "\n\n".join(blocks)


# ---------------------------------------------------------------- 写入层 ---

def insert_note(con, agent, kind, topic, tags, content, src="", sig="",
                scope="", project="", mkey="", pinned=None, supersede_id=None):
    content = content.strip()[:NOTE_CAP]
    if not content:
        return None
    kind = (kind or "note").strip().lower()
    project = (project or "").strip()
    if kind == "style":
        scope, pinned = "global", 1          # 输出偏好是常驻层，永远置顶、永远全局
    if pinned is None:
        pinned = 1 if kind == "style" else 0
    scope = (scope or "").strip().lower() or ("project" if project else "global")
    if scope not in SCOPES:
        scope = "global"
    if scope == "global":
        project = ""
    ts = now_iso()
    cur = con.execute(
        "insert into notes(ts,day,agent,kind,topic,tags,content,src,sig,"
        "scope,project,mkey,pinned,superseded,digested) "
        "values(?,?,?,?,?,?,?,?,?,?,?,?,?,0,0)",
        (ts, day_of(), agent, kind, topic, tags, content, src, sig,
         scope, project, (mkey or "").strip(), int(pinned)))
    nid = cur.lastrowid
    con.execute("insert into notes_fts(body,nid) values(?,?)", (content, nid))
    if mkey:
        # 同一个 key 只留最新一条可见：旧条留档（可 --all 查），但不再参与注入
        con.execute("update notes set superseded=1 where mkey=? and id!=? and superseded=0",
                    (mkey, nid))
    if supersede_id:
        con.execute("update notes set superseded=1 where id=?", (supersede_id,))
    return nid


def drop_src(con, src):
    for r in con.execute("select id from notes where src=?", (src,)).fetchall():
        con.execute("delete from notes_fts where nid=?", (r["id"],))
    con.execute("delete from notes where src=?", (src,))


def chunk_text(text, limit=CHUNK_LIMIT):
    chunks, cur, size = [], [], 0
    for para in text.split("\n\n"):
        if cur and size + len(para) > limit:
            chunks.append("\n\n".join(cur)); cur, size = [], 0
        cur.append(para); size += len(para)
    if cur:
        chunks.append("\n\n".join(cur))
    return [c for c in chunks if c.strip()] or [""]


def run_sync(con):
    t0 = time.time()
    seen = {}
    for product, pattern in load_sources():
        root = HOME / pattern.split("/")[0]
        if not root.exists():
            continue
        for f in HOME.glob(pattern):
            if not f.is_file() or f.stat().st_size > 500_000:
                continue
            if f.suffix.lower() not in MD_SUFFIXES:
                continue      # glob 写宽了也不至于把 config.toml / json 当记忆搬进来
            p = str(f)
            sig = f"{int(f.stat().st_mtime)}:{f.stat().st_size}"
            seen[p] = (product, sig)
    changed = removed = 0
    for p, (product, sig) in sorted(seen.items()):
        row = con.execute("select sig from files where path=?", (p,)).fetchone()
        if row and row["sig"] == sig:
            continue
        try:
            text = Path(p).read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        drop_src(con, p)
        chunks = chunk_text(text)
        multi = len(chunks) > 1
        pr = project_from_path(p)
        for i, chunk in enumerate(chunks):
            topic = f"{Path(p).stem}#{i + 1}" if multi else Path(p).stem
            insert_note(con, product, "native", topic, "", chunk, src=p, sig=sig,
                        project=pr, scope="project" if pr else "global")
        con.execute("insert or replace into files(path,sig) values(?,?)", (p, sig))
        changed += 1
    for (p,) in con.execute("select path from files").fetchall():
        if p not in seen:
            drop_src(con, p)
            con.execute("delete from files where path=?", (p,))
            removed += 1
    total = con.execute("select count(*) c from notes where kind='native'").fetchone()["c"]
    return len(seen), changed, removed, total, time.time() - t0


def cmd_mem_sync(con, args):
    seen, changed, removed, total, dur = run_sync(con)
    touch_stamp()
    print(f"同步完成：扫描 {seen} 个原生记忆文件，更新 {changed}、清理 {removed}，"
          f"当前原生记忆 {total} 条（{dur:.1f}s）")


def _sync_now(con, quiet=False):
    seen, changed, removed, total, dur = run_sync(con)
    touch_stamp()
    if not quiet:
        print(f"  同步：扫描 {seen} 个文件，更新 {changed}、清理 {removed}，"
              f"原生记忆 {total} 条（{dur:.1f}s）")


def cmd_mem_source(con, args):
    act, rows = args.source_act, source_rows()

    if act == "list":
        for r in rows:
            g = str(r.get("glob") or "")
            on = r.get("enabled", True) is not False
            print(f"  {'✓' if on else '✗'} {str(r.get('product', '')):<13}{g:<48} 命中 {count_glob(g)}")
        print(f"\n配置文件：{SOURCES_FILE}（改完即生效，不用重装修改代码）")
        return

    if act == "add":
        g = args.glob.strip().strip('"')
        if any(str(r.get("glob")) == g for r in rows):
            print(f"这条规则已经在里了：{g}")
            return
        p = (args.product or g.split("/")[0].lstrip(".")) .strip().lower()
        rows.append({"product": p, "glob": g, "enabled": True})
        save_source_rows(rows)
        n = count_glob(g)
        print(f"已登记 {p} ← {g}（当前命中 {n} 个文件）")
        if n:
            _sync_now(con)
        else:
            print("  暂时一个都没命中：路径是相对 home 的 glob，等那个产品装好后跑 agenthub mem sync 即可。")
        return

    if act == "rm":
        t = args.target.strip().strip('"').lower()
        keep = [r for r in rows
                if str(r.get("glob", "")).lower() != t and str(r.get("product", "")).lower() != t]
        if len(keep) == len(rows):
            print(f"没找到这条规则：{t}（先看 agenthub mem source list）")
            return
        save_source_rows(keep)
        print(f"已删除 {len(rows) - len(keep)} 条规则，正在按新清单重算（该产品的原生记忆会一并清出共享库）")
        _sync_now(con)
        return

    cands = scan_sources()
    if not cands:
        print("没发现未接入的记忆目录。已接入的看 agenthub mem source list")
        return
    print(f"发现 {len(cands)} 条还没接入的原生记忆源：\n")
    for r in cands:
        print(f"  {r['product']:<14}{r['glob']:<50} {r['files']} 个文件")
    if args.source_act == "scan" and getattr(args, "fix", False):
        have = {str(r.get("glob")) for r in rows}
        add = [r for r in cands if r["glob"] not in have]
        for r in add:
            rows.append({"product": r["product"], "glob": r["glob"], "enabled": True})
        save_source_rows(rows)
        print(f"\n已自动接入 {len(add)} 条 → {SOURCES_FILE}")
        _sync_now(con)
    else:
        print("\n一次接入全部：agenthub mem source scan --fix")
        print("只接其中一条：  agenthub mem source add <产品名> \"<glob>\"")


def touch_stamp():
    try:
        SYNC_STAMP.touch()
    except OSError:
        pass


def ensure_fresh(con):
    """读之前顺手增量同步一次。

    挂在读路径而不是定时器上：需要新鲜的时刻就是有人查的时刻，这样只要查了就不可能滞后。
    同步失败一律吞掉——宁可查到稍旧的记忆，也不要把一次查询弄成报错。
    """
    try:
        if time.time() - SYNC_STAMP.stat().st_mtime < SYNC_MIN_INTERVAL:
            return
    except OSError:
        pass
    try:
        run_sync(con)
        touch_stamp()
    except Exception:
        pass


def agentfind_trail(query, limit=2):
    """问 AgentFind：这条结论出自哪场对话。服务没开就安静地什么都不加。

    Windows 上连一个没监听的 127.0.0.1 端口不会立刻被拒，会干等到超时，所以失败要冷却 60s，
    免得服务没起时每次查询都白等一秒。
    """
    try:
        if time.time() - DOWN_STAMP.stat().st_mtime < DOWN_COOLDOWN:
            return []
    except OSError:
        pass
    try:
        url = f"{AGENTFIND}/api/search?q={quote(query)}&limit={limit}"
        with direct_open(url, timeout=1.2) as r:
            res = (json.loads(r.read().decode("utf-8")).get("results") or [])[:limit]
    except Exception:
        try:
            DOWN_STAMP.touch()
        except OSError:
            pass
        return []
    return [f'  · {x["label"]} {x["day"]} · {str(x["title"])[:30]} '
            f'→ {AGENTFIND}/?sid={quote(x["sid"], safe="")}' for x in res]


# ---------------------------------------------------------------- 命令 ---

def cmd_mem_write(con, args):
    content = sys.stdin.read() if args.content in ("-", "") else args.content
    project = (getattr(args, "project", "") or "").strip()
    scope = (getattr(args, "scope", "") or "").strip().lower()
    key = (getattr(args, "key", "") or "").strip()
    if project and not scope:
        scope = "project"
    if not project and scope == "project":
        project = detect_project()
    if not project and not scope:
        # 在某个 git 仓库里写，默认就归到那个项目层；不在仓库里就老老实实进全局层
        auto = detect_project()
        if auto:
            project, scope = auto, "project"
    old = con.execute("select count(*) c from notes where mkey=? and superseded=0",
                      (key,)).fetchone()["c"] if key else 0
    nid = insert_note(con, args.agent, args.kind, args.topic, args.tags, content,
                      scope=scope, project=project, mkey=key,
                      pinned=True if getattr(args, "pin", False) else None,
                      supersede_id=getattr(args, "supersede", None))
    if nid is None:
        print("内容是空的，没写")
        return
    marks = []
    if scope:
        marks.append(f"层:{scope}" + (f"@{project}" if project else ""))
    if getattr(args, "pin", False) or args.kind == "style":
        marks.append("置顶")
    if key:
        marks.append(f"key={key}")
    print(f"已写入共享记忆 #{nid}（agent={args.agent} kind={args.kind}"
          + (f" {', '.join(marks)}" if marks else "") + "）")
    if old:
        print(f"  同 key 的 {old} 条旧结论已标为作废，注入时不再出现"
              f"（agenthub mem search --all 仍可查）")
    if getattr(args, "supersede", None):
        print(f"  #{args.supersede} 已标为作废")


def cmd_mem_pin(con, args):
    ids = args.id if isinstance(args.id, list) else [args.id]
    for i in ids:
        r = con.execute("select id, kind from notes where id=?", (i,)).fetchone()
        if not r:
            print(f"没有 #{i}")
            continue
        val = 0 if args.unpin else 1
        con.execute("update notes set pinned=? where id=?", (val, i))
        print(f"#{i} {'取消置顶' if args.unpin else '已置顶'}（kind={r['kind']}）")
    print("置顶条目会出现在每次 agenthub mem context 的 L0/L1 段，慎用——只放真正的硬规则和偏好。")


def cmd_mem_layers(con, args):
    """看七层各自有多少条、谁在置顶、哪些已被覆盖或蒸馏。"""
    q = lambda sql, *p: con.execute(sql, p).fetchall()

    def line(label, n, note=""):
        print(f"  {label:<22}{n:>6}   {note}")

    print("L1 输出偏好（kind=style，常驻注入）")
    st = q("select count(*) c from notes where kind='style' and superseded=0")[0]["c"]
    line("style", st, str(STYLE_FILE) if st else "还没有：agenthub mem style --write")
    print("\nL0 全局硬规则（置顶、非 style）")
    for r in q("select topic, count(*) c from notes where pinned=1 and kind!='style' "
               "and superseded=0 group by topic order by c desc limit 8"):
        line(r["topic"] or "(无主题)", r["c"])
    if not q("select 1 from notes where pinned=1 and kind!='style' limit 1"):
        print("  （空）用 agenthub mem pin <id> 把真正的规则顶上去")
    print("\nL2 项目上下文（scope=project）")
    rows = q("select project, count(*) c from notes where scope='project' and project!='' "
             "and superseded=0 and kind!='native' group by project order by c desc limit 10")
    for r in rows:
        line("@" + r["project"], r["c"], "自写结论")
    nat = q("select count(*) c from notes where scope='project' and kind='native'")[0]["c"]
    if nat:
        line("（原生切块带项目标记）", nat, "按产品自己的目录 slug 归组，只作归组不参与 L2 注入")
    if not rows and not nat:
        print("  （空）写的时候带 --project，或 AGENTHUB_PROJECT=名字")
    print("\nL3 相关记忆（原生原文 vs 自写结论）")
    line("自写结论", q("select count(*) c from notes where kind!='native' "
                       "and superseded=0")[0]["c"], "note/fact/decision/handoff/digest")
    line("原生镜像", q("select count(*) c from notes where kind='native'")[0]["c"],
         "各产品 md 的只读切块")
    print("\n生命周期")
    line("已作废 superseded", q("select count(*) c from notes where superseded=1")[0]["c"],
         "同 key 被新结论覆盖，默认不注入")
    line("已入摘要 digested", q("select count(*) c from notes where digested=1")[0]["c"],
         "agenthub mem digest --write 之后降噪")
    line("蒸馏条目 digest", q("select count(*) c from notes where kind='digest'")[0]["c"])


def cmd_mem_digest(con, args):
    """把一段时间的碎结论压成摘要：默认只打印，--write 才落库。

    落库时原条目标 digested=1（不删、仍可 search --all），所以是"降噪"不是"销毁"。"""
    days = int(getattr(args, "days", 7) or 7)
    since_day = (dt.date.today() - dt.timedelta(days=days)).isoformat()
    cond = ["kind!='native'", "kind!='digest'", "kind!='style'", "pinned=0",
            "superseded=0", "day>=?"]
    params = [since_day]
    project = (getattr(args, "project", "") or "").strip()
    if project:
        cond.append("project=?"); params.append(project)
    rows = con.execute(
        f"select * from notes where {' and '.join(cond)} order by id", params).fetchall()
    if not rows:
        print(f"最近 {days} 天没有可蒸馏的条目" + (f"（@{project}）" if project else ""))
        return
    buckets = {}
    for r in rows:
        buckets.setdefault((r["project"] or "", r["topic"] or "未分类"), []).append(r)
    print(f"最近 {days} 天可蒸馏 {len(rows)} 条，归成 {len(buckets)} 组：\n")
    lines, used_ids = [], []
    for (proj, topic), rs in sorted(buckets.items(), key=lambda kv: -len(kv[1])):
        head = f"### {('@' + proj + ' · ') if proj else ''}{topic}（{len(rs)} 条）"
        items = []
        for r in rs:
            one = " ".join(r["content"].strip().split())
            items.append(f"- [{r['agent']}/{r['kind']} {r['day']}] {one[:220]}")
            used_ids.append(r["id"])
        lines.append(head + "\n" + "\n".join(items))
        print(head)
        for it in items[-3:]:
            print("  " + it[:160])
        if len(items) > 3:
            print(f"  …另有 {len(items) - 3} 条")
    body = "\n\n".join(lines)
    if not getattr(args, "write", False):
        print(f"\n共 {len(body)} 字。落库：agenthub mem digest --days {days}"
              + (f" --project {project}" if project else "") + " --write")
        print("落库后这些原条目会标「已入摘要」，注入和检索默认跳过（--all 仍可查）。")
        return
    nid = insert_note(con, "agenthub", "digest", f"摘要 {since_day}~{day_of()}",
                      "摘要", body, scope="project" if project else "global",
                      project=project)
    marks = ",".join("?" * len(used_ids))
    con.execute(f"update notes set digested=1 where id in ({marks})", used_ids)
    print(f"\n已写入摘要 #{nid}（{len(body)} 字），{len(used_ids)} 条原条目标为「已入摘要」。")


def cmd_mem_style(con, args):
    """输出偏好层：单一事实源 ~/.agents/style.md，同时以 kind=style 常驻注入。"""
    content = sys.stdin.read() if args.content in ("-", "") else (args.content or "")
    if not content.strip() and not args.write:
        r = con.execute("select * from notes where kind='style' and superseded=0 "
                        "order by id desc limit 1").fetchone()
        if r:
            print(f"# 当前生效的输出偏好（#{r['id']} {r['day']}）\n")
            print(r["content"])
        else:
            print("还没有输出偏好层。写一份：")
            print('  agenthub mem style --write "中文回复；结论先行；表格对比优先；'
                  '申请文书 300–500 字并核对字符数"')
        print(f"\n事实源：{STYLE_FILE}")
        return
    if not content.strip():
        if STYLE_FILE.exists():
            content = STYLE_FILE.read_text(encoding="utf-8", errors="ignore")
        if not content.strip():
            print(f"{STYLE_FILE} 是空的，没东西可写")
            return
    STYLE_FILE.parent.mkdir(parents=True, exist_ok=True)
    STYLE_FILE.write_text(content.strip() + "\n", encoding="utf-8")
    nid = insert_note(con, "user", "style", "输出偏好", "格式,偏好", content, src=str(STYLE_FILE))
    print(f"已写入输出偏好 #{nid}（置顶、全局，每次 context 都会带），事实源 {STYLE_FILE}")
    print("  以后改这个文件再跑一次 `agenthub mem style --write` 即可；"
          "想作废旧版就加 --supersede <旧id>")


def cmd_mem_list(con, args):
    ensure_fresh(con)
    cond, params = ["1=1"], []
    if args.agent:
        cond.append("agent=?"); params.append(args.agent)
    if args.kind:
        cond.append("kind=?"); params.append(args.kind)
    for t in tag_list(getattr(args, "tag", None)):
        cond.append("(','||replace(replace(lower(tags),' ',''),';',',')||',') like ?")
        params.append(f"%,{t},%")
    rows = con.execute(
        f"select * from notes where {' and '.join(cond)} "
        f"order by id desc limit ?", (*params, args.n)).fetchall()
    if not rows:
        print("（还没有记忆。让各 Agent 用 agenthub mem write 写入，或跑 agenthub mem sync 同步原生记忆）")
        return
    for r in rows:
        head = fmt_note(r, with_body=False)
        first = r["content"].strip().splitlines()[0][:80] if r["content"].strip() else ""
        print(f"{head}\n    {first}")


def cmd_mem_read(con, args):
    r = con.execute("select * from notes where id=?", (args.id,)).fetchone()
    print(fmt_note(r) if r else f"没有 #{args.id} 这条记忆")


def cmd_mem_forget(con, args):
    r = con.execute("select id from notes where id=?", (args.id,)).fetchone()
    if not r:
        print(f"没有 #{args.id} 这条记忆"); return
    con.execute("delete from notes_fts where nid=?", (args.id,))
    con.execute("delete from notes where id=?", (args.id,))
    print(f"已删除 #{args.id}")


def cmd_mem_search(con, args):
    ensure_fresh(con)
    all_ = bool(getattr(args, "all", False))
    rows = search_notes(con, args.query, agent=args.agent, kind=args.kind,
                        since=args.since, limit=args.n, sort=args.sort,
                        tag=getattr(args, "tag", None),
                        scope=getattr(args, "scope", None),
                        project=(getattr(args, "project", "") or "").strip() or None,
                        include_superseded=all_, include_digested=all_)
    if not rows:
        print("没有命中。试试更短的关键词（1–2 字也可）——各产品的原生记忆在查之前已经自动同步进来了")
        return
    for r in rows:
        print("-" * 62)
        print(fmt_note(r, with_body=False))
        try:
            snip = con.execute(
                "select snippet(notes_fts,0,'»','«',' … ',64) s from notes_fts "
                "where nid=?", (r["id"],)).fetchone()["s"]
            print(f"    {snip[:400]}")
        except sqlite3.OperationalError:
            print("    " + r["content"][:200].replace("\n", " "))
    trail = agentfind_trail(args.query)
    if trail:
        print("-" * 62)
        print("过程原文（这场对话的完整记录，点开可跳回那个应用）:")
        print("\n".join(trail))


def cmd_mem_context(con, args):
    ensure_fresh(con)
    project = (getattr(args, "project", "") or "").strip()
    if project in ("", "auto"):
        project = detect_project()
    block = build_context(con, args.query, n=args.n, agent=args.agent, project=project,
                          include_superseded=bool(getattr(args, "all", False)),
                          total=int(getattr(args, "total", 0) or CONTEXT_TOTAL))
    print(block if block else f"（共享记忆里没有与「{args.query}」相关的内容）")
    if block:
        head = f"L0 硬规则 / L1 输出偏好 / L2 @{project} / L3 检索结果" if project \
            else "L0 硬规则 / L1 输出偏好 / L3 检索结果（当前目录没识别出项目，L2 空）"
        print(f"\n<!-- 分层：{head} -->")
        trail = agentfind_trail(args.query, limit=1)
        if trail:
            print(f"\n<!-- 过程原文：{trail[0].strip()}（这条是给人点的，粘进提示词时可删） -->")


def cmd_mem_stats(con, args):
    print("按来源：")
    for r in con.execute("select agent, count(*) c from notes group by agent order by c desc"):
        print(f"  {r['agent']:<12} {r['c']:>5}")
    print("按类型：")
    for r in con.execute("select kind, count(*) c from notes group by kind order by c desc"):
        print(f"  {r['kind']:<12} {r['c']:>5}")


# ---------------------------------------------------------------- 派工层 ---

AGENTS = {
    "codex": {
        "bin": "codex",
        "desc": "Codex CLI（OpenAI）— codex exec，支持 -C 工作目录、-s 沙箱",
        "example": 'agenthub call codex "把 README 里的错别字改掉" --cwd D:\\某项目 --write',
    },
    "claude": {
        "bin": "claude",
        "desc": "Claude Code CLI（Anthropic）— claude -p，纯问答默认够用",
        "example": 'agenthub call claude "审一下这段摘要的逻辑" --cwd D:\\某项目 --yolo',
    },
}
# 探测列表：装了但没接无头模式的产品，仅提示存在
EXTRA_PROBES = ["opencode", "qwen", "gemini", "grok", "cursor-agent", "droid", "kimi"]

# ------------------------------------------------------- 协作层（角色 / 桥接 / 链路）---

MAX_CHAIN_DEPTH = 5           # 递归派工最大层数：user → A → B → C…，防失控
CHAIN_ENV = "AGENTHUB_CHAIN"  # 派工链路随子进程环境传递；子 Agent 再派工时自动续上
BRIDGE_DIR = APP_DIR / "bridge"

# 桌面端产品没有无头 CLI，走共享目录桥接：任务投进 inbox/task_<id>.json+md，
# 产品侧的自动化/技能读任务执行，把回执写进 outbox/result_<id>.json。
# agenthub 绝不往这些产品自己的配置目录里写任何东西，桥接目录统一在 ~/.agenthub/bridge/。
BRIDGE_PRODUCTS = {
    "workbuddy":  {"name": "WorkBuddy",   "config": ".workbuddy"},
    "qoder-cn":   {"name": "Qoder CN",    "config": ".qoder-cn"},
    "qoder":      {"name": "Qoder 国际版", "config": ".qoder"},
    "trae-cn":    {"name": "TRAE CN",      "config": ".trae-cn"},
    "zcode":      {"name": "ZCode",       "config": ".zcode"},
    "qwenworkcn": {"name": "QwenWork CN", "config": ".qwenworkcn"},
    "marvis":     {"name": "Marvis",      "config": ".marvis"},
}

# 能力声明：只写本机验证过的事实（与 catalog 口径一致），进 ~/.agents/catalog.json。
# execution_mode: headless=CLI 无头可直接派工 / bridge=图形界面但可目录桥接
CAPS = {
    "codex": {"execution_mode": "headless", "can_write_files": True, "can_run_commands": True,
              "supports_subagent_dispatch": True,
              "strength_tags": ["代码生成", "重构", "终端命令", "测试"]},
    "claude": {"execution_mode": "headless", "can_write_files": True, "can_run_commands": True,
               "supports_subagent_dispatch": True,
               "strength_tags": ["长文写作", "方案评审", "代码审查", "调研"]},
    "workbuddy": {"execution_mode": "bridge", "can_write_files": True, "can_run_commands": False,
                  "supports_subagent_dispatch": False,
                  "strength_tags": ["日常助理", "文档撰写", "自动化任务"]},
}
for _b in BRIDGE_PRODUCTS:    # 其余桌面端给一份保守基线，宁可少写不错写
    CAPS.setdefault(_b, {"execution_mode": "bridge", "can_write_files": False,
                         "can_run_commands": False, "supports_subagent_dispatch": False,
                         "strength_tags": []})


def connector_of(product):
    """该 Agent 的接入方式（进 catalog 的 connector 字段）。"""
    if product in AGENTS:
        return {"type": "cli", "bin": AGENTS[product]["bin"]}
    if product in BRIDGE_PRODUCTS:
        return {"type": "bridge", "inbox": str(BRIDGE_DIR / product / "inbox"),
                "outbox": str(BRIDGE_DIR / product / "outbox")}
    return {"type": "none"}


def agent_available(product):
    """派工目标此刻是否可用：CLI 看命令在不在 PATH；桥接看产品装没装（配置目录存在）。"""
    if product in AGENTS:
        return bool(shutil.which(AGENTS[product]["bin"]))
    if product in BRIDGE_PRODUCTS:
        return (HOME / BRIDGE_PRODUCTS[product]["config"]).exists()
    return False


# ---- 角色系统（roles.json：角色 → Agent 候选，call role:角色 按序挑第一个可用的） ----
ROLES_FILE = APP_DIR / "roles.json"
DEFAULT_ROLES = {
    "developer":  {"agents": ["codex", "claude"], "note": "写代码、改代码、跑命令"},
    "researcher": {"agents": ["claude", "codex"], "note": "调研、搜集资料、对比方案"},
    "reviewer":   {"agents": ["claude", "codex"], "note": "评审代码/方案/文档"},
    "tester":     {"agents": ["codex", "claude"], "note": "写测试用例、验证修复"},
    "writer":     {"agents": ["claude", "codex"], "note": "写文档、润色"},
    "assistant":  {"agents": ["workbuddy", "codex", "claude"], "note": "日常事务、轻量问答"},
}


def role_rows():
    if not ROLES_FILE.exists():
        save_role_rows(DEFAULT_ROLES)
    try:
        data = json.loads(ROLES_FILE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {k: dict(v) for k, v in DEFAULT_ROLES.items()}
    r = data.get("roles")
    return r if isinstance(r, dict) else {k: dict(v) for k, v in DEFAULT_ROLES.items()}


def save_role_rows(roles):
    APP_DIR.mkdir(parents=True, exist_ok=True)
    ROLES_FILE.write_text(json.dumps({"roles": roles}, ensure_ascii=False, indent=2) + "\n",
                          encoding="utf-8")


def resolve_role(spec):
    """'role:developer' → (产品名, 角色名)。按候选顺序挑第一个当前可用的。"""
    name = spec.split(":", 1)[1].strip()
    r = role_rows().get(name)
    if not r:
        raise SystemExit(f"没有这个角色：{name}（agenthub role list 看已定义的角色）")
    tried = []
    for a in r.get("agents", []):
        tried.append(a)
        if agent_available(a):
            return a, name
    raise SystemExit(f"角色 {name} 的候选（{'、'.join(tried) or '空'}）目前都不可用")


def chain_from_env():
    return [a for a in os.environ.get(CHAIN_ENV, "").split(",") if a]


def resolve_exe(name):
    """把命令名解析成可直接启动的完整路径（Windows 下 npm shim 是 .cmd，裸名字起不来）。"""
    hits = shutil.which(name)
    if not hits:
        return None
    p = Path(hits)
    if p.suffix.lower() == ".cmd":
        exe = p.with_suffix(".exe")
        if exe.exists():
            return str(exe)
    return hits


def proxy_env():
    """子 Agent 继承不到终端里的代理设置（如 clash 7897/7890），探测本机常见代理端口并注入。"""
    env = dict(os.environ)
    if any(k in env for k in ("HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy")):
        return env, None
    for port in (7897, 7890, 10809, 1080):
        s = socket.socket()
        s.settimeout(0.2)
        try:
            s.connect(("127.0.0.1", port))
        except OSError:
            continue
        finally:
            s.close()
        env["HTTP_PROXY"] = env["HTTPS_PROXY"] = f"http://127.0.0.1:{port}"
        env["NO_PROXY"] = "localhost,127.0.0.1"
        return env, f"已注入本机代理 127.0.0.1:{port}"
    return env, None


def run_dispatch(argv, cwd, full_prompt, timeout, child_env):
    """跑子 Agent。Windows 下 .cmd shim 会派生孙进程握住输出管道，超时必须按进程树强杀。"""
    t0 = time.time()
    try:
        proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, cwd=str(cwd), env=child_env)
    except FileNotFoundError:
        raise SystemExit(f"无法启动：{argv[0]}")
    timed_out = False
    try:
        out_b, err_b = proc.communicate(input=full_prompt.encode("utf-8"),
                                        timeout=timeout)
        code = proc.returncode
    except subprocess.TimeoutExpired:
        timed_out = True
        subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                       capture_output=True)
        try:
            out_b, err_b = proc.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            out_b, err_b = proc.communicate()
        code = 124
    dur = time.time() - t0
    out = (out_b or b"").decode("utf-8", errors="replace")
    err = (err_b or b"").decode("utf-8", errors="replace")
    if timed_out:
        err += f"\n[agenthub] 超时（>{timeout}s），已强制终止整棵进程树"
    return out, err, code, dur


def build_cmd(product, prompt, cwd, write, yolo, model):
    """返回 (argv, 说明)。提示词统一走 stdin，避免 Windows 命令行长度/转义问题。"""
    exe = resolve_exe(AGENTS[product]["bin"])
    if not exe:
        raise SystemExit(f"本机没找到 {AGENTS[product]['bin']} 命令")
    if product == "codex":
        argv = [exe, "exec", "--skip-git-repo-check", "--color", "never",
                "-s", "workspace-write" if write else "read-only", "-C", str(cwd), "-"]
        if model:
            argv += ["-m", model]
        note = "沙箱=" + ("workspace-write" if write else "read-only")
    elif product == "claude":
        argv = [exe, "-p", "--output-format", "text"]
        if yolo:
            argv += ["--dangerously-skip-permissions"]
        if model:
            argv += ["--model", model]
        note = "权限=" + ("bypass(--yolo)" if yolo else "默认(工具调用会被拒，适合问答)")
    else:
        raise SystemExit(f"不支持的子 Agent：{product}（agenthub agents 看可用列表）")
    return argv, note


def build_envelope(prompt, mem_block, chain, product, role_name, contract=None):
    """给子 Agent 的任务信封：身份 + 派工链路 + 契约 + 共享记忆 + 任务本体。

    链路说明让子 Agent 知道自己在协作树里的位置、还能不能再向下派工；
    这也是递归派工能成立的前提——子 Agent（codex/claude 这类 CLI）看得懂 agenthub 用法。
    contract 是机器可校验的交接契约，和自然语言 task 同时给，不互相替代。
    """
    lines = ["<dispatch-context>",
             f"你是被主 Agent 派工的子 Agent（身份：{product}"
             + (f"，角色 {role_name}" if role_name else "") + "）。",
             f"派工链路：{' → '.join(chain)}"]
    if len(chain) < MAX_CHAIN_DEPTH:
        lines.append("如需帮助可继续向下派工：agenthub call <产品|role:角色> \"子任务\""
                     "（链路自动续记、防循环）。重要结论请写回共享记忆："
                     f"agenthub mem write \"结论\" --agent {product} --tags <逗号分隔标签>")
    else:
        lines.append(f"已达最大派工深度（{MAX_CHAIN_DEPTH} 层），请直接完成本任务，不要再向下派工。")
    lines.append("</dispatch-context>")
    head = "\n".join(lines)
    contract_block = ""
    if contract:
        contract_block = ("<contract>\n"
                          + json.dumps(contract, ensure_ascii=False, indent=2)
                          + "\n</contract>\n\n"
                          + "请按 contract.output_schema 返回；回执里给出 evidence "
                            "（跑过的命令或来源链接）和 artifacts（产出文件路径）。\n\n")
    body = f"<shared-memory>\n{mem_block}\n</shared-memory>\n\n" if mem_block else ""
    return f"{head}\n\n{contract_block}{body}<task>\n{prompt}\n</task>"


def write_transcript(product, cwd, note, mem_query, mem_block, prompt, code, dur, out, err,
                     chain, role_name, extra=""):
    DISPATCH_DIR.mkdir(parents=True, exist_ok=True)
    stamp = dt.datetime.now().strftime("%Y-%m-%d_%H%M%S")
    tpath = DISPATCH_DIR / f"{stamp}-{product}.md"
    tpath.write_text(
        f"# agenthub 派工 · {product}"
        + (f"（角色 {role_name}）" if role_name else "") + "\n\n"
        f"- 时间: {now_iso()}\n- cwd: {cwd}\n- 沙箱/权限: {note}\n- 记忆查询: {mem_query or '-'}\n"
        f"- 派工链路: {' → '.join(chain) if chain else '（顶层）'}\n- 退出码: {code} · 用时 {dur:.0f}s\n"
        + (extra + "\n" if extra else "")
        + f"\n## 任务\n\n{prompt}\n\n## 共享记忆上下文\n\n{mem_block or '（未使用）'}\n\n"
        f"## stdout\n\n```\n{out.strip() or '（空）'}\n```\n\n"
        f"## stderr\n\n```\n{err.strip()[-3000:] or '（空）'}\n```\n",
        encoding="utf-8")
    return tpath


def record_dispatch(con, product, cwd, prompt, mem_query, code, dur, tpath, out,
                    chain, role_name):
    con.execute(
        "insert into dispatches(ts,product,cwd,prompt,mem_query,exit_code,dur,"
        "transcript,output,chain,role) values(?,?,?,?,?,?,?,?,?,?,?)",
        (now_iso(), product, str(cwd), prompt, mem_query or "", code, dur,
         str(tpath), out.strip()[:8000], ",".join(chain), role_name or ""))


def cli_dispatch(args, product, cwd, full_prompt, chain, role_name):
    argv, note = build_cmd(product, full_prompt, cwd, args.write, args.yolo, args.model)
    child_env, proxy_note = (dict(os.environ), None) if args.no_proxy else proxy_env()
    if proxy_note:
        note += f" · {proxy_note}"
    child_env[CHAIN_ENV] = ",".join(chain)   # 子 Agent 再派工时链路自动续上
    print(f"[agenthub] 派工 → {product}（{note}）cwd={cwd}"
          + (f" 角色={role_name}" if role_name else "")
          + (f" 链路={' → '.join(chain)}" if len(chain) > 1 else "")
          + (f" 记忆查询=「{args.mem}」" if args.mem else ""))
    out, err, code, dur = run_dispatch(argv, cwd, full_prompt, args.timeout, child_env)
    return out, err, code, dur, note, ""


def bridge_write_readme(product):
    """每个桥接产品的目录里放一份说明：信封/回执长什么样、产品侧怎么接。首次用到时生成。"""
    bdir = BRIDGE_DIR / product
    if (bdir / "README.md").exists():
        return
    bdir.mkdir(parents=True, exist_ok=True)
    name = BRIDGE_PRODUCTS[product]["name"]
    (bdir / "README.md").write_text(
        f"# {name} 桥接说明\n\n"
        f"agenthub 通过这个目录与 {name} 协作（agenthub 不碰 {name} 自己的任何文件）：\n\n"
        f"- `inbox/task_<id>.json` — 任务信封：id / from / to / role / chain / cwd / prompt_file / contract\n"
        f"- `inbox/task_<id>.md`  — 完整任务提示词（含契约、共享记忆上下文与派工链路）\n"
        f"- `outbox/result_<id>.json` — {name} 干完后写的回执：\n\n"
        "```json\n"
        "{\"task_id\": \"<id>\", \"status\": \"done 或 failed\",\n"
        " \"summary\": \"一句话结果\", \"output\": \"详细输出（可空）\",\n"
        " \"artifacts\": [\"产出文件的绝对路径\"],\n"
        " \"evidence\": [\"跑过的命令或来源链接\"],\n"
        " \"files_changed\": [\"改过的文件\"],\n"
        " \"blockers\": [\"没做完的点和原因\"]}\n"
        "```\n\n"
        f"信封里的 `contract` 是验收依据：按 `output_schema` 交东西，逐条对照 `acceptance`；\n"
        f"做不到的写进 `blockers`，不要用一句「已完成」带过。\n\n"
        f"## {name} 侧怎么接\n\n"
        f"在 {name} 里建一个自动化/技能，定期扫 inbox 里 status=pending 的任务：\n"
        f"读同名 .md 执行 → 写 outbox/result_<id>.json → 把任务 json 的 status 改成 done。\n"
        f"没有自动化能力时，人工看到收件箱里有任务，把 .md 内容粘给 {name} 执行也可以。\n",
        encoding="utf-8")


def bridge_dispatch(args, product, cwd, full_prompt, chain, role_name, contract=None):
    """桌面端桥接派工：可选人工审批 → 投递带租约的信封 → 轮询回执。"""
    bridge_write_readme(product)
    bdir = BRIDGE_DIR / product
    inbox, outbox = bdir / "inbox", bdir / "outbox"
    for d in (inbox, outbox):
        d.mkdir(parents=True, exist_ok=True)
    timeout_s = max(1, int(float(getattr(args, "timeout", DEFAULT_TIMEOUT)
                                 or DEFAULT_TIMEOUT)))
    started = time.time()
    approval_id = ""
    if getattr(args, "requires_approval", False):
        approval_id = approval_create("bridge", "", "bridge_dispatch", full_prompt)
        print(f"[agenthub] 需要人工审批：agenthub approval decide {approval_id} yes")
        deadline = started + timeout_s
        row = None
        while time.time() < deadline:
            row = approval_get(approval_id)
            if row and row.get("decision"):
                break
            time.sleep(2)
        if not row or not row.get("decision"):
            return ("", f"人工审批超时（{approval_id}）", 1,
                    time.time() - started, "approval timeout", "")
        if row["decision"] != "approved":
            return ("", f"人工审批被拒（{approval_id}）", 1,
                    time.time() - started, "approval rejected", "")
    lease_s = max(int(getattr(args, "lease_seconds", 300) or 300),
                  timeout_s if not getattr(args, "no_wait", False) else 300)
    lease_until = (dt.datetime.now() + dt.timedelta(seconds=lease_s)) \
        .isoformat(timespec="seconds")
    tid = dt.datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + os.urandom(3).hex()
    envelope = {"id": tid, "from": "agenthub", "to": product, "role": role_name or "",
                "chain": chain, "cwd": str(cwd), "created_at": now_iso(),
                "status": "pending", "prompt_file": f"task_{tid}.md",
                "contract": contract or {}, "approval_id": approval_id,
                "attempt": 1, "lease_until": lease_until,
                "heartbeat_at": now_iso()}
    jpath = inbox / f"task_{tid}.json"
    receipt_create(tid, "", f"bridge:{product}", {"product": product})
    effect_enqueue("bridge", tid, {
        "product": product, "inbox": str(inbox), "tid": tid,
        "envelope": envelope, "prompt": full_prompt})
    effect_run("bridge", limit=1)
    if not jpath.exists():
        return "", "桥接投递失败：effects 没有落盘", 1, 0.0, "bridge effect missing", ""
    note = f"桥接投递 inbox/{jpath.name}"
    print(f"[agenthub] 派工 → {product}（{note}）cwd={cwd}"
          + (f" 角色={role_name}" if role_name else "")
          + (f" 链路={' → '.join(chain)}" if len(chain) > 1 else "")
          + (f" 记忆查询=「{getattr(args, 'mem', '')}」"
             if getattr(args, "mem", "") else ""))
    if getattr(args, "no_wait", False):
        print("[agenthub] --no-wait：任务已投递，不等回执（agenthub bridge ls 看进度）")
        return "（任务已投递，--no-wait 模式未等待回执）", "", 0, 0.0, note, ""
    print(f"[agenthub] 等回执（最长 {timeout_s}s，每 5s 查一次 outbox）…")
    t0 = time.time()
    while time.time() - t0 < timeout_s:
        rpath = outbox / f"result_{tid}.json"
        if rpath.exists():
            try:
                res = json.loads(rpath.read_text(encoding="utf-8"))
            except ValueError:
                res = {"status": "done", "summary": "（回执不是合法 JSON，原文如下）",
                       "output": rpath.read_text(encoding="utf-8", errors="replace")}
            status = str(res.get("status") or "done")
            parts = [str(res.get("summary") or ""), str(res.get("output") or "")]
            if res.get("artifacts"):
                parts.append("产出: " + "; ".join(res["artifacts"]))
            out = "\n".join(x for x in parts if x)
            code = 0 if status.lower() in ("done", "ok", "success") else 1
            return out, "", code, time.time() - t0, note + f" · 回执 outbox/{rpath.name}", ""
        time.sleep(5)
    err = (f"\n[agenthub] 等回执超时（>{timeout_s}s）。任务还留在 inbox，"
           f"产品侧完成后回执会写进 outbox（agenthub bridge ls 查看）。")
    return "", err, 124, time.time() - t0, note, ""


def cmd_agents(con, args):
    print("可派工 · CLI 无头（已接通）：")
    for name, a in AGENTS.items():
        ok = shutil.which(a["bin"])
        print(f"  {'✓' if ok else '✗'} {name:<8} {a['desc']}")
        if ok:
            print(f"      {a['example']}")
    print("\n可派工 · 桌面桥接（共享目录 inbox/outbox）：")
    for name, b in BRIDGE_PRODUCTS.items():
        ok = agent_available(name)
        print(f"  {'✓' if ok else '✗'} {name:<12}{b['name']}  → {(BRIDGE_DIR / name / 'inbox')}")
    print("  产品侧接入说明：agenthub bridge setup <产品>")
    extra = [b for b in EXTRA_PROBES if shutil.which(b)]
    if extra:
        print(f"\n检测到但暂未接派工：{', '.join(extra)}")


def cmd_call(con, args):
    product_raw, task = args.product or "", args.task or ""
    if not task and product_raw and args.role and not product_raw.startswith("role:"):
        task, product_raw = product_raw, ""   # `call --role X "任务"`：唯一位置参数是任务
    role_name = (args.role or "").strip()
    if product_raw.startswith("role:"):
        product, role_name = resolve_role(product_raw)
    elif not product_raw and role_name:
        product, role_name = resolve_role(f"role:{role_name}")
    else:
        product = product_raw
        if role_name and product:
            cand = role_rows().get(role_name, {}).get("agents", [])
            if product not in cand:
                print(f"⚠ {product} 不在角色 {role_name} 的候选里"
                      f"（agenthub role assign {role_name} {product} 可加入），照常派工")
    if not product:
        raise SystemExit("要指定派给谁：产品名 / role:角色名"
                         "（agenthub agents 看列表，agenthub role list 看角色）")
    if product not in AGENTS and product not in BRIDGE_PRODUCTS:
        raise SystemExit(f"不认识的派工目标：{product}"
                         f"（CLI 无头：{'、'.join(AGENTS)}；桥接：{'、'.join(BRIDGE_PRODUCTS)}）")
    cwd = Path(args.cwd or os.getcwd()).resolve()
    if not cwd.exists():
        raise SystemExit(f"工作目录不存在：{cwd}")

    prompt = sys.stdin.read() if task in ("-", "") else task
    if not prompt.strip():
        raise SystemExit("任务内容为空")

    chain = chain_from_env()
    if product in chain:
        raise SystemExit(f"检测到循环派工：{' → '.join(chain + [product])} "
                         f"里 {product} 出现了两次")
    if len(chain) >= MAX_CHAIN_DEPTH:
        raise SystemExit(f"派工链路过深（已 {len(chain)} 层，上限 {MAX_CHAIN_DEPTH}）："
                         f"{' → '.join(chain)}")
    new_chain = chain + [product]

    mem_block = ""
    if args.mem:
        ensure_fresh(con)
        mem_block = build_context(con, args.mem, n=args.mem_n,
                                  project=detect_project(getattr(args, "cwd", None)))
        if not mem_block:
            print(f"[agenthub] 共享记忆里没有与「{args.mem}」相关的内容，照常派工")

    full_prompt = build_envelope(prompt, mem_block, new_chain, product, role_name)

    if product in AGENTS:
        out, err, code, dur, note, extra = cli_dispatch(args, product, cwd, full_prompt,
                                                        new_chain, role_name)
    else:
        out, err, code, dur, note, extra = bridge_dispatch(args, product, cwd, full_prompt,
                                                           new_chain, role_name)

    tpath = write_transcript(product, cwd, note, args.mem, mem_block, prompt,
                             code, dur, out, err, new_chain, role_name, extra)
    record_dispatch(con, product, cwd, prompt, args.mem, code, dur, tpath, out,
                    new_chain, role_name)
    if getattr(args, "ticket", None):
        pending = bool(getattr(args, "no_wait", False) and product not in AGENTS)
        t = ticket_attach_assignment(args.ticket, product, code, output=out,
                                     error=err, workspace=str(cwd),
                                     pending=pending, con=con)
        print(f"[agenthub] 工单 {args.ticket} → {t['stage']}")
    insert_note(con, product, "dispatch",
                f"派工: {prompt.strip().splitlines()[0][:60]}",
                f"agenthub,{product}" + (f",role:{role_name}" if role_name else ""),
                f"派工给 {product}"
                + (f"（角色 {role_name}）" if role_name else "")
                + (f"，链路 {' → '.join(new_chain)}" if len(new_chain) > 1 else "")
                + f"（cwd={cwd}）。\n任务: {prompt}\n结果摘要: {out.strip()[:1500] or '(无输出)'}")
    print(f"[agenthub] 完成：退出码 {code}，用时 {dur:.0f}s，完整记录 → {tpath}\n")
    out_clean = out.strip()
    if len(out_clean) > 8000:
        print(out_clean[:8000] + "\n…（输出过长，已截断，完整内容看上面的记录文件）")
    else:
        print(out_clean or "（子 Agent 没有文本输出，详见记录文件）")


def cmd_log(con, args):
    if args.show:
        r = con.execute("select * from dispatches where id=?", (args.show,)).fetchone()
        if not r:
            print(f"没有 #{args.show} 这条派工"); return
        p = Path(r["transcript"])
        print(p.read_text(encoding="utf-8", errors="replace") if p.exists()
              else r["output"])
        return
    rows = con.execute("select * from dispatches order by id desc limit ?",
                       (args.n,)).fetchall()
    if not rows:
        print("（还没有派工记录）"); return
    for r in rows:
        head = r["prompt"].strip().splitlines()[0][:56] if r["prompt"].strip() else ""
        ch = r["chain"] or ""
        chs = (f" ·链 {'→'.join(ch.split(','))}") if ch.count(",") else ""
        print(f"#{r['id']} {r['ts']} {r['product']:<7} exit={r['exit_code']} "
              f"{r['dur']:.0f}s{chs}  {head}")


# ================================================== 工作流引擎（Phase 2）===

# -------------------------------------------- 迷你 YAML 子集（零依赖解析器）----
# 只支持写工作流够用的那部分：缩进映射、序列（含映射项）、行内列表、| > 多行块、
# 引号字符串、数字/布尔。不支持行内映射/锚点/多文档——碰到了就带行号报错。

class YamlishError(ValueError):
    pass


def _yl_strip_comment(line):
    """行尾注释：# 在行首或前面是空白、且不在引号里才算注释。"""
    out, q = [], ""
    for i, ch in enumerate(line):
        if q:
            out.append(ch)
            if ch == q:
                q = ""
        elif ch in "\"'":
            q = ch
            out.append(ch)
        elif ch == "#" and (i == 0 or line[i - 1] in " \t"):
            break
        else:
            out.append(ch)
    return "".join(out).rstrip()


def _yl_unquote(s, no):
    q = s[0]
    if len(s) < 2 or not s.endswith(q):
        raise YamlishError(f"第 {no} 行：引号没闭合：{s}")
    body = s[1:-1]
    if q == "'":
        return body.replace("''", "'")
    return (body.replace("\\\\", "\0").replace('\\"', '"').replace("\\n", "\n")
                .replace("\\t", "\t").replace("\0", "\\"))


def _yl_split_items(s):
    """逗号切分（引号内不切），用于行内列表。"""
    out, cur, q = [], "", ""
    for ch in s:
        if q:
            cur += ch
            if ch == q:
                q = ""
        elif ch in "\"'":
            q = ch
            cur += ch
        elif ch == ",":
            out.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        out.append(cur.strip())
    return out


def _yl_scalar(s, no):
    s = s.strip()
    if s.startswith("[") and s.endswith("]"):
        inner = s[1:-1].strip()
        return [] if not inner else [_yl_scalar(x, no) for x in _yl_split_items(inner)]
    if s.startswith("{") and s.endswith("}"):
        raise YamlishError(f"第 {no} 行：暂不支持行内映射 {{…}}，请改成缩进写法")
    if s[:1] in ("\"", "'"):
        return _yl_unquote(s, no)
    low = s.lower()
    if low in ("true", "yes", "on"):
        return True
    if low in ("false", "no", "off"):
        return False
    if low in ("null", "~"):
        return None
    try:
        return int(s)
    except ValueError:
        pass
    try:
        return float(s)
    except ValueError:
        pass
    return s


def yamlish_load(text):
    """迷你 YAML 子集 → Python 对象。只保证工作流定义需要的语法，报错带行号。"""
    raw = text.splitlines()
    cur = [0]

    def peek():
        # 跳过空行/纯注释/---，返回 (行号, 缩进, 去注释内容)；文件尾返回 (0, -1, "")
        while cur[0] < len(raw):
            ln = raw[cur[0]]
            if "\t" in ln[:len(ln) - len(ln.lstrip())]:
                raise YamlishError(f"第 {cur[0] + 1} 行：用了 Tab 缩进，YAML 只认空格")
            body = _yl_strip_comment(ln)
            if not body.strip() or body.strip() == "---":
                cur[0] += 1
                continue
            return cur[0] + 1, len(ln) - len(ln.lstrip(" ")), body.strip()
        return 0, -1, ""

    def read_block(ind, fold, chomp):
        # 收集缩进更深的原文行（不剥注释、保留空行与相对缩进）
        parts = []
        while cur[0] < len(raw):
            ln = raw[cur[0]]
            if not ln.strip():
                parts.append("")
                cur[0] += 1
                continue
            li = len(ln) - len(ln.lstrip(" "))
            if li <= ind:
                break
            parts.append(ln[li:])
            cur[0] += 1
        while parts and parts[-1] == "":
            parts.pop()
        if not parts:
            return ""
        if fold:
            out = []
            for p in parts:
                if p == "":
                    if out and out[-1] != "\n":
                        out.append("\n")
                elif not out or out[-1] == "\n":
                    out.append(p)
                else:
                    out.append(" " + p)
            body = "".join(out)
        else:
            body = "\n".join(parts)
        return body if chomp == "-" else body + "\n"

    def parse_block(ind):
        no, ci, content = peek()
        if no == 0:
            raise YamlishError("内容意外结束")
        if content.startswith("- ") or content == "-":
            return parse_seq(ci)
        return parse_map(ci)

    def parse_map(ind, first=None):
        m, pending = {}, first
        while True:
            if pending is not None:
                no, ci, content = pending
                pending = None
            else:
                no, ci, content = peek()
                if no == 0 or ci < ind:
                    return m
                if ci > ind:
                    raise YamlishError(f"第 {no} 行：缩进多了一层（{ci} 空格，这层应为 {ind}）")
                if content.startswith("- ") or content == "-":
                    return m
                cur[0] += 1                      # 消费掉这一行键
            if ":" not in content:
                raise YamlishError(f"第 {no} 行：这行不是「键: 值」格式：{content}")
            key, _, rest = content.partition(":")
            key = _yl_scalar(key, no)
            if not isinstance(key, str) or not key.strip():
                raise YamlishError(f"第 {no} 行：键必须是文本：{content}")
            key = key.strip()
            rest = rest.strip()
            if rest in ("|", "|-", ">", ">-"):
                m[key] = read_block(ind, fold=rest[0] == ">",
                                    chomp="-" if rest.endswith("-") else "")
            elif rest:
                if rest.startswith("- "):
                    raise YamlishError(f"第 {no} 行：值不能直接跟「- 」，请换一行写")
                m[key] = _yl_scalar(rest, no)
            else:
                no2, ci2, ct2 = peek()
                if no2 and ci2 > ind:
                    m[key] = parse_block(ci2)
                elif no2 and ci2 == ind and (ct2.startswith("- ") or ct2 == "-"):
                    m[key] = parse_seq(ind)      # 序列与键同层的写法
                else:
                    m[key] = None
        return m

    def parse_seq(ind):
        items = []
        while True:
            no, ci, content = peek()
            if no == 0 or ci < ind:
                return items
            if ci > ind:
                raise YamlishError(f"第 {no} 行：缩进多了一层（这层应为 {ind} 空格）")
            if not (content == "-" or content.startswith("- ")):
                return items
            cur[0] += 1                          # 消费掉 "- …" 这一行
            rest = content[1:].strip()
            if not rest:
                no2, ci2, _ = peek()
                items.append(parse_block(ci2) if no2 and ci2 > ind else None)
                continue
            if rest.startswith("- "):
                raise YamlishError(f"第 {no} 行：不支持嵌套序列（- - …），请用缩进分层")
            if re.match(r"^[^\s:]+\s*:(\s|$)", rest):
                # "- key: value" → 当成缩进 +2 的映射首行，后续键继续挂在这项下面
                items.append(parse_map(ind + 2, first=(no, ind + 2, rest)))
            else:
                items.append(_yl_scalar(rest, no))
        return items

    no, ci, content = peek()
    if no == 0:
        return {}
    val = parse_block(ci)
    no2, ci2, ct2 = peek()
    if no2:
        raise YamlishError(f"第 {no2} 行：顶层块已结束，这行没法挂进去：{ct2}")
    return val


# ------------------------------------------------------- 工作流载入与校验 ----

WF_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")
WF_COND_RE = re.compile(r"^([A-Za-z0-9_-]+)\.verdict\s*(==|!=)\s*(pass|fail)$")
WF_KNOWN_TOP = {"name", "version", "project_dir", "project_context", "team",
                "steps", "sub_workflows", "budget", "contract"}
WF_KNOWN_STEP = {"id", "role", "description", "prompt", "input", "output",
                 "depends_on", "retry_on_failure", "loop_back_to", "max_loops",
                 "condition", "timeout", "mem", "parallel", "objective",
                 "output_schema", "acceptance", "tools", "constraints", "mode",
                 "budget", "tool", "arguments", "approval"}


def wf_load(path):
    """读入 + 校验工作流 YAML。错误直接 SystemExit（消息给人看），成功返回 flow 字典。"""
    path = Path(path)
    if not path.exists():
        raise SystemExit(f"找不到工作流文件：{path}")
    text = path.read_text(encoding="utf-8")
    try:
        data = yamlish_load(text)
    except YamlishError as e:
        raise SystemExit(f"工作流 YAML 解析失败（{path}）：{e}")
    if not isinstance(data, dict):
        raise SystemExit("工作流文件顶层必须是一个映射（key: value）")
    warns = [f"未认识的顶层字段「{k}」（拼写错了？）" for k in data if k not in WF_KNOWN_TOP]
    name = str(data.get("name") or "").strip()
    if not name:
        raise SystemExit("工作流缺 name 字段")
    if data.get("sub_workflows"):
        raise SystemExit("sub_workflows 暂不支持（v1 只跑单层 DAG），请把子工作流展开成平铺步骤")

    team = {}
    for role, product in (data.get("team") or {}).items():
        product = str(product).strip()
        if product not in AGENTS and product not in BRIDGE_PRODUCTS:
            raise SystemExit(f"team.{role} = {product} 不是已接入的 Agent"
                             f"（CLI 无头：{'、'.join(AGENTS)}；桥接：{'、'.join(BRIDGE_PRODUCTS)}）")
        team[role] = product

    ctx = []
    for item in (data.get("project_context") or []):
        if not isinstance(item, dict) or not str(item.get("name") or "").strip():
            raise SystemExit("project_context 每项都要有 name，内容用 file 或 content 给")
        t = ""
        if item.get("file"):
            f = path.parent / str(item["file"])
            if f.exists():
                t = f.read_text(encoding="utf-8", errors="replace")
            else:
                warns.append(f"项目上下文「{item['name']}」的文件不存在：{f}")
        else:
            t = str(item.get("content") or "")
        ctx.append({"name": str(item["name"]), "text": t})

    steps_raw = data.get("steps")
    if not isinstance(steps_raw, list) or not steps_raw:
        raise SystemExit("工作流必须有 steps（非空列表）")
    steps, by_id = [], {}
    for i, sr in enumerate(steps_raw, 1):
        if not isinstance(sr, dict):
            raise SystemExit(f"steps 第 {i} 项必须是一个映射（- id: … 开头）")
        sid = str(sr.get("id") or "").strip()
        if not WF_ID_RE.match(sid):
            raise SystemExit(f"steps 第 {i} 项：id「{sid}」不合法（只许字母、数字、_、-）")
        if sid in by_id:
            raise SystemExit(f"步骤 id 重复：{sid}")
        if "sub_workflow" in sr:
            raise SystemExit(f"步骤 {sid}：sub_workflow 暂不支持（v1 只跑单层 DAG）")
        warns += [f"步骤 {sid}：未认识的字段「{k}」" for k in sr if k not in WF_KNOWN_STEP]
        tool = str(sr.get("tool") or "").strip()
        role = str(sr.get("role") or "").strip()
        if tool:
            if "/" not in tool:
                raise SystemExit(f"步骤 {sid}：tool 要写成「server名/工具名」，收到的是：{tool}")
            server, tool_name = tool.split("/", 1)
            if not server or not tool_name:
                raise SystemExit(f"步骤 {sid}：tool 要写成「server名/工具名」，收到的是：{tool}")
            if server != "self" and server not in mcp_registry():
                raise SystemExit(f"步骤 {sid}：MCP server「{server}」没注册"
                                 f"（agenthub mcp list 看名单；自带的用 self/工具名）")
            product, role_name, role = f"mcp:{server}", "", "tool"
        else:
            if not role:
                raise SystemExit(f"步骤 {sid}：缺 role（team 里要有的角色名，或直接写产品名）")
            if role in team:
                product, role_name = team[role], role
            elif role in AGENTS or role in BRIDGE_PRODUCTS:
                product, role_name = role, ""
            else:
                raise SystemExit(f"步骤 {sid} 的角色「{role}」没在 team 里配置，也不是已接入产品")
        if not sr.get("prompt") and not sr.get("description"):
            raise SystemExit(f"步骤 {sid}：要有 prompt 或 description（说清干什么活）")
        deps = sr.get("depends_on")
        if deps is None:
            deps = []
        elif isinstance(deps, str):
            deps = [deps]
        elif not isinstance(deps, list):
            raise SystemExit(f"步骤 {sid}：depends_on 要是列表或单个 id")
        deps = [str(d).strip() for d in deps]
        retry = int(sr.get("retry_on_failure") or 0)
        if retry < 0:
            raise SystemExit(f"步骤 {sid}：retry_on_failure 不能是负数")
        timeout = int(sr["timeout"]) if sr.get("timeout") else None
        cond = None
        cond_raw = str(sr.get("condition") or "").strip()
        if cond_raw:
            m = WF_COND_RE.match(cond_raw)
            if not m:
                raise SystemExit(f"步骤 {sid}：condition 暂只支持「<步骤id>.verdict == pass|fail」"
                                 f"（== 或 !=），收到的是：{cond_raw}")
            cond = (m.group(1), m.group(2), m.group(3))
            if cond[0] not in [str(d).strip() for d in deps] and cond[0] != sid:
                raise SystemExit(f"步骤 {sid}：condition 引用的「{cond[0]}」必须同时出现在"
                                 f" depends_on 里（引擎要在它出结果后才能判条件）")
        s = {"id": sid, "role": role, "product": product, "role_name": role_name,
             "prompt": str(sr.get("prompt") or ""), "description": str(sr.get("description") or ""),
             "input": str(sr.get("input") or ""), "output": str(sr.get("output") or ""),
             "depends_on": deps, "retry": retry, "timeout": timeout,
             "mem": str(sr.get("mem") or ""), "condition": cond,
             "loop_back_to": str(sr.get("loop_back_to") or ""),
             "max_loops": int(sr.get("max_loops") or 2),
             "mode": str(sr.get("mode") or "tool"),
             "objective": str(sr.get("objective") or ""),
             "output_schema": sr.get("output_schema") or "",
             "acceptance": sr.get("acceptance") or [],
             "tools": sr.get("tools") or [],
             "constraints": sr.get("constraints") or [],
             "tool": tool,
             "arguments": sr.get("arguments") if isinstance(sr.get("arguments"), dict) else {},
             "approval": bool(sr.get("approval")),
             "budget": sr.get("budget") if isinstance(sr.get("budget"), dict) else {}}
        steps.append(s)
        by_id[sid] = s

    for s in steps:
        for d in s["depends_on"]:
            if d not in by_id:
                raise SystemExit(f"步骤 {s['id']}：depends_on 里的「{d}」不存在")
            if d == s["id"]:
                raise SystemExit(f"步骤 {s['id']}：不能依赖自己")

    # DAG 环检测 + 静态波次（Kahn 拓扑分层）
    indeg = {s["id"]: 0 for s in steps}
    children = {s["id"]: [] for s in steps}
    for s in steps:
        for d in set(s["depends_on"]):
            indeg[s["id"]] += 1
            children[d].append(s["id"])
    wave_ids, waves = [i for i, d in indeg.items() if d == 0], []
    while wave_ids:
        waves.append(sorted(wave_ids))
        nxt = []
        for x in wave_ids:
            for c in children[x]:
                indeg[c] -= 1
                if indeg[c] == 0:
                    nxt.append(c)
        wave_ids = nxt
    if sum(len(w) for w in waves) != len(steps):
        stuck = sorted(i for i, d in indeg.items() if d > 0)
        raise SystemExit("工作流存在依赖环，涉及步骤：" + "、".join(stuck))

    # loop_back_to：目标必须存在，且是本步骤的（传递）上游——打回只能打回干过的活
    def wf_ancestors(sid):
        seen, stack = set(), list(by_id[sid]["depends_on"])
        while stack:
            x = stack.pop()
            if x in seen:
                continue
            seen.add(x)
            stack.extend(by_id[x]["depends_on"])
        return seen

    loops = []
    for s in steps:
        if not s["loop_back_to"]:
            continue
        t = s["loop_back_to"]
        if t not in by_id:
            raise SystemExit(f"步骤 {s['id']}：loop_back_to「{t}」不存在")
        if t not in wf_ancestors(s["id"]):
            raise SystemExit(f"步骤 {s['id']}：loop_back_to 只能打回自己的（传递）上游，「{t}」不是")
        if s["max_loops"] < 1:
            raise SystemExit(f"步骤 {s['id']}：max_loops 至少为 1")
        loops.append((s["id"], t, s["max_loops"]))
    for s in steps:
        if s["condition"] and s["condition"][0] not in by_id:
            raise SystemExit(f"步骤 {s['id']}：condition 引用的「{s['condition'][0]}」不存在")

    # verdict 约定涉及谁：循环源头 + 循环目标 + 被 condition 引用的步骤
    verdict_steps = ({s["id"] for s in steps if s["loop_back_to"]}
                     | {s["loop_back_to"] for s in steps if s["loop_back_to"]}
                     | {s["condition"][0] for s in steps if s["condition"]})

    budget = data.get("budget") if isinstance(data.get("budget"), dict) else {}
    contract_strict = str(data.get("contract") or "").strip().lower() in ("strict", "true", "1")
    return {"name": name, "version": str(data.get("version") or "1.0"),
            "workflow_file": str(path), "workflow_text": text,
            "project_dir": str(data.get("project_dir") or ""),
            "context": ctx, "team": team, "steps": steps, "by_id": by_id,
            "waves": waves, "loops": loops, "verdict_steps": sorted(verdict_steps),
            "budget": budget, "contract_strict": contract_strict,
            "warnings": warns}


# ------------------------------------------------------ 项目目录与运行状态 ----

PROJECTS_DIR = APP_DIR / "projects"


def wf_project_dir(name):
    slug = re.sub(r'[\\/:*?"<>|\s]+', "-", str(name).strip()).strip("-") or "project"
    return PROJECTS_DIR / slug


def _atomic_json(path, obj):
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(obj, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def wf_new_state(flow, project, run_id, workdir):
    return {"run_id": run_id, "trace_id": run_id,
            "workflow": flow["name"], "project": project,
            "workflow_file": flow["workflow_file"], "workdir": str(workdir),
            "status": "running", "created_at": now_iso(), "updated_at": now_iso(),
            "wave": 0, "loops": {}, "loop_notes": {},
            "budget": flow.get("budget") or {},
            "steps": {s["id"]: {"status": "pending", "attempts": 0, "verdict": "",
                                "exit_code": None, "dur": 0.0, "output_file": "",
                                "error": "", "ts": "", "started_at": "",
                                "ended_at": ""} for s in flow["steps"]}}


def wf_save_state(run_dir, state):
    state["updated_at"] = now_iso()
    _atomic_json(Path(run_dir) / "state.json", state)


def wf_load_state(run_dir):
    try:
        return json.loads((Path(run_dir) / "state.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


# ------------------------------------------------- P0：事件 / 检查点 / 契约 ----

_P0_EVENT_LOCK = threading.Lock()


def wf_event(run_dir, event_type, step_id="", payload=None, con=None):
    """追加一条运行事件，同时写 run_dir/trace.jsonl；返回写入的记录。

    事件日志是恢复和观测的真值源；state.json 只是它的一个快照。
    """
    run_dir = Path(run_dir)
    run_id = run_dir.name
    ts = now_iso()
    payload = payload or {}
    own = con is None
    con = con or connect()
    try:
        with _P0_EVENT_LOCK:
            row = con.execute(
                "select coalesce(max(seq),0) as m from run_events where run_id=?",
                (run_id,)).fetchone()
            seq = int(row["m"] if row else 0) + 1
            con.execute(
                "insert into run_events(run_id,seq,ts,type,step_id,payload)"
                " values(?,?,?,?,?,?)",
                (run_id, seq, ts, event_type, step_id or "",
                 json.dumps(payload, ensure_ascii=False)))
        rec = {"run_id": run_id, "seq": seq, "ts": ts, "type": event_type,
               "step_id": step_id or "", "payload": payload}
        try:
            with (run_dir / "trace.jsonl").open("a", encoding="utf-8") as f:
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")
        except OSError:
            pass
        return rec
    finally:
        if own:
            con.close()


def wf_event_list(run_id, con=None):
    own = con is None
    con = con or connect()
    try:
        rows = con.execute(
            "select * from run_events where run_id=? order by seq",
            (run_id,)).fetchall()
        out = []
        for r in rows:
            try:
                p = json.loads(r["payload"] or "{}")
            except ValueError:
                p = {}
            out.append({"run_id": r["run_id"], "seq": r["seq"], "ts": r["ts"],
                        "type": r["type"], "step_id": r["step_id"], "payload": p})
        return out
    finally:
        if own:
            con.close()


def wf_checkpoint_save(run_dir, step_id, attempt, status, prompt="",
                       output_path="", error="", started_at="", ended_at="",
                       con=None):
    run_id = Path(run_dir).name
    ih = hashlib.sha256((prompt or "").encode("utf-8")).hexdigest() if prompt else ""
    own = con is None
    con = con or connect()
    try:
        con.execute(
            "insert or replace into step_checkpoints"
            "(run_id,step_id,attempt,status,input_hash,output_path,error,"
            " started_at,ended_at) values(?,?,?,?,?,?,?,?,?)",
            (run_id, step_id, int(attempt), status, ih, str(output_path or ""),
             str(error or ""), started_at or "", ended_at or ""))
    finally:
        if own:
            con.close()


def wf_checkpoint_list(run_id, step_id=None, con=None):
    own = con is None
    con = con or connect()
    try:
        sql = "select * from step_checkpoints where run_id=?"
        args = [run_id]
        if step_id:
            sql += " and step_id=?"
            args.append(step_id)
        sql += " order by step_id, attempt"
        return [dict(r) for r in con.execute(sql, args).fetchall()]
    finally:
        if own:
            con.close()


def _num(v, cast, default=0):
    try:
        return cast(v or 0)
    except (TypeError, ValueError):
        return default


def wf_budget_new(run_id, budget=None, started_at="", con=None):
    b = budget or {}
    own = con is None
    con = con or connect()
    try:
        con.execute(
            "insert into run_budgets"
            "(run_id,token_limit,cost_limit,time_limit_s,max_parallel,max_depth,"
            " used_tokens,used_cost,started_at,updated_at)"
            " values(?,?,?,?,?,?,?,?,?,?)"
            " on conflict(run_id) do update set"
            " token_limit=excluded.token_limit, cost_limit=excluded.cost_limit,"
            " time_limit_s=excluded.time_limit_s, max_parallel=excluded.max_parallel,"
            " max_depth=excluded.max_depth, updated_at=excluded.updated_at",
            (run_id, _num(b.get("tokens"), int), _num(b.get("cost"), float),
             _num(b.get("seconds"), float), _num(b.get("max_parallel"), int),
             _num(b.get("max_depth"), int), 0, 0.0,
             started_at or now_iso(), now_iso()))
    finally:
        if own:
            con.close()


def wf_budget_get(run_id, con=None):
    own = con is None
    con = con or connect()
    try:
        r = con.execute("select * from run_budgets where run_id=?", (run_id,)).fetchone()
        return dict(r) if r else None
    finally:
        if own:
            con.close()


def wf_budget_add_usage(run_id, tokens=0, cost=0.0, con=None):
    own = con is None
    con = con or connect()
    try:
        con.execute(
            "update run_budgets set used_tokens=used_tokens+?,"
            " used_cost=used_cost+?, updated_at=? where run_id=?",
            (int(tokens or 0), float(cost or 0.0), now_iso(), run_id))
    finally:
        if own:
            con.close()


def wf_budget_exceeded(run_id, started_at=None, con=None):
    """返回 (是否超预算, 原因)。token/cost 是尽力统计，时间预算是硬闸。"""
    row = wf_budget_get(run_id, con=con)
    if not row:
        return False, ""
    if row.get("token_limit") and row.get("used_tokens", 0) > row["token_limit"]:
        return True, f"token 预算超限：已用 {row['used_tokens']}，上限 {row['token_limit']}"
    if row.get("cost_limit") and row.get("used_cost", 0.0) > row["cost_limit"]:
        return True, f"成本预算超限：已用 {row['used_cost']:.3f}，上限 {row['cost_limit']:.3f}"
    if row.get("time_limit_s"):
        t0 = started_at
        if t0 is None and row.get("started_at"):
            try:
                t0 = dt.datetime.fromisoformat(row["started_at"]).timestamp()
            except ValueError:
                t0 = None
        if t0 is not None:
            elapsed = time.time() - t0
            if elapsed > row["time_limit_s"]:
                return True, (f"时间预算超限：已用 {elapsed:.1f}s，"
                              f"上限 {row['time_limit_s']:.0f}s")
    return False, ""


def _as_list(v):
    if v is None:
        return []
    if isinstance(v, (list, tuple)):
        return [str(x) for x in v]
    return [str(v)]


def wf_contract(flow, s):
    """派工契约：把自然语言任务里隐含的验收条件变成机器可校验字段。"""
    b = dict(flow.get("budget") or {})
    sb = s.get("budget") if isinstance(s.get("budget"), dict) else {}
    b.update(sb)
    return {
        "mode": str(s.get("mode") or "tool"),
        "objective": str(s.get("objective") or s.get("description") or ""),
        "inputs": _as_list(s.get("input")),
        "constraints": _as_list(s.get("constraints")),
        "tools": _as_list(s.get("tools")),
        "output_schema": s.get("output_schema") or s.get("output") or "",
        "acceptance": _as_list(s.get("acceptance")),
        "budget": b,
        "return_to": "manager",
    }


def wf_contract_missing(contract):
    miss = []
    if not contract.get("objective"):
        miss.append("objective")
    if not contract.get("output_schema"):
        miss.append("output_schema")
    if not contract.get("acceptance"):
        miss.append("acceptance")
    return miss


def wf_validate_contracts(flow, strict=False):
    problems = []
    for s in flow.get("steps") or []:
        miss = wf_contract_missing(wf_contract(flow, s))
        if miss:
            problems.append(f"步骤 {s.get('id')}：派工契约缺 {'/'.join(miss)}")
    if strict and problems:
        raise SystemExit("严格契约模式拒绝运行：\n" + "\n".join(problems))
    return problems


def wf_schedule_groups(steps, max_parallel):
    """把同一波的就绪步骤重新打包：不超过并发上限，且同一 Agent 不同组。"""
    limit = max(1, int(max_parallel or 1))
    groups = []
    for s in steps:
        for g in groups:
            if len(g) < limit and all(x.get("product") != s.get("product") for x in g):
                g.append(s)
                break
        else:
            groups.append([s])
    return groups


def wf_extract_usage(text):
    """从子 Agent 输出里尽力抠 token/成本；抠不到就返回 0，不假装有数据。"""
    t = text or ""
    m = re.search(r'"total_tokens"\s*:\s*(\d+)', t, re.I)
    if m:
        tokens = int(m.group(1))
    else:
        nums = [int(x) for x in re.findall(
            r'"(?:input_tokens|output_tokens|tokens)"\s*:\s*(\d+)', t, re.I)]
        tokens = sum(nums) if nums else 0
        if not tokens:
            m = re.search(r'\btokens?\s*[:=]\s*(\d+)', t, re.I)
            if m:
                tokens = int(m.group(1))
    m = re.search(r'"(?:cost|credit|total_cost)"\s*:\s*([0-9]+(?:\.[0-9]+)?)', t, re.I)
    cost = float(m.group(1)) if m else 0.0
    return tokens, cost


def approval_create(run_id, step_id, kind, request, con=None):
    aid = "ap-" + os.urandom(4).hex()
    own = con is None
    con = con or connect()
    try:
        con.execute(
            "insert into approvals(id,run_id,step_id,kind,request,decision,"
            " created_at) values(?,?,?,?,?,'',?)",
            (aid, run_id or "", step_id or "", kind or "", request or "", now_iso()))
        return aid
    finally:
        if own:
            con.close()


def approval_get(aid, con=None):
    own = con is None
    con = con or connect()
    try:
        r = con.execute("select * from approvals where id=?", (aid,)).fetchone()
        return dict(r) if r else None
    finally:
        if own:
            con.close()


def approval_decide(aid, decision, by="user", con=None):
    decision = "approved" if str(decision).lower() in ("yes", "y", "approved", "ok") \
        else "rejected"
    own = con is None
    con = con or connect()
    try:
        cur = con.execute(
            "update approvals set decision=?, decided_by=?, decided_at=?"
            " where id=? and (decision is null or decision='')",
            (decision, by or "user", now_iso(), aid))
        return cur.rowcount > 0
    finally:
        if own:
            con.close()


def approval_list(con=None, run_id=None, limit=20):
    own = con is None
    con = con or connect()
    try:
        sql = "select * from approvals"
        args = []
        if run_id:
            sql += " where run_id=?"
            args.append(run_id)
        sql += " order by created_at desc limit ?"
        args.append(int(limit or 20))
        return [dict(r) for r in con.execute(sql, args).fetchall()]
    finally:
        if own:
            con.close()


def wf_trace_text(run_id, con=None):
    evs = wf_event_list(run_id, con=con)
    if not evs:
        return "（没有事件记录）"
    lines = []
    for e in evs:
        p = e.get("payload") or {}
        bits = []
        for k in ("workflow", "project", "agent", "role", "status", "exit_code",
                  "dur", "attempt", "output", "error", "reason", "budget"):
            if k in p and p[k] not in ("", None):
                v = p[k]
                if isinstance(v, (dict, list)):
                    v = json.dumps(v, ensure_ascii=False)
                if k in ("output", "error") and isinstance(v, str) and len(v) > 120:
                    v = v[:117] + "..."
                bits.append(f"{k}={v}")
        line = f"{e['seq']:>4} {e['ts']} {e['type']:<18} {e['step_id']:<20}"
        if bits:
            line += " " + " ".join(bits)
        lines.append(line)
    return "\n".join(lines)


# ------------------------------------------ P1.5 工单 / 授权 / 注意力 ----

POLICY_PATH = APP_DIR / "policy.json"

DEFAULT_POLICY = {
    "mode": "observe",          # off / observe / auto
    "sources": {
        "cli": {"allowed": ["*"], "admission": "explicit"},
        "mcp": {"allowed": ["*"], "admission": "inferred"},
        "workflow": {"allowed": ["*"], "admission": "inferred"},
        "bridge": {"allowed": ["*"], "admission": "inferred"},
        "github": {"allowed": [], "admission": "inferred"},
        "feishu": {"allowed": [], "admission": "inferred"},
    },
    "rules": [],
}


def policy_load():
    try:
        p = json.loads(POLICY_PATH.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        p = {}
    if not isinstance(p, dict):
        p = {}
    p.setdefault("mode", "observe")
    p.setdefault("rules", [])
    sources = p.setdefault("sources", {})
    for name, default in DEFAULT_POLICY["sources"].items():
        sources.setdefault(name, dict(default))
    return p


def policy_save(p):
    POLICY_PATH.parent.mkdir(parents=True, exist_ok=True)
    _atomic_json(POLICY_PATH, p)


def policy_version(p=None):
    p = p or policy_load()
    blob = json.dumps({"mode": p.get("mode"), "rules": p.get("rules"),
                       "sources": p.get("sources")},
                      sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()[:16]


def policy_gate(source, actor="", addressed=True, p=None):
    """确定性准入：先判断谁可以发起、以什么强度发起，再交给模型判断。"""
    p = p or policy_load()
    part = (p.get("sources") or {}).get(source)
    if not part:
        return False, "", "unknown_source"
    allowed = part.get("allowed") or []
    actor = actor or ""
    is_allowed = ("*" in allowed) or (bool(actor) and actor in allowed)
    if is_allowed and addressed:
        return True, "explicit", "addressed"
    if addressed:
        if part.get("admission") == "explicit":
            return True, "explicit", "admission"
        return False, "", "not_a_member"
    if is_allowed and p.get("mode") != "off":
        return True, "inferred", "admission"
    if p.get("mode") == "auto" and part.get("admission") in ("inferred", "auto"):
        return True, "inferred", "admission"
    return False, "", "admission_ignored"


def _ticket_id():
    return "tk-" + dt.datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + os.urandom(2).hex()


def _ticket_now():
    """工单时间用毫秒，避免同一秒内“报告”和“人工回答”比较不出先后。"""
    return dt.datetime.now().isoformat(timespec="milliseconds")


def mandate_get(ticket_id, con=None):
    own = con is None
    con = con or connect()
    try:
        r = con.execute(
            "select * from mandates where ticket_id=? order by created_at desc limit 1",
            (ticket_id,)).fetchone()
        return dict(r) if r else None
    finally:
        if own:
            con.close()


def ticket_get(tid, con=None):
    own = con is None
    con = con or connect()
    try:
        r = con.execute("select * from tickets where id=?", (tid,)).fetchone()
        return dict(r) if r else None
    finally:
        if own:
            con.close()


def ticket_assignments(tid, con=None):
    own = con is None
    con = con or connect()
    try:
        return [dict(r) for r in con.execute(
            "select * from assignments where ticket_id=? order by rowid desc", (tid,))]
    finally:
        if own:
            con.close()


def ticket_input_rows(tid, con=None):
    own = con is None
    con = con or connect()
    try:
        return [dict(r) for r in con.execute(
            "select * from ticket_inputs where ticket_id=? order by id", (tid,))]
    finally:
        if own:
            con.close()


def ticket_compute_stage(t, assignment=None):
    """按 docs/003 的顺序推导阶段：先看人，再看工作，再看空闲。"""
    if not t:
        return "ended"
    if t.get("ended_at"):
        return "ended"
    if t.get("attention_kind") == "question":
        return "person_answer"
    report = t.get("latest_report") or ""
    rap = t.get("latest_report_at") or ""
    inp = t.get("latest_input_at") or ""
    newer_input = bool(inp and rap and inp > rap)
    if report == "ready" and not newer_input:
        return "person_review"
    if report in ("blocked", "failed") and not newer_input:
        return "person_" + report
    if newer_input:
        if assignment is None or assignment.get("status") == "pending":
            return "working_queued"
        return "working"
    if assignment is None:
        return "working_queued"
    if assignment.get("status") == "pending":
        return "working_queued"
    if report == "waiting" or assignment.get("status") == "ended":
        return "idle"
    return "working"


def ticket_refresh(tid, con=None):
    own = con is None
    con = con or connect()
    try:
        t = con.execute("select * from tickets where id=?", (tid,)).fetchone()
        if not t:
            return None
        t = dict(t)
        a = con.execute(
            "select * from assignments where ticket_id=? order by rowid desc limit 1",
            (tid,)).fetchone()
        stage = ticket_compute_stage(t, dict(a) if a else None)
        con.execute("update tickets set stage=?, updated_at=? where id=?",
                    (stage, now_iso(), tid))
        t["stage"] = stage
        return t
    finally:
        if own:
            con.close()


def _link_run_con(con, tid, run_id, executor="workflow", workspace=""):
    now = _ticket_now()
    con.execute("update tickets set run_id=?, workspace=case when ?<>'' then ? else workspace end,"
                " updated_at=? where id=?",
                (run_id, workspace or "", workspace or "", now, tid))
    aid = "as-" + hashlib.sha1((tid + "|" + run_id).encode("utf-8")).hexdigest()[:12]
    con.execute(
        "insert or ignore into assignments(id,ticket_id,executor,status,run_id,"
        "workspace,started_at) values(?,?,?,?,?,?,?)",
        (aid, tid, executor, "pending", run_id, workspace, now))
    con.execute("update assignments set status='running', started_at=? "
                "where id=? and status='pending'", (now, aid))
    return aid


def ticket_link_run(tid, run_id, executor="workflow", workspace="", con=None):
    own = con is None
    con = con or connect()
    try:
        if not con.execute("select 1 from tickets where id=?", (tid,)).fetchone():
            raise SystemExit(f"工单不存在：{tid}（agenthub ticket list 看看）")
        _link_run_con(con, tid, run_id, executor, workspace)
        return ticket_refresh(tid, con=con)
    finally:
        if own:
            con.close()


def ticket_create(source, title, requester="", source_event_id="", scope="",
                  project="", run_id="", workspace="", strength="explicit",
                  matched_rules=None, body="", force=True, con=None):
    """建工单 + 授权。force=True 用于人工直接建单；intake 走 policy_gate。"""
    title = (title or "").strip()
    if not title:
        raise SystemExit("工单要有标题")
    own = con is None
    con = con or connect()
    try:
        if source_event_id:
            old = con.execute(
                "select * from tickets where source=? and source_event_id=?",
                (source, source_event_id)).fetchone()
            if old:
                return dict(old), False
        if not force:
            ok, got_strength, reason = policy_gate(source, requester, True)
            if not ok:
                raise SystemExit(f"未准入：{reason}")
            strength = got_strength
        p = policy_load()
        now = _ticket_now()
        tid = _ticket_id()
        mid = "md-" + os.urandom(4).hex()
        rules = ",".join(matched_rules or [])
        con.execute(
            "insert into mandates(id,ticket_id,policy_version,strength,matched_rules,"
            "actor,scope,created_at) values(?,?,?,?,?,?,?,?)",
            (mid, tid, policy_version(p), strength or "explicit", rules,
             requester or "", scope or "", now))
        con.execute(
            "insert into tickets(id,source,source_event_id,requester,title,scope,"
            "stage,mandate_id,run_id,project,workspace,created_at,updated_at) "
            "values(?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (tid, source or "cli", source_event_id or "", requester or "", title,
             scope or "", "working_queued", mid, run_id or "", project or "",
             workspace or "", now, now))
        if body:
            con.execute(
                "insert into ticket_inputs(ticket_id,kind,body,human,created_at)"
                " values(?,?,?,?,?)",
                (tid, "trigger", body, 1, now))
            con.execute("update tickets set latest_input_at=? where id=?", (now, tid))
        if run_id:
            _link_run_con(con, tid, run_id, "workflow")
        return ticket_refresh(tid, con=con), True
    finally:
        if own:
            con.close()


def _record_assessment(con, ticket_id, source, event_id, assessment, decision):
    aid = "asm-" + hashlib.sha1(
        (str(ticket_id) + "|" + str(event_id) + "|" + now_iso()).encode("utf-8")).hexdigest()[:12]
    con.execute(
        "insert into assessments(id,ticket_id,source,event_id,direction,"
        "intent_evidence,rule_assessments,decision,created_at) "
        "values(?,?,?,?,?,?,?,?,?)",
        (aid, ticket_id or "", source or "", event_id or "",
         str((assessment or {}).get("direction") or ""),
         str((assessment or {}).get("evidence") or
             (assessment or {}).get("intent_evidence") or ""),
         json.dumps((assessment or {}).get("rules") or
                    (assessment or {}).get("rule_assessments") or [],
                    ensure_ascii=False),
         decision or "", now_iso()))
    return aid


def ticket_intake(source, actor, title, body="", event_id="", scope="",
                  project="", run_id="", workspace="", addressed=True,
                  assessment=None, con=None):
    if event_id:
        own = con is None
        c = con or connect()
        try:
            old = c.execute(
                "select * from tickets where source=? and source_event_id=?",
                (source, event_id)).fetchone()
            if old:
                m = mandate_get(old["id"], con=c)
                return {"ok": True, "created": False, "ticket": dict(old),
                        "strength": (m or {}).get("strength", ""),
                        "reason": "duplicate"}
        finally:
            if own:
                c.close()
    p = policy_load()
    ok, strength, reason = policy_gate(source, actor, addressed, p=p)
    if not ok:
        return {"ok": False, "reason": reason, "strength": strength}
    if strength == "inferred" and p.get("mode") == "observe":
        return {"ok": True, "would_accept": True, "strength": strength,
                "reason": reason, "policy_version": policy_version(p)}
    rules = [r.get("id") for r in (p.get("rules") or [])
             if r.get("enabled", True) and r.get("id")]
    if strength == "inferred" and p.get("mode") == "auto":
        if not isinstance(assessment, dict):
            return {"ok": False, "reason": "needs_assessment",
                    "strength": strength, "policy_version": policy_version(p)}
        direction = str(assessment.get("direction") or "")
        evidence = str(assessment.get("evidence") or
                       assessment.get("intent_evidence") or "").strip()
        got = assessment.get("rules") or assessment.get("rule_assessments") or []
        matched = [r.get("id") for r in got
                   if isinstance(r, dict) and r.get("matched") and r.get("id")
                   and r.get("id") in rules]
        if direction != "implementation_requested" or not evidence or not matched:
            return {"ok": False, "reason": "assessment_rejected",
                    "strength": strength, "policy_version": policy_version(p)}
        rules = sorted(set(matched))
    t, created = ticket_create(
        source=source, title=title, requester=actor, source_event_id=event_id,
        scope=scope, project=project, run_id=run_id, workspace=workspace,
        strength=strength, matched_rules=rules if strength == "inferred" else [],
        body=body, force=True, con=con)
    if assessment or strength == "inferred":
        own = con is None
        c2 = con or connect()
        try:
            _record_assessment(c2, t["id"], source, event_id, assessment,
                               "accepted" if created else "duplicate")
        finally:
            if own:
                c2.close()
    return {"ok": True, "created": created, "ticket": t, "strength": strength,
            "reason": reason, "policy_version": policy_version(p)}


def source_event_get(source, event_id, con=None):
    own = con is None
    con = con or connect()
    try:
        r = con.execute(
            "select * from source_events where source=? and event_id=?",
            (source, event_id)).fetchone()
        return dict(r) if r else None
    finally:
        if own:
            con.close()


def source_ingest(source, events, actor="user", addressed_default=True, con=None):
    """通用来源入口：接收一批 Source 事件，交给 gate/intake，并记录去向。"""
    own = con is None
    con = con or connect()
    counts = {"stored": 0, "duplicate": 0, "ticket": 0, "would_accept": 0,
              "ignored": 0, "needs_assessment": 0, "invalid": 0}
    try:
        for ev in events or []:
            if not isinstance(ev, dict):
                counts["invalid"] += 1
                continue
            eid = str(ev.get("event_id") or ev.get("id") or "").strip()
            if not eid:
                counts["invalid"] += 1
                continue
            if source_event_get(source, eid, con=con):
                counts["duplicate"] += 1
                continue
            title = str(ev.get("title") or ev.get("text") or eid)[:200]
            body = str(ev.get("body") or ev.get("text") or "")
            addressed = bool(ev.get("addressed", addressed_default))
            now = now_iso()
            con.execute(
                "insert into source_events(source,event_id,actor,title,body,"
                "addressed,disposition,created_at) values(?,?,?,?,?,?,'new',?)",
                (source, eid, str(ev.get("actor") or actor or "user"), title,
                 body, 1 if addressed else 0, now))
            try:
                r = ticket_intake(
                    source, str(ev.get("actor") or actor or "user"), title,
                    body=body, event_id=eid, scope=str(ev.get("scope") or ""),
                    project=str(ev.get("project") or ""),
                    run_id=str(ev.get("run_id") or ""),
                    workspace=str(ev.get("workspace") or ""),
                    addressed=addressed, assessment=ev.get("assessment"),
                    con=con)
            except SystemExit as e:
                r = {"ok": False, "reason": str(e)}
            if r.get("ok") and r.get("created"):
                disp = "ticket"
                counts["ticket"] += 1
            elif r.get("ok") and r.get("would_accept"):
                disp = "would_accept"
                counts["would_accept"] += 1
            elif not r.get("ok") and r.get("reason") in ("needs_assessment",
                                                         "assessment_rejected"):
                disp = "needs_assessment"
                counts["needs_assessment"] += 1
            else:
                disp = "ignored"
                counts["ignored"] += 1
            counts["stored"] += 1
            con.execute(
                "update source_events set disposition=?, ticket_id=?, reason=?"
                " where source=? and event_id=?",
                (disp, (r.get("ticket") or {}).get("id", ""),
                 str(r.get("reason") or ""), source, eid))
        return counts
    finally:
        if own:
            con.close()


def ticket_report(tid, state, detail="", question="", con=None):
    if state not in ("progress", "waiting", "ready", "blocked", "failed", "dropped"):
        raise SystemExit(f"未知 report 状态：{state}")
    own = con is None
    con = con or connect()
    try:
        t = con.execute("select * from tickets where id=?", (tid,)).fetchone()
        if not t:
            raise SystemExit(f"工单不存在：{tid}")
        now = _ticket_now()
        attention = question or (detail if state in ("blocked", "failed") else "")
        attention_kind = ("question" if question else state) \
            if state in ("blocked", "failed") else ""
        ended = now if state == "dropped" else ""
        outcome = "dropped" if state == "dropped" else t["outcome"]
        con.execute(
            "update tickets set latest_report=?, latest_report_at=?, attention=?,"
            " attention_kind=?, ended_at=?, outcome=?, updated_at=? where id=?",
            (state, now, attention, attention_kind, ended, outcome or "", now, tid))
        con.execute(
            "update assignments set status='ended', report_state=?, report_text=?,"
            " reported_at=?, ended_at=? where ticket_id=? and status in ('pending','running')",
            (state, detail or "", now, now, tid))
        t = ticket_refresh(tid, con=con)
        if state in ("ready", "blocked", "failed"):
            effect_enqueue(
                "notice", f"ticket:{tid}:{state}:{int(time.time() * 1000)}",
                {"ticket_id": tid, "stage": t["stage"], "title": t["title"],
                 "text": detail or question}, con=con)
            effect_run("notice", limit=5, con=con)
        return t
    finally:
        if own:
            con.close()


def ticket_answer(tid, text, kind="answer", human=True, con=None):
    text = (text or "").strip()
    if not text:
        raise SystemExit("输入内容为空")
    own = con is None
    con = con or connect()
    try:
        t = con.execute("select * from tickets where id=?", (tid,)).fetchone()
        if not t:
            raise SystemExit(f"工单不存在：{tid}")
        if t["ended_at"]:
            if t["outcome"] == "stopped":
                raise SystemExit("这条工单已被人工停止；需要新请求")
            if not ticket_revalidate(tid, con=con):
                raise SystemExit("授权已失效，不能恢复这条工单")
            con.execute("update tickets set ended_at='', outcome='', updated_at=? "
                        "where id=?", (now_iso(), tid))
        now = _ticket_now()
        con.execute(
            "insert into ticket_inputs(ticket_id,kind,body,human,created_at)"
            " values(?,?,?,?,?)",
            (tid, kind, text, 1 if human else 0, now))
        con.execute(
            "update tickets set latest_input_at=?, attention='', attention_kind='',"
            " updated_at=? where id=?", (now, now, tid))
        return ticket_refresh(tid, con=con)
    finally:
        if own:
            con.close()


def ticket_retry(tid, con=None):
    own = con is None
    con = con or connect()
    try:
        t = con.execute("select * from tickets where id=?", (tid,)).fetchone()
        if not t:
            raise SystemExit(f"工单不存在：{tid}")
        if t["ended_at"]:
            raise SystemExit("工单已经结束，先 answer 恢复再 retry")
        now = _ticket_now()
        con.execute(
            "update tickets set latest_report='', attention='', attention_kind='',"
            " updated_at=? where id=?", (now, tid))
        if t["run_id"]:
            _link_run_con(con, tid, t["run_id"], "workflow")
        return ticket_refresh(tid, con=con)
    finally:
        if own:
            con.close()


def ticket_stop(tid, reason="", con=None):
    own = con is None
    con = con or connect()
    try:
        t = con.execute("select * from tickets where id=?", (tid,)).fetchone()
        if not t:
            raise SystemExit(f"工单不存在：{tid}")
        now = _ticket_now()
        con.execute(
            "update tickets set ended_at=?, outcome='stopped', stage='ended',"
            " attention=?, attention_kind='', updated_at=? where id=?",
            (now, reason or "", now, tid))
        con.execute(
            "update assignments set status='ended', ended_at=?, error=? "
            "where ticket_id=? and status in ('pending','running')",
            (now, reason or "stopped by person", tid))
        return ticket_get(tid, con=con)
    finally:
        if own:
            con.close()


def ticket_revalidate(tid, con=None):
    t = ticket_get(tid, con=con)
    if not t:
        return False
    m = mandate_get(tid, con=con)
    if not m:
        return False
    p = policy_load()
    addressed = m.get("strength") == "explicit"
    ok, strength, _ = policy_gate(t["source"], m.get("actor") or "",
                                  addressed=addressed, p=p)
    if not ok:
        return False
    if m.get("strength") == "inferred":
        if p.get("mode") == "off":
            return False
        enabled = {r.get("id") for r in (p.get("rules") or [])
                   if r.get("enabled", True)}
        for rid in filter(None, (m.get("matched_rules") or "").split(",")):
            if rid not in enabled:
                return False
    return True


def ticket_list(stage=None, open_only=False, limit=50, con=None):
    own = con is None
    con = con or connect()
    try:
        sql = "select * from tickets where 1=1"
        args = []
        if stage:
            if stage.endswith("%"):
                sql += " and stage like ?"
            else:
                sql += " and stage=?"
            args.append(stage)
        if open_only:
            sql += " and ended_at=''"
        sql += " order by priority desc, created_at desc limit ?"
        args.append(int(limit or 50))
        return [dict(r) for r in con.execute(sql, args)]
    finally:
        if own:
            con.close()


def ticket_inbox(limit=50, con=None):
    return ticket_list(stage="person_%", open_only=True, limit=limit, con=con)


def ticket_attach_assignment(tid, executor, code, output="", error="",
                             run_id="", workspace="", pending=False, con=None):
    """把一个直接派工的结果落到工单：成功 ready，超时 blocked，其他 failed。

    pending=True 用于桥接 --no-wait：只代表投递成功，还没干完，工单保持 working。
    """
    own = con is None
    con = con or connect()
    try:
        t = con.execute("select * from tickets where id=?", (tid,)).fetchone()
        if not t:
            raise SystemExit(f"工单不存在：{tid}")
        now = _ticket_now()
        aid = "as-" + hashlib.sha1(
            (tid + "|" + str(executor) + "|" + now).encode("utf-8")).hexdigest()[:12]
        if pending:
            status, state, detail, ended = "running", "progress", output or "", ""
        else:
            status = "ended"
            state = "ready" if code == 0 else ("blocked" if code == 124 else "failed")
            detail = (output if code == 0 else error) or ""
            ended = now
        con.execute(
            "insert into assignments(id,ticket_id,executor,status,attempt,workspace,"
            "run_id,report_state,report_text,reported_at,started_at,ended_at,error)"
            " values(?,?,?,?,1,?,?,?,?,?,?,?,?)",
            (aid, tid, executor or "", status, workspace or "", run_id or "", state,
             detail[:2000], now, now, ended, error or ""))
        if run_id:
            con.execute("update tickets set run_id=?, updated_at=? where id=?",
                        (run_id, now, tid))
        if workspace:
            con.execute("update tickets set workspace=?, updated_at=? where id=?",
                        (workspace, now, tid))
        if pending:
            con.execute(
                "update tickets set latest_report='progress', latest_report_at=?,"
                " attention='', attention_kind='', updated_at=? where id=?",
                (now, now, tid))
            return ticket_refresh(tid, con=con)
        return ticket_report(tid, state, detail=detail, con=con)
    finally:
        if own:
            con.close()


def ticket_report_run(tid, code, run_id, detail="", con=None):
    if code == 0:
        state = "ready"
        detail = detail or f"workflow run {run_id} 完成，等待人工验收"
    elif code == 2:
        state = "waiting"
        detail = detail or f"workflow run {run_id} 被 STOP"
    else:
        state = "failed"
        detail = detail or f"workflow run {run_id} 失败"
    return ticket_report(tid, state, detail=detail, con=con)


def ticket_print(t, con=None, with_details=True):
    if not t:
        print("（工单不存在）")
        return
    print(f"{t['id']}  [{t['stage']}]  {t['title']}")
    print(f"  来源 {t['source']} · 请求人 {t['requester'] or '-'} · 项目 {t['project'] or '-'}"
          + (f" · run {t['run_id']}" if t.get("run_id") else ""))
    if t.get("scope"):
        print(f"  范围 {t['scope']}")
    if t.get("workspace"):
        print(f"  工作目录 {t['workspace']}")
    if t.get("attention"):
        print(f"  待人工（{t['attention_kind'] or 'attention'}）：{t['attention']}")
    if t.get("latest_report"):
        print(f"  最近报告 {t['latest_report']} · {t['latest_report_at']}")
    if not with_details:
        return
    m = mandate_get(t["id"], con=con)
    if m:
        print(f"  授权 {m['strength']} · policy {m['policy_version']}"
              + (f" · 规则 {m['matched_rules']}" if m["matched_rules"] else ""))
    for a in ticket_assignments(t["id"], con=con):
        print(f"  派工 {a['executor']:<12} {a['status']:<8}"
              + (f" run={a['run_id']}" if a.get("run_id") else "")
              + (f" workspace={a['workspace']}" if a.get("workspace") else "")
              + (f"  {a['error'][:80]}" if a.get("error") else ""))
    for i in ticket_input_rows(t["id"], con=con):
        print(f"  输入 #{i['id']} {i['kind']}: {i['body'][:100]}")


# ------------------------------------------------ P2 持久化 effects / outbox ----

class EffectLater(Exception):
    """现在不能执行（暂停/没 slot），重排但不计失败。"""


OUTBOX_DIR = APP_DIR / "outbox"
EFFECT_HANDLERS = {}
EFFECT_META = {}


def effect_handle(kind, fn, retry=(2.0, 60.0), ordered=False, quiet=False):
    EFFECT_HANDLERS[kind] = fn
    EFFECT_META[kind] = {"retry": retry, "ordered": ordered, "quiet": quiet}


def effect_enqueue(kind, key, payload, con=None):
    own = con is None
    con = con or connect()
    try:
        now = now_iso()
        row = con.execute("select coalesce(max(seq),0) as m from effects").fetchone()
        seq = int(row["m"] if row else 0) + 1
        con.execute(
            "insert into effects(kind,key,payload,seq,due,attempts,error,generation,"
            "created_at,updated_at) values(?,?,?,?,?,0,'',1,?,?) "
            "on conflict(kind,key) do update set payload=excluded.payload,"
            " generation=generation+1, attempts=0, error='', due=excluded.due,"
            " updated_at=excluded.updated_at",
            (kind, key, json.dumps(payload or {}, ensure_ascii=False), seq,
             time.time(), now, now))
    finally:
        if own:
            con.close()


def effect_rows(kind=None, con=None):
    own = con is None
    con = con or connect()
    try:
        sql = "select * from effects"
        args = []
        if kind:
            sql += " where kind=?"
            args.append(kind)
        sql += " order by due, seq"
        return [dict(r) for r in con.execute(sql, args)]
    finally:
        if own:
            con.close()


def _health_set(kind, state, detail="", con=None):
    own = con is None
    con = con or connect()
    try:
        con.execute(
            "insert into health(kind,state,detail,updated_at) values(?,?,?,?) "
            "on conflict(kind) do update set state=excluded.state,"
            " detail=excluded.detail, updated_at=excluded.updated_at",
            (kind, state, (detail or "")[:500], now_iso()))
    finally:
        if own:
            con.close()


def receipt_create(rid, ticket_id="", channel="", payload=None, con=None):
    own = con is None
    con = con or connect()
    try:
        now = now_iso()
        con.execute(
            "insert or ignore into receipts(id,ticket_id,channel,status,payload,"
            "created_at,updated_at) values(?,?,?,'pending',?,?,?)",
            (rid, ticket_id or "", channel or "",
             json.dumps(payload or {}, ensure_ascii=False), now, now))
    finally:
        if own:
            con.close()


def receipt_get(rid, con=None):
    own = con is None
    con = con or connect()
    try:
        r = con.execute("select * from receipts where id=?", (rid,)).fetchone()
        return dict(r) if r else None
    finally:
        if own:
            con.close()


def receipt_settle(rid, status, error="", con=None):
    own = con is None
    con = con or connect()
    try:
        now = now_iso()
        con.execute(
            "update receipts set status=?, error=?, sent_at=?, updated_at=? where id=?",
            (status, error or "", now if status == "sent" else "", now, rid))
    finally:
        if own:
            con.close()


def _effect_notice(key, payload):
    OUTBOX_DIR.mkdir(parents=True, exist_ok=True)
    safe = re.sub(r"[^A-Za-z0-9_.-]+", "_", key)[:90] or "notice"
    if safe != key:
        safe += "-" + hashlib.sha1(key.encode("utf-8")).hexdigest()[:8]
    path = OUTBOX_DIR / f"{safe}.json"
    path.write_text(json.dumps({"key": key, "at": now_iso(), **(payload or {})},
                               ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    receipt_settle(key, "sent")


def _effect_bridge(key, payload):
    inbox = Path(payload["inbox"])
    inbox.mkdir(parents=True, exist_ok=True)
    tid = payload.get("tid") or key
    envelope = payload.get("envelope") or {}
    (inbox / f"task_{tid}.json").write_text(
        json.dumps(envelope, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    (inbox / f"task_{tid}.md").write_text(payload.get("prompt") or "", encoding="utf-8")
    receipt_settle(key, "sent")


effect_handle("notice", _effect_notice, retry=(2.0, 60.0), ordered=True)
effect_handle("bridge", _effect_bridge, retry=(5.0, 300.0), ordered=False)


def effect_run(kind=None, limit=20, con=None):
    """处理到期副作用；成功的删除，失败的退避，Later 只延后不计失败。"""
    own = con is None
    con = con or connect()
    try:
        now = time.time()
        sql = "select * from effects where due<=?"
        args = [now]
        if kind:
            sql += " and kind=?"
            args.append(kind)
        sql += " order by seq limit ?"
        args.append(int(limit or 20))
        rows = con.execute(sql, args).fetchall()
        done, seen = 0, set()
        for row in rows:
            k = row["kind"]
            if k in seen and EFFECT_META.get(k, {}).get("ordered"):
                continue
            seen.add(k)
            handler = EFFECT_HANDLERS.get(k)
            if not handler:
                continue
            retry = EFFECT_META.get(k, {}).get("retry", (2.0, 60.0))
            try:
                handler(row["key"], json.loads(row["payload"] or "{}"))
            except EffectLater:
                con.execute("update effects set due=?, updated_at=? where kind=? and key=?",
                            (now + retry[0], now_iso(), k, row["key"]))
            except Exception as e:
                attempts = int(row["attempts"] or 0) + 1
                delay = min(retry[1], retry[0] * (2 ** min(attempts, 16)))
                con.execute(
                    "update effects set attempts=?, due=?, error=?, updated_at=?"
                    " where kind=? and key=?",
                    (attempts, now + delay, str(e)[:1000], now_iso(), k, row["key"]))
                _health_set(k, "error" if attempts >= 3 else "ready",
                            f"{attempts} 次失败: {e}", con=con)
            else:
                con.execute("delete from effects where kind=? and key=?", (k, row["key"]))
                _health_set(k, "ready", "ok", con=con)
                done += 1
        return done
    finally:
        if own:
            con.close()


def effect_pending(kind=None, con=None):
    return len(effect_rows(kind, con=con))


def wf_runs(project):
    rd = wf_project_dir(project) / "runs"
    return sorted(p for p in rd.iterdir() if p.is_dir()) if rd.exists() else []


def wf_resume_state(state):
    """续跑前清理：failed/running 的步骤回 pending 重来（重试预算重置），STOP 清掉。"""
    for st in state["steps"].values():
        if st["status"] in ("failed", "running"):
            st.update({"status": "pending", "attempts": 0, "verdict": "", "exit_code": None,
                       "dur": 0.0, "output_file": "", "error": "", "ts": "",
                       "started_at": "", "ended_at": ""})
    state["status"] = "running"


# ------------------------------------------------------------- 调度引擎 ----

VERDICT_RE = re.compile(r"\bverdict\s*[:：]\s*(pass|fail)\b", re.I)
WF_OUT_SNIPPET = 4000     # 上游产出注入下游提示词的截断长度


def wf_extract_verdict(out):
    ms = VERDICT_RE.findall(out or "")
    return ms[-1].lower() if ms else ""


def wf_eval_cond(state, cond):
    ref, op, want = cond
    got = state["steps"][ref].get("verdict") or "pass"   # 没写 verdict 当 pass
    return (got == want) if op == "==" else (got != want)


def wf_reset_from(state, by_id, target):
    """把 target 及其全部下游打回 pending（清结果；loops 计数保留）。"""
    kids = {}
    for sid, s in by_id.items():
        for d in s["depends_on"]:
            kids.setdefault(d, []).append(sid)
    doomed, stack = set(), [target]
    while stack:
        x = stack.pop()
        if x in doomed:
            continue
        doomed.add(x)
        stack.extend(kids.get(x, []))
    for sid in doomed:
        state["steps"][sid] = {"status": "pending", "attempts": 0, "verdict": "",
                               "exit_code": None, "dur": 0.0, "output_file": "",
                               "error": "", "ts": "", "started_at": "",
                               "ended_at": ""}


def wf_step_prompt(flow, s, st, state):
    """单步骤提示词：项目上下文 + 上游产出 + 打回意见 + 任务本体。"""
    parts = [f"<workflow>\n项目「{flow['name']}」· 步骤 {s['id']}（角色 {s['role']}）"
             f"· 第 {st['attempts']} 次尝试\n</workflow>"]
    ctx = [c for c in flow["context"] if c["text"]]
    if ctx:
        parts.append("<project-context>\n" + "\n\n".join(
            f"## {c['name']}\n{c['text'][:WF_OUT_SNIPPET]}" for c in ctx) + "\n</project-context>")
    ups = []
    for d in s["depends_on"]:
        dst = state["steps"][d]
        body = ""
        if dst.get("output_file") and Path(dst["output_file"]).exists():
            body = Path(dst["output_file"]).read_text(encoding="utf-8", errors="replace")
        body = (body or dst.get("output") or "").strip()
        ups.append(f"## {d}（{dst['status']}）\n{body[:WF_OUT_SNIPPET] or '（无产出）'}")
    if ups:
        parts.append("<upstream-results>\n" + "\n\n".join(ups) + "\n</upstream-results>")
    ln = state.get("loop_notes", {}).get(s["id"])
    if ln:
        parts.append(f"<loop-back>\n这一步是第 {ln['iter']} 轮返工，来自「{ln['from']}」的"
                     f"打回意见，请针对性修改：\n{str(ln['note'])[:3000]}\n</loop-back>")
    task = [s["prompt"] or s["description"]]
    if s["prompt"] and s["description"]:
        task.append(f"（本步骤目标：{s['description']}）")
    if s["input"]:
        task.append(f"输入材料：{s['input']}")
    if s["output"]:
        task.append(f"要求产出：{s['output']}")
    if s["id"] in flow["verdict_steps"]:
        task.append("引擎约定：请在输出的最后一行单独写 verdict: pass（通过/无需返工）"
                    "或 verdict: fail（不通过/需要返工），引擎据此决定是否打回返工或放行下游。")
    parts.append("<task>\n" + "\n".join(task) + "\n</task>")
    return "\n\n".join(parts)


def wf_step_dispatch(step, product, role_name, prompt, cwd, chain_base, opts, run_dir,
                     label=""):
    """单步骤派工：与 agenthub call 同一套底层（信封/转写/入库），在引擎线程里跑。
    返回 (out, err, code, dur, note)。测试会整个替换掉这个函数。"""
    con = connect()
    try:
        chain = list(chain_base) + [product]
        if product in chain_base:
            raise RuntimeError(f"循环派工：链路 {' → '.join(chain)} 里 {product} 出现两次")
        max_depth = getattr(opts, "max_depth", 0) or MAX_CHAIN_DEPTH
        if len(chain) > max_depth:
            raise RuntimeError(f"派工链路过深（>{max_depth} 层）：{' → '.join(chain)}")
        mem_q = step.get("mem") or getattr(opts, "mem", "") or ""
        mem_block = build_context(con, mem_q, n=5, project=detect_project(cwd)) if mem_q else ""
        full_prompt = build_envelope(prompt, mem_block, chain, product, role_name,
                                     contract=step.get("_contract"))
        ns = SimpleNamespace(write=getattr(opts, "write", False),
                             yolo=getattr(opts, "yolo", False),
                             model=getattr(opts, "model", None),
                             no_proxy=getattr(opts, "no_proxy", False),
                             timeout=step.get("_effective_timeout")
                             or step.get("timeout")
                             or getattr(opts, "timeout", DEFAULT_TIMEOUT),
                             mem=mem_q or None, mem_n=5, no_wait=False,
                             requires_approval=bool(step.get("approval")),
                             lease_seconds=getattr(opts, "lease_seconds", 300))
        if product in AGENTS:
            out, err, code, dur, note, _ = cli_dispatch(ns, product, cwd, full_prompt,
                                                        chain, role_name)
        else:
            out, err, code, dur, note, _ = bridge_dispatch(
                ns, product, cwd, full_prompt, chain, role_name,
                contract=step.get("_contract"))
        tpath = write_transcript(product, cwd, note, mem_q, mem_block, prompt, code, dur,
                                 out, err, chain, role_name)
        record_dispatch(con, product, cwd, prompt, mem_q, code, dur, tpath, out,
                        chain, role_name)
        tags = "agenthub,workflow" + (f",{label}" if label else "")
        insert_note(con, product, "dispatch",
                    f"工作流[{label or flow_name_of(run_dir)}] 步骤 {step['id']}"[:60],
                    tags,
                    f"工作流步骤 {step['id']} 派给 {product}"
                    + (f"（角色 {role_name}）" if role_name else "")
                    + f"（cwd={cwd}）。\n任务: {prompt}\n结果摘要: {out.strip()[:1500] or '(无输出)'}")
        return out, err, code, dur, note
    finally:
        con.close()


def flow_name_of(run_dir):
    st = wf_load_state(run_dir)
    return (st or {}).get("project") or "?"


def wf_step_thread(s, product, role_name, prompt, cwd, chain_base, opts, run_dir,
                   label, results):
    """线程壳：跑一步派工，把异常收成这一步的失败，不炸整个引擎。

    这里不再用 redirect_stdout：它改的是进程级 sys.stdout，两个步骤并行时会互相
    抢缓冲区，后结束的线程可能把真 stdout 留在 StringIO 里，后续日志整段消失。
    子进程的 stdout/stderr 已经由 run_dispatch 收进 out/err，落盘信息不会丢。
    """
    t0 = time.time()
    try:
        if s.get("tool"):
            out, err, code, dur, note = wf_step_tool(
                s, prompt, cwd, opts, run_dir, label)
        else:
            out, err, code, dur, note = wf_step_dispatch(
                s, product, role_name, prompt, cwd, chain_base, opts, run_dir, label)
    except Exception as e:      # 派工目标没装好等：算这步失败，不炸整个引擎
        out, err, code, dur, note = "", f"{type(e).__name__}: {e}", 1, time.time() - t0, ""
    results[s["id"]] = {"out": out, "err": err, "code": code,
                        "dur": dur or time.time() - t0, "note": note,
                        "log": "（子 Agent 控制台输出已并入 stdout/stderr 落盘）"}


def wf_loop_trigger(state, s, st):
    """打回触发：本步骤自己写了 verdict: fail，或它是被 condition 放进来的（上游判了 fail）。"""
    if st.get("verdict") == "fail":
        return True
    return bool(s["condition"] and wf_eval_cond(state, s["condition"]))


def wf_run(flow, project, run_dir, state, opts):
    """调度主循环：事件日志 + 拓扑波次并行 + 重试 + 打回循环 + STOP + 预算闸。"""
    run_dir = Path(run_dir)
    by_id, stop_path = flow["by_id"], run_dir / "STOP"
    workdir = Path(state["workdir"])
    chain_base = chain_from_env() + [f"wf:{project}"]
    budget = dict(flow.get("budget") or {})
    max_parallel = _num(budget.get("max_parallel"), int, 8) or 8
    run_started = time.time()

    def log(m):
        print(f"[wf] {m}")

    def event(kind, step_id="", payload=None):
        try:
            wf_event(run_dir, kind, step_id=step_id, payload=payload or {})
        except Exception as e:
            log(f"⚠ 事件记录失败：{type(e).__name__}: {e}")

    def checkpoint(step_id, attempt, status, prompt="", output_path="", error="",
                   started_at="", ended_at=""):
        try:
            wf_checkpoint_save(run_dir, step_id, attempt, status, prompt=prompt,
                               output_path=output_path, error=error,
                               started_at=started_at, ended_at=ended_at)
        except Exception as e:
            log(f"⚠ 检查点记录失败：{type(e).__name__}: {e}")

    try:
        wf_budget_new(state["run_id"], budget, started_at=now_iso())
    except Exception as e:
        log(f"⚠ 预算记录初始化失败：{type(e).__name__}: {e}")
    event("run_started", payload={"workflow": flow["name"], "project": project,
                                  "budget": budget, "workdir": str(workdir)})
    log(f"开始「{flow['name']}」（run {state['run_id']}，{len(by_id)} 步，工作目录 {workdir}）")
    for w in flow["warnings"]:
        log(f"⚠ {w}")
    code = 0
    budget_hit = False
    while True:
        if stop_path.exists():
            state["status"] = "stopped"
            event("run_stop_requested", payload={"run_id": state["run_id"]})
            log("发现 STOP 标记，不再派新步骤（正在跑的已等完）")
            break
        ready = []
        for s in flow["steps"]:
            st = state["steps"][s["id"]]
            if st["status"] != "pending":
                continue
            if not all(state["steps"][d]["status"] in ("success", "skipped")
                       for d in s["depends_on"]):
                continue
            if s["condition"] and not wf_eval_cond(state, s["condition"]):
                st["status"] = "skipped"
                st["error"] = "条件不满足：" + " ".join(s["condition"])
                log(f"- {s['id']} 跳过（{st['error']}）")
                event("step_skipped", step_id=s["id"], payload={"reason": st["error"]})
                wf_save_state(run_dir, state)
                continue
            ready.append(s)
        if not ready:
            break
        for group in wf_schedule_groups(ready, max_parallel):
            exceeded, why = wf_budget_exceeded(state["run_id"], started_at=run_started)
            if exceeded:
                state["status"] = "failed"
                state["error"] = why
                event("budget_exceeded", payload={"reason": why})
                log(f"✗ {why}，不再派新步骤")
                budget_hit = True
                break
            state["wave"] += 1
            log(f"第 {state['wave']} 波（并行 {len(group)}）："
                + "、".join(s["id"] for s in group))
            remaining = None
            if budget.get("seconds"):
                try:
                    remaining = max(1, int(float(budget["seconds"])
                                           - (time.time() - run_started)))
                except (TypeError, ValueError):
                    remaining = None
            prompts, threads, results = {}, [], {}
            for s in group:
                st = state["steps"][s["id"]]
                st["status"], st["ts"] = "running", now_iso()
                st["started_at"], st["ended_at"] = st["ts"], ""
                st["attempts"] += 1
                s["_contract"] = wf_contract(flow, s)
                if remaining:
                    base_to = s.get("timeout") or getattr(opts, "timeout", DEFAULT_TIMEOUT)
                    s["_effective_timeout"] = max(1, min(int(base_to), int(remaining)))
                prompts[s["id"]] = wf_step_prompt(flow, s, st, state)
                checkpoint(s["id"], st["attempts"], "running", prompt=prompts[s["id"]],
                           started_at=st["started_at"])
                event("step_started", step_id=s["id"],
                      payload={"agent": s["product"], "role": s["role_name"],
                               "mode": s.get("mode", "tool"),
                               "attempt": st["attempts"],
                               "trace_id": state.get("trace_id", state["run_id"])})
                threads.append(threading.Thread(
                    target=wf_step_thread,
                    args=(s, s["product"], s["role_name"], prompts[s["id"]], workdir,
                          chain_base, opts, run_dir, project, results),
                    daemon=True))
            wf_save_state(run_dir, state)
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            for s in group:
                sid, st = s["id"], state["steps"][s["id"]]
                r = results.get(sid)
                sdir = run_dir / "steps" / sid
                sdir.mkdir(parents=True, exist_ok=True)
                k = st["attempts"]
                (sdir / f"attempt{k}.prompt.md").write_text(prompts[sid], encoding="utf-8")
                outcome = "failed"
                if r is None:
                    st["status"], st["error"] = "failed", "线程异常（没有结果）"
                    st["ended_at"] = now_iso()
                    checkpoint(sid, k, "failed", prompt=prompts[sid], error=st["error"],
                               started_at=st.get("started_at", ""), ended_at=st["ended_at"])
                    event("step_finished", step_id=sid,
                          payload={"status": "failed", "error": st["error"],
                                   "attempt": k, "exit_code": None})
                    continue
                (sdir / f"attempt{k}.out.md").write_text(r["out"] or "", encoding="utf-8")
                (sdir / f"attempt{k}.log.md").write_text(
                    f"# {sid} · 第 {k} 次尝试\n\n- 退出码 {r['code']} · 用时 {r['dur']:.0f}s · {r['note']}\n\n"
                    f"## 引擎与子 Agent 控制台输出\n\n```\n{r['log'].strip() or '（无）'}\n```\n\n"
                    f"## stderr\n\n```\n{(r['err'] or '').strip()[-2000:] or '（无）'}\n```\n",
                    encoding="utf-8")
                st["exit_code"], st["dur"] = r["code"], round(r["dur"], 1)
                tokens, cost = wf_extract_usage(r["out"])
                if tokens or cost:
                    try:
                        wf_budget_add_usage(state["run_id"], tokens=tokens, cost=cost)
                    except Exception as e:
                        log(f"⚠ 预算累计失败：{type(e).__name__}: {e}")
                if r["code"] == 0:
                    st["verdict"] = wf_extract_verdict(r["out"])
                    st["status"] = "success"
                    st["output_file"] = str(sdir / f"attempt{k}.out.md")
                    outcome = "success"
                    log(f"✓ {sid} 完成（{r['dur']:.0f}s"
                        + (f"，verdict: {st['verdict']}" if st["verdict"] else "") + "）")
                    if s["loop_back_to"] and wf_loop_trigger(state, s, st):
                        n = state["loops"].get(sid, 0)
                        if n >= s["max_loops"]:
                            st["status"] = "failed"
                            st["error"] = f"打回循环超过 max_loops={s['max_loops']}（问题始终没解决）"
                            outcome = "failed"
                            log(f"✗ {sid} {st['error']}")
                        else:
                            state["loops"][sid] = n + 1
                            state.setdefault("loop_notes", {})[s["loop_back_to"]] = {
                                "from": sid, "iter": n + 1, "note": (r["out"] or "")[:3000]}
                            wf_reset_from(state, by_id, s["loop_back_to"])
                            outcome = "loop_back"
                            event("step_loop_back", step_id=sid,
                                  payload={"to": s["loop_back_to"], "iter": n + 1,
                                           "max_loops": s["max_loops"]})
                            log(f"↺ {sid} 打回 {s['loop_back_to']}"
                                f"（第 {n + 1}/{s['max_loops']} 次）")
                elif st["attempts"] <= s["retry"]:
                    st["status"] = "pending"
                    outcome = "retry"
                    log(f"… {sid} 失败（退出码 {r['code']}），重试 {st['attempts']}/{s['retry']}")
                else:
                    st["status"] = "failed"
                    st["error"] = (f"退出码 {r['code']}："
                                   f"{(r['err'] or r['out'] or '').strip()[-300:]}")
                    outcome = "failed"
                    log(f"✗ {sid} 失败（退出码 {r['code']}，已试 {st['attempts']} 次）")
                st["ended_at"] = now_iso()
                checkpoint(sid, k, outcome, prompt=prompts[sid],
                           output_path=st.get("output_file", ""),
                           error=st.get("error", ""),
                           started_at=st.get("started_at", ""),
                           ended_at=st["ended_at"])
                event("step_finished", step_id=sid,
                      payload={"status": outcome, "exit_code": st["exit_code"],
                               "dur": st["dur"], "attempt": k,
                               "tokens": tokens, "cost": cost,
                               "output": st.get("output_file", ""),
                               "error": st.get("error", "")})
            wf_save_state(run_dir, state)
        if budget_hit:
            break

    statuses = [st["status"] for st in state["steps"].values()]
    if state["status"] == "stopped":
        code = 2
    elif "failed" in statuses:
        state["status"] = "failed"
        code = 1
    elif all(x in ("success", "skipped") for x in statuses):
        state["status"] = "done"
    else:
        state["status"] = "failed"
        code = 1
        log("有步骤因上游失败而无法执行，工作流未完成")
    if state["status"] in ("done", "failed") and stop_path.exists():
        stop_path.unlink()
    tally = {}
    for st in state["steps"].values():
        tally[st["status"]] = tally.get(st["status"], 0) + 1
    log("结束：" + state["status"] + "（"
        + "，".join(f"{k} {v}" for k, v in sorted(tally.items())) + "）")
    if state["status"] == "stopped":
        log("agenthub workflow run <文件> --resume 可从断点继续")
    event("run_finished", payload={"status": state["status"], "code": code,
                                   "tally": tally})
    wf_save_state(run_dir, state)
    return code


# ------------------------------------------------------------ 模板与 CLI ----

WF_TEMPLATE = '''# AgentHub 工作流 —— 软件开发示例（含「测试 ⇄ 修复」闭环）
# 用法：改好 team 里的 Agent 名字（agenthub agents 看本机可派工名单），
# 然后 agenthub workflow run 这份文件 --dry-run 先看计划，再去掉 --dry-run 真跑。
name: "软件开发示例"
version: "1.0"
# project_dir: "D:/某个项目目录"     # 可选：不填就用运行时的当前目录

# 运行预算：超时是硬闸，token/成本靠子 Agent 输出里的 usage 尽力统计
budget:
  seconds: 3600
  max_parallel: 4
  max_depth: 5

# 契约严格模式：打开后，缺 objective / output_schema / acceptance 的步骤直接拒派。
# 也可以只在命令行用 --strict-contract 临时打开。
# contract: strict

# 项目上下文：每个步骤的提示词都会带上（file 相对本文件目录，与 content 二选一）
project_context:
  - name: "项目说明"
    content: |
      （在这里写项目背景、技术栈、目录结构、验收标准——所有步骤都能看到）

# 团队：角色 → 用哪个 Agent 干活（同一个 Agent 可以兼任多个角色）
team:
  pm: "claude"
  researcher: "workbuddy"
  developer: "codex"
  tester: "codex"

steps:
  - id: requirements
    role: pm
    description: "把项目说明拆成可执行的开发任务清单，标出难点、风险和验收标准"
    output: "任务清单（Markdown）"
    acceptance: ["清单里每个任务都能独立执行", "难点、风险和验收标准都写出来"]

  - id: research
    role: researcher
    depends_on: requirements
    description: "调研任务清单里的技术难点，给出选型建议和理由"
    input: "任务清单"
    output: "技术调研报告"
    acceptance: ["每个技术难点都有选型建议和理由", "给出可核验的来源"]

  - id: design
    role: developer
    depends_on: requirements
    description: "写实现方案：模块划分、接口约定、要改哪些文件"
    input: "任务清单"
    output: "实现方案"
    acceptance: ["模块划分、接口约定、要改的文件都写清楚"]

  - id: review
    role: pm
    depends_on: [research, design]
    description: "评审技术调研报告和实现方案，只留必须修改的问题"
    output: "评审意见"
    acceptance: ["每条意见都能直接执行", "不堆无关的风格建议"]
    loop_back_to: design        # 评审 verdict: fail 时打回 design 重做
    max_loops: 2

  - id: implement
    role: developer
    depends_on: review
    description: "按评审通过的方案实现（在项目工作目录里直接改文件），能跑测试的要跑通"
    input: "实现方案 + 评审意见"
    output: "代码改动 + 测试结果"
    acceptance: ["按方案实现", "能跑的测试都跑通", "列出改过的文件"]

  - id: test
    role: tester
    depends_on: implement
    description: "验收实现：在项目工作目录里跑测试/构建，或对照实现方案逐条检查，列出所有问题"
    output: "测试报告"
    acceptance: ["每个问题都能复现", "报告里带命令或证据"]
    # test 的 verdict 是引擎的判定依据：pass = 验收通过；fail = 有 bug

  - id: fix
    role: developer
    depends_on: test
    condition: "test.verdict == fail"   # 测试没过才要修
    description: "按测试报告修复所有问题，修完输出修复说明"
    input: "测试报告"
    output: "修复说明 + 复测结果"
    acceptance: ["每个问题都有修复说明", "复测通过"]
    loop_back_to: test          # 修完再测，形成闭环
    max_loops: 3
'''


def wf_print_plan(flow, project, workdir):
    print(f"工作流「{flow['name']}」v{flow['version']} · 项目 {project}")
    print(f"工作目录：{workdir}")
    if flow["team"]:
        print("团队：" + "，".join(f"{r}→{p}" for r, p in flow["team"].items()))
    if flow.get("budget"):
        print("预算：" + "，".join(f"{k}={v}" for k, v in flow["budget"].items()))
    for c in flow["context"]:
        tag = f"（{len(c['text'])} 字）" if c["text"] else "（内容为空）"
        print(f"项目上下文：{c['name']}{tag}")
    print(f"步骤 {len(flow['steps'])} 个，分 {len(flow['waves'])} 波并行执行：")
    for i, w in enumerate(flow["waves"], 1):
        print(f"  波 {i}：" + "、".join(
            f"{sid}（{flow['by_id'][sid]['role']}→{flow['by_id'][sid]['product']}）"
            for sid in w))
    for sid, t, n in flow["loops"]:
        print(f"循环：{sid} 不过就打回 {t}（最多 {n} 次）")
    for s in flow["steps"]:
        if s["condition"]:
            print(f"条件：{s['id']} 仅当 {s['condition'][0]}.verdict "
                  f"{s['condition'][1]} {s['condition'][2]} 时执行")
    for w in flow["warnings"]:
        print(f"⚠ {w}")
    for w in wf_validate_contracts(flow, strict=False):
        print(f"⚠ {w}（--strict-contract 会拒派）")
    print("（--dry-run 只看计划不派工；去掉它开始真跑）")


def cmd_workflow(con, args):
    act = args.wf_act
    if act == "new":
        p = Path(args.path)
        if p.exists() and not args.force:
            raise SystemExit(f"{p} 已存在（--force 覆盖）")
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(WF_TEMPLATE, encoding="utf-8")
        print(f"已生成工作流模板 → {p}\n"
              f"改好 team 里的 Agent 名字后：agenthub workflow run {p} --dry-run")
        return
    if act == "run":
        flow = wf_load(args.file)

        def apply_overrides():
            b = dict(flow.get("budget") or {})
            for key, attr in (("seconds", "budget_seconds"),
                              ("tokens", "budget_tokens"),
                              ("max_parallel", "max_parallel"),
                              ("max_depth", "max_depth")):
                v = getattr(args, attr, None)
                if v is not None:
                    b[key] = v
            flow["budget"] = b
            args.max_depth = (_num(b.get("max_depth"), int, MAX_CHAIN_DEPTH)
                              or MAX_CHAIN_DEPTH)
            strict = bool(getattr(args, "strict_contract", False)
                          or flow.get("contract_strict"))
            wf_validate_contracts(flow, strict=strict)

        apply_overrides()
        project = args.project or flow["name"]
        workdir = Path(args.cwd or flow["project_dir"] or os.getcwd()).resolve()
        if args.dry_run:
            wf_print_plan(flow, project, workdir)
            return
        ensure_fresh(con)     # 真跑前才同步记忆；dry-run 不值得等这一下
        if not workdir.exists():
            raise SystemExit(f"项目工作目录不存在：{workdir}")
        if args.resume:
            run_dir = next((r for r in reversed(wf_runs(project))
                            if (wf_load_state(r) or {}).get("status") in ("stopped", "failed")),
                           None)
            if not run_dir:
                raise SystemExit(f"项目 {project} 没有可续跑的运行"
                                 f"（只有 stopped/failed 状态能 --resume）")
            state = wf_load_state(run_dir)
            wf_copy = run_dir / "workflow.yaml"
            if wf_copy.exists():
                flow = wf_load(wf_copy)     # 续跑用当时的 workflow 副本，保证前后一致
                apply_overrides()
            wf_resume_state(state)
            (run_dir / "STOP").unlink(missing_ok=True)
            done = sum(1 for x in state["steps"].values()
                       if x["status"] in ("success", "skipped"))
            print(f"[wf] 续跑 run {state['run_id']}（已完成 {done}/{len(flow['steps'])} 步）")
        else:
            run_id = dt.datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + os.urandom(2).hex()
            run_dir = wf_project_dir(project) / "runs" / run_id
            state = wf_new_state(flow, project, run_id, workdir)
        (run_dir / "steps").mkdir(parents=True, exist_ok=True)
        (run_dir / "workflow.yaml").write_text(flow["workflow_text"], encoding="utf-8")
        if getattr(args, "ticket", None):
            ticket_link_run(args.ticket, state["run_id"], "workflow",
                            workspace=str(workdir))
            print(f"[wf] 关联工单 {args.ticket}")
        wf_save_state(run_dir, state)
        code = wf_run(flow, project, run_dir, state, args)
        if getattr(args, "ticket", None):
            t = ticket_report_run(args.ticket, code, state["run_id"])
            print(f"[wf] 工单 {args.ticket} → {t['stage']}")
        sys.exit(code)
    if act == "list":
        wf_cmd_list(args)
        return
    if act == "status":
        wf_cmd_status(args)
        return
    if act == "trace":
        runs = wf_runs(args.project)
        run_dir = wf_project_dir(args.project) / "runs" / args.run if args.run \
            else (runs[-1] if runs else None)
        if not run_dir or not run_dir.exists():
            raise SystemExit(f"项目 {args.project} 没有对应的运行"
                             f"（agenthub workflow list 看看）")
        if args.json:
            print(json.dumps(wf_event_list(run_dir.name), ensure_ascii=False, indent=2))
        else:
            print(wf_trace_text(run_dir.name))
        return
    if act == "stop":
        wf_cmd_stop(args)
        return


def wf_cmd_list(args):
    if not PROJECTS_DIR.exists():
        print("（还没有项目。agenthub project new 名字 建一个，"
              "或直接 agenthub workflow run 某份工作流）")
        return
    shown = 0
    for pd in sorted(PROJECTS_DIR.iterdir()):
        if not pd.is_dir():
            continue
        runs = [r for r in wf_runs(pd.name) if wf_load_state(r)]
        print(f"📁 {pd.name}" + ("（无运行记录）" if not runs else ""))
        for r in runs[::-1][:max(0, args.n)]:
            st = wf_load_state(r)
            shown += 1
            print(f"   {st['run_id']}  {st['status']:<8} 「{st['workflow']}」"
                  f"  更新 {st['updated_at']}")
    if not shown:
        print("（还没有运行记录）")


def wf_cmd_status(args):
    runs = wf_runs(args.project)
    run_dir = wf_project_dir(args.project) / "runs" / args.run if args.run \
        else (runs[-1] if runs else None)
    if not run_dir or not run_dir.exists():
        raise SystemExit(f"项目 {args.project} 没有对应的运行（agenthub workflow list 看看）")
    st = wf_load_state(run_dir)
    if not st:
        raise SystemExit(f"run 目录里没有 state.json：{run_dir}")
    icon = {"success": "✓", "failed": "✗", "running": "▸", "stopped": "‖",
            "skipped": "-", "pending": "·"}
    print(f"项目「{st['project']}」工作流「{st['workflow']}」 run {st['run_id']}")
    print(f"状态 {st['status']} · 开始 {st['created_at']} · 更新 {st['updated_at']}")
    print(f"目录 {run_dir}")
    b = wf_budget_get(st["run_id"])
    if b:
        print(f"预算：时间 {b['time_limit_s']:.0f}s · token {b['used_tokens']}/{b['token_limit']}"
              f" · 并行上限 {b['max_parallel'] or '未设'} · 深度上限 {b['max_depth'] or '默认'}")
    print(f"事件流水：agenthub workflow trace {args.project} --run {st['run_id']}")
    for sid, s in st["steps"].items():
        v = f"  verdict={s['verdict']}" if s["verdict"] else ""
        e = f"  ← {s['error'][:90]}" if s["error"] else ""
        print(f"  {icon.get(s['status'], '?')} {sid:<24} {s['status']:<8}"
              f" 尝试 {s['attempts']}{v}{e}")
    loops = st.get("loops") or {}
    if loops:
        print("打回记录：" + "，".join(f"{k}→{v} 次" for k, v in loops.items()))


def wf_cmd_stop(args):
    target = next((r for r in reversed(wf_runs(args.project))
                   if (wf_load_state(r) or {}).get("status") == "running"), None)
    if not target:
        raise SystemExit(f"项目 {args.project} 没有 running 状态的运行"
                         f"（agenthub workflow list 看看）")
    (target / "STOP").write_text(now_iso() + "\n", encoding="utf-8")
    print(f"已写 STOP 标记 → {target / 'STOP'}\n"
          f"引擎派下一个步骤前会停下；正在跑的步骤会等它自然跑完。")


def cmd_project(con, args):
    if args.pj_act == "new":
        pd = wf_project_dir(args.name)
        if pd.exists():
            raise SystemExit(f"项目目录已存在：{pd}")
        (pd / "runs").mkdir(parents=True, exist_ok=True)
        wfpath = pd / "workflow.yaml"
        wfpath.write_text(WF_TEMPLATE.replace('name: "软件开发示例"',
                                              f'name: "{args.name}"'), encoding="utf-8")
        insert_note(con, "user", "note", f"新建项目 {args.name}", "agenthub,project",
                    f"agenthub project new {args.name}：目录 {pd}，"
                    f"工作流模板 {wfpath}（记得改 team 里的 Agent）")
        print(f"项目已建：{pd}\n工作流模板：{wfpath}\n"
              f"下一步：改 team 与步骤 → agenthub workflow run {wfpath} --dry-run")
        return
    wf_cmd_list(args)     # project list 与 workflow list 看同一份项目/运行记录


# ---------------------------------------------------------------- 入口 ---

CATALOG_PATH = HOME / ".agents" / "catalog.json"

# 预存的各家 Agent 索引信息：安装位置候选、配置目录、原生记忆形状、会话记录形状、
# 本机实测过的多开结论。三个工具共用（驾驶舱识别产品、mem source 接入记忆、AgentFind 找解析器），
# 所以只写"本机验证过的事实"，不写猜出来的路径；额度接口留空由 agenthub catalog 之后补。
CATALOG_SEED = [
    {"id": "workbuddy", "name": "WorkBuddy", "vendor": "Tencent", "kind": "electron",
     "config": ".workbuddy", "exe": ["{LOCALAPPDATA}/LenovoSoftstore/Install/WorkBuddy/WorkBuddy.exe"],
     "memory": [".workbuddy/memory/*.md", ".workbuddy/MEMORY.md", ".workbuddy/USER.md",
                ".workbuddy/IDENTITY.md", ".workbuddy/SOUL.md", ".workbuddy/automations/*/memory.md"],
     "sessions": [], "multi_open": False,
     "note": "第二次启动被已有窗口接管（参数被丢弃），只能整目录换号"},
    {"id": "qoder-cn", "name": "Qoder CN", "vendor": "Alibaba", "kind": "electron",
     "config": ".qoder-cn", "exe": ["{LOCALAPPDATA}/Programs/Qoder CN/Qoder CN.exe"],
     "memory": [".qoder-cn/memory/**/*.md", ".qoder-cn/memories/**/*.md",
                ".qoder-cn/projects/*/memory/*.md"],
     "sessions": [], "multi_open": False, "note": "单实例接管"},
    {"id": "qoder", "name": "Qoder 国际版", "vendor": "Alibaba", "kind": "electron",
     "config": ".qoder", "exe": ["{LOCALAPPDATA}/Programs/Qoder/Qoder.exe"],
     "memory": [".qoder/memory/**/*.md", ".qoder/projects/*/memory/*.md"],
     "sessions": [], "multi_open": False, "note": "与 CN 版是两个独立产品，配置目录不共用"},
    {"id": "qoder-cn-ide", "name": "Qoder CN IDE", "vendor": "Alibaba", "kind": "electron",
     "config": ".qoder-cn-ide", "exe": ["{LOCALAPPDATA}/Programs/Qoder CN IDE/Qoder CN IDE.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "实测可 --user-data-dir 多开"},
    {"id": "astudio", "name": "AStudio", "vendor": "iFlytek", "kind": "electron",
     "config": "{LOCALAPPDATA}/Programs/AStudio Data/userdata",
     "exe": ["{LOCALAPPDATA}/Programs/AStudio/AStudio.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "科大讯飞星辰/星火；产品自带本地内核服务(127.0.0.1)，登录态是明文 astron-session.json"},
    {"id": "minimax", "name": "MiniMax Code", "vendor": "MiniMax", "kind": "electron",
     "config": ".minimax", "exe": ["{LOCALAPPDATA}/Programs/MiniMax Code/MiniMax Code.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "OAuth 设备码登录；令牌落 ~/.minimax/auth/<env>/<region>/mcode-public/auth.json"},
    {"id": "trae-cn", "name": "TRAE CN", "vendor": "ByteDance", "kind": "electron",
     "config": ".trae-cn",
     "exe": ["{LOCALAPPDATA}/Programs/Trae CN/Trae CN.exe",
             "{LOCALAPPDATA}/Programs/TRAE SOLO CN/TRAE SOLO CN.exe"],
     "memory": [".trae-cn/memory/**/*.md"],
     "sessions": [".trae-cn/memory/projects/**/session_memory_*.jsonl"],
     "multi_open": True,
     "note": "正文库加密，AgentFind 只能索引会话摘要；session_memory_*.jsonl 尚未接解析器"},
    {"id": "zcode", "name": "ZCode", "vendor": "Zhipu", "kind": "electron",
     "config": ".zcode", "exe": ["{LOCALAPPDATA}/Programs/ZCode/ZCode.exe"],
     "memory": [], "sessions": ["~/.zcode/cli/db/db.sqlite"], "multi_open": True,
     "note": "memoryEnabled:false，对共享记忆是纯读者"},
    {"id": "codex", "name": "Codex CLI", "vendor": "OpenAI", "kind": "cli",
     "config": ".codex", "exe": ["{APPDATA}/npm/codex.cmd"], "env": "CODEX_HOME",
     "memory": [".codex/memories/*.md", ".codex/memories/**/*.md", ".codex/AGENTS.md"],
     "sessions": [".codex/sessions/**/*.jsonl"], "multi_open": True,
     "note": "⚠ 原生记忆已迁到 .codex/memories_1.sqlite，md 侧是僵尸数据"},
    {"id": "claude", "name": "Claude Code CLI", "vendor": "Anthropic", "kind": "cli",
     "config": ".claude", "exe": ["{APPDATA}/npm/claude.cmd"], "env": "CLAUDE_CONFIG_DIR",
     "memory": [".claude/CLAUDE.md"], "sessions": [".claude/projects/*/*.jsonl"],
     "multi_open": True, "note": ""},
    {"id": "dsh", "name": "DeepSeek DSH", "vendor": "DeepSeek", "kind": "electron",
     "config": ".dsh", "exe": [], "memory": [],
     "sessions": [".dsh/sessions/**/session.*.jsonl.zst*"], "multi_open": None,
     "note": "zstd 压缩 jsonl，需 Python 3.14+ 标准库；会把注入型通知当用户消息当标题"},
    {"id": "qwenworkcn", "name": "QwenWork CN", "vendor": "Alibaba", "kind": "electron",
     "config": ".qwenworkcn", "exe": [],
     "memory": [".qwenworkcn/awareness/main/MEMORY.md", ".qwenworkcn/awareness/main/memory/*.md",
                ".qwenworkcn/awareness/**/*.md"],
     "sessions": [], "multi_open": None, "note": "自动任务会话多，AgentFind 已按 session.auto 降权"},
    {"id": "opencode", "name": "opencode", "vendor": "SST", "kind": "cli",
     "config": ".config/opencode", "exe": [], "memory": [],
     "sessions": ["~/.local/share/opencode/opencode.db"], "multi_open": None,
     "note": "sqlite 表集合 {session,message,part}，与 ZCode 同构"},
    {"id": "copilot", "name": "VS Code Copilot Chat", "vendor": "GitHub", "kind": "extension",
     "config": "Code/User/globalStorage/github.copilot-chat", "exe": [], "memory": [],
     "sessions": ["%APPDATA%/Code/User/globalStorage/github.copilot-chat/session-store.db"],
     "multi_open": None, "note": "一行一个来回；只读打开不能带 immutable（WAL 可见性）"},
    # ---- 以下照着 AgentCockpit registry.rs 补齐（2026-10-16 照 jlcodes99/cockpit-tools 清单）----
    {"id": "workbuddy-intl", "name": "WorkBuddy 国际版", "vendor": "Tencent", "kind": "electron",
     "config": ".workbuddy-ai", "exe": ["{LOCALAPPDATA}/Programs/WorkBuddyAI/WorkBuddyAI.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "独立安装的另一份 WorkBuddy（站点 www.workbuddy.ai），与国内版同一套解密链路"},
    {"id": "catpaw", "name": "妙手 (美团 CatPaw)", "vendor": "Meituan", "kind": "electron",
     "config": ".meituan-catpaw", "exe": ["{LOCALAPPDATA}/Programs/妙手/妙手.exe"],
     "memory": [".meituan-catpaw/<uid>/agents/**/*.md"],
     "sessions": [], "multi_open": False,
     "note": "单实例接管；登录态明文 ~/.meituan-catpaw/auth.json；控制面只挂命名管道"},
    {"id": "marvis", "name": "Marvis", "vendor": "Tencent", "kind": "plain",
     "config": ".marvis", "exe": ["{LOCALAPPDATA}/Programs/Marvis/Application/MarvisLauncher.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "Qt+CEF 壳（非 Electron）；.marvis 是 symlink → %APPDATA%\\Tencent\\Marvis\\User\\<openid>\\；登录态明文 mobile_address.json"},
    {"id": "xiaomi-mimo", "name": "Xiaomi MiMo", "vendor": "Xiaomi", "kind": "electron",
     "config": "Xiaomi MiMo", "exe": ["{LOCALAPPDATA}/Programs/Xiaomi MiMo/Xiaomi MiMo.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "多分区 Electron 应用；登录态在 Partitions\\xiaomi-account\\Network\\Cookies（整机一份）"},
    {"id": "codebuddy", "name": "CodeBuddy", "vendor": "Tencent", "kind": "electron",
     "config": "CodeBuddy", "exe": ["{LOCALAPPDATA}/Programs/CodeBuddy/CodeBuddy.exe",
     "{PROGRAMFILES}/CodeBuddy/CodeBuddy.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "VS Code/Chromium 壳"},
    {"id": "codebuddy-cn", "name": "CodeBuddy CN", "vendor": "Tencent", "kind": "electron",
     "config": "CodeBuddy CN", "exe": ["{LOCALAPPDATA}/Programs/CodeBuddy CN/CodeBuddy CN.exe",
     "{PROGRAMFILES}/CodeBuddy CN/CodeBuddy CN.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "VS Code/Chromium 壳"},
    {"id": "antigravity", "name": "Antigravity", "vendor": "Google", "kind": "electron",
     "config": "Antigravity", "exe": ["{LOCALAPPDATA}/Programs/antigravity/Antigravity.exe",
     "{LOCALAPPDATA}/Programs/Antigravity/Antigravity.exe",
     "{PROGRAMFILES}/Antigravity/Antigravity.exe"],
     "memory": [], "sessions": [], "multi_open": True,
     "note": "按 Google 账号计费，客户端没有可查的余额接口"},
    {"id": "coze", "name": "Coze", "vendor": "ByteDance", "kind": "electron",
     "config": "Coze", "exe": ["{LOCALAPPDATA}/Programs/Coze/Coze.exe",
     "{LOCALAPPDATA}/Programs/Coze/Coze.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "app_shell 多版本架构；登录态读得到（分区 localStorage 里的 userId），没有余额接口"},
    {"id": "raccoon", "name": "商汤小浣熊", "vendor": "SenseTime", "kind": "electron",
     "config": ".office-raccoon",
     "exe": ["{LOCALAPPDATA}/Programs/@raccoonoffice-desktop/商汤小浣熊.exe",
             "{LOCALAPPDATA}/Programs/@raccoonoffice-desktop/商汤小浣熊.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "档案根 ~/.office-raccoon（agent/config/auth.json 明文 JWT、user-data/ 为 Chromium）；"
             "客户端支持 --client-profile 多档案，驾驶舱暂按仅换号登记；没有余额接口"},
    {"id": "doubao", "name": "豆包", "vendor": "ByteDance", "kind": "electron",
     "config": "{LOCALAPPDATA}/Doubao",
     "exe": ["{LOCALAPPDATA}/Programs/Doubao/Doubao.exe",
             "{LOCALAPPDATA}/Programs/Doubao/app/Doubao.exe",
             "{LOCALAPPDATA}/Programs/Doubao/Doubao.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "CEF/Chromium 147 壳；登录态是 doubao.com 会话 cookie（User Data"
             "\\Default\\Network\\Cookies，os_crypt v10，豆包运行时该库独占）"},
    {"id": "cursor", "name": "Cursor", "vendor": "Anysphere", "kind": "electron",
     "config": "Cursor", "exe": ["{LOCALAPPDATA}/Programs/cursor/Cursor.exe",
     "{LOCALAPPDATA}/Programs/Cursor/Cursor.exe",
     "{PROGRAMFILES}/cursor/Cursor.exe"],
     "memory": [], "sessions": [], "multi_open": True,
     "note": "正文存服务器端，本机只有空壳元数据（AgentFind 索引不到）"},
    {"id": "kiro", "name": "Kiro", "vendor": "Kiro", "kind": "electron",
     "config": "Kiro", "exe": ["{LOCALAPPDATA}/Programs/Kiro/Kiro.exe",
     "{PROGRAMFILES}/Kiro/Kiro.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "VS Code/Chromium 壳"},
    {"id": "windsurf", "name": "Windsurf", "vendor": "Codeium", "kind": "electron",
     "config": "Windsurf", "exe": ["{LOCALAPPDATA}/Programs/Windsurf/Windsurf.exe",
     "{PROGRAMFILES}/Windsurf/Windsurf.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "VS Code/Chromium 壳"},
    {"id": "zed", "name": "Zed", "vendor": "Zed Industries", "kind": "electron",
     "config": "Zed", "exe": ["{LOCALAPPDATA}/Programs/Zed/Zed.exe",
     "{PROGRAMFILES}/Zed/Zed.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "VS Code/Chromium 壳"},
    {"id": "grok", "name": "Grok Bot", "vendor": "xAI", "kind": "electron",
     "config": "Grok Bot", "exe": ["{LOCALAPPDATA}/Programs/Grok Bot/Grok Bot.exe",
     "{LOCALAPPDATA}/Programs/Grok Bot/Grok Bot.exe"],
     "memory": [], "sessions": [], "multi_open": True,
     "note": "Electron 壳；~/.grok 是运行时家目录"},
    {"id": "trae", "name": "Trae", "vendor": "ByteDance", "kind": "electron",
     "config": "Trae", "exe": ["{LOCALAPPDATA}/Programs/Trae/Trae.exe",
     "{PROGRAMFILES}/Trae/Trae.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "国际版"},
    {"id": "trae-solo", "name": "TRAE SOLO", "vendor": "ByteDance", "kind": "electron",
     "config": "TRAE SOLO", "exe": ["{LOCALAPPDATA}/Programs/TRAE SOLO/TRAE SOLO.exe",
     "{PROGRAMFILES}/TRAE SOLO/TRAE SOLO.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "国际版"},
    {"id": "trae-solo-cn", "name": "TRAE SOLO CN", "vendor": "ByteDance", "kind": "electron",
     "config": "TRAE SOLO CN", "exe": ["{LOCALAPPDATA}/Programs/TRAE SOLO CN/TRAE SOLO CN.exe",
     "{LOCALAPPDATA}/Programs/TRAE SOLO CN/TRAE SOLO CN.exe"],
     "memory": [], "sessions": [], "multi_open": True, "note": "国内版"},
    {"id": "codex-desktop", "name": "ChatGPT", "vendor": "OpenAI", "kind": "pwa",
     "config": "Codex/web/Codex", "exe": ["{PROGRAMFILES}/WindowsApps/OpenAI.Codex_*_x64__2p2nqsd0c76g0/app/ChatGPT.exe"],
     "memory": [], "sessions": [], "multi_open": True,
     "note": "MSIX Chromium PWA 壳；--user-data-dir 实测可多开"},
    {"id": "claude-desktop", "name": "Claude 桌面版", "vendor": "Anthropic", "kind": "plain",
     "config": "Claude", "exe": ["{PROGRAMFILES}/WindowsApps/Claude_*_x64__pzs8sxrjxfjjc/app/Claude.exe"],
     "memory": [], "sessions": [], "multi_open": False,
     "note": "官方包身份启动，不复制/接管资料"},
]
# 余额/额度接口：只写**本机日志或官方文档里确证存在**的端点，状态分四档，避免把猜的当成品。
#   ready         有 API Key 就能直接查（官方公开）
#   need_capture  端点确凿，但要客户端自己的登录态，得先抓包导出 token
#   need_key      端点确凿，缺一个能用的 API Key
#   none          本轮没找到任何证据
QUOTA = {
    "dsh": ("need_key", "https://api.deepseek.com/user/balance", "balance_infos",
                     "Bearer <sk- API Key>", "官方文档；实测端点活着，用登录态 token 会 401"),
    "codex": ("ready", "https://api.openai.com/v1/organization/costs", None,
              "Bearer <Admin Key>", "驾驶舱已有 openai 适配器；本地只有占位 key"),
    "claude": ("need_key", None, None, "Bearer",
              "走 aiwelink 中转，settings.json 里有明文 sk-；实测 /v1/models 200 正常，但 /api/user/self "
              "管理面对大陆 IP 返回地域拦截页——查余额需代理，或在 aiwelink 网页上看"),
    "zcode": ("local_read", "https://zcode.z.ai/api/v1/zcode-plan/billing/balance?app_version=3.14.5",
              "data.balances", "本地 enc:v1 解密 / Bearer",
              "已打通（2026-10-11）：凭据 enc:v1 是 AES-256-GCM，key=SHA256(env.ZCODE_CREDENTIAL_SECRET "
              "?? 'zcode-credential-fallback:{platform}:{homedir}:{username}')，纯本地派生、不依赖 DPAPI、"
              "不用抓包；解出 zcodejwttoken 后调 balance。⚠ 服务端硬性要求 X-Device-Mid 头（取自 "
              "v2/telemetry-state.json），缺则 400 code:3001；实测只带 Authorization+Device-Mid 即可 200。"
              "服务端有频率限制，连发会 429。支持 --profile <账号目录> 按账号分别读"),
    "qoder-cn": ("local_read", "https://openapi.qoder.com.cn/api/v2/quota/usage", None, "本地 DPAPI / Bearer",
                 "已打通且优先走服务端实时：拿 state.vscdb 的 secret://aicoding.auth.userInfo.token 调 "
                 "/api/v2/quota/usage（客户端 getAccountUsage 同一条），失败退回本地缓存的 creditUsage。"
                 "本地缓存会过期（实测本地 1200、服务端 1700）。支持 --profile <账号目录> 按账号分别读；"
                 "逐笔到期日只在 dedicatedResourcePackages 里有，主额度/附加包只有汇总 expiresAt"),
    "qoder": ("local_read", "https://openapi.qoder.sh/api/v2/user/plan", None, "本地 DPAPI",
              "与 CN 版同一套代码路径，同一份实现直接读出多桶积分与到期日"),
    "trae-cn": ("local_read", "https://api.trae.cn/trae/api/v2/pay/ide_user_ent_usage", None,
                "本地 byteCrypto / Cloud-IDE-JWT",
                "已打通：登录态在 %APPDATA%\\Trae CN\\User\\globalStorage\\storage.json"
                "（2026-10-11 升级前是 TRAE SOLO CN）的 "
                "iCubeAuthInfo://icube.cloudide（不是 DPAPI，是 Trae 自研 byteCrypto.js：AES-128-CBC + "
                "SHA-512 密钥派生，密钥硬编码在 main.js，静态可解）。解出 token 后调 "
                "/trae/api/v2/pay/ide_user_ent_usage（服务端实时），usage_summary 是权威余额，"
                "user_entitlement_pack_list 逐笔套餐（免费额度+每月登录赠送+每日签到奖励）。"
                "支持 --profile <账号目录> 按账号分别读"),
    "workbuddy": ("local_read", "https://www.workbuddy.cn/billing/meter/get-user-resource-summary",
                  None, "本地 wbEncrypted 解密 / Bearer",
                  "已打通服务端实时余额：atRestSecretKey 逆向自 exe（TEA-18 加密常量 RVA 0xA0F5798，"
                  "硬编码对所有安装通用），K=SHA256(atRestSecretKey) 解密 %LOCALAPPDATA%\\CodeBuddyExtension"
                  "\\Data\\Public\\auth\\workbuddy-desktop.info 的 accessToken（$wbEncrypted 信封 AES-256-GCM），"
                  "再 POST /billing/meter/get-user-resource-summary（Header 必带 User-Agent，否则 403），"
                  "取 data.Packages[].CycleRemainCapacity。agenthub quota workbuddy 读国内版，"
                  "workbuddy-intl 读国际版（workbuddy-desktop-ai.info，endpoint www.workbuddy.ai）。"
                  "逐笔到期日 summary 里没有，另调 get-user-resource-free/paid-packages"
                  "（body 必带 PackageCodes，取自 app.asar 枚举）取 Accounts[] 的 "
                  "CycleEndTime/DeductionEndTime，每个包取有效笔中较早的一笔"),
    "workbuddy-intl": ("local_read", "https://www.workbuddy.ai/billing/meter/get-user-resource-summary",
                       None, "本地 wbEncrypted 解密 / Bearer",
                       "WorkBuddy 国际版：登录态 workbuddy-desktop-ai.info（domain www.workbuddy.ai，"
                       "endpoint www.workbuddy.ai），与国内版同一套解密链路（同一个 atRestSecretKey）"),
    "qwenworkcn": ("none", None, None, None, "只找到 UI 标记文件，非余额"),
    "opencode": ("need_key", None, None, "Bearer", "走各家 relay，用对应上游的接口查"),
    "astudio": ("local_read", "https://agent.xfyun.cn/xingchen-studio/points/balance", "data",
                "Cookie: ssoSessionId/sso_sessionid/account_id/token（明文 astron-session.json）",
                "已打通并实测（2026-10-09）：登录态是明文 JSON（<安装目录上一级>/AStudio Data/userdata/"
                "astron-session.json），直接调官方 points/balance，返回 totalAmount/totalBalance（通用积分）"
                "与 sparkTotalAmount/sparkTotalBalance（星火积分），activityNextExpireTime 是到期日。"
                "产品自带本地内核服务（127.0.0.1:<port>/api/astron-auth/points-balance）背后就是这条。"),
    "minimax": ("local_read", "https://www.minimaxi.com/backend/account/token_plan_credit", "data",
                "Bearer <mcode OAuth access token>（~/.minimax/auth/**/mcode-public/auth.json）",
                "已打通并实测（2026-10-09）：读 auth.json 的 accessToken 直调，返回 "
                "total_credits/used_credits/remaining_credits/balance_breakdown/api_key。"
                "access token 过期时用 refresh_token 走 account.minimax.cn/oauth2/token 续期并写回。"
                "⚠ refresh token 是轮换+重放检测的：换过一次后旧令牌立即作废，再重放会撤销整族（需重新登录）"),
    "antigravity": ("none", None, None, None,
                    "按 Google 账号计费，客户端没有可查的余额接口——只报得出登的是谁、报不出额度"),
    "coze": ("none", None, None, None,
             "登录态读得到（分区 localStorage 里的 userId），但没有余额接口"),
    "raccoon": ("local_read", "https://xiaohuanxiong.com/api/web/points/v1/balance",
                "~/.office-raccoon/agent/config/auth.json", "Bearer <access_token>",
                "已打通并实测（2026-11-05）：登录态明文 ~/.office-raccoon/agent/config/auth.json"
                "（access_token 为 HS256 JWT，sub=pu-…）；GET /points/v1/balance 给"
                "available_points 与 daily/reward/topup/monthly 分档，/points/v1/bills 给逐笔发放与"
                "expire_at，/auth/v1/user_info 给昵称+手机号，/auth/v1/entitlement_info 给 Free/Pro。"
                "每日 300；当天登录过桌面客户端的那笔有效期 7 天（实测 11-05→11-11），"
                "签到＝带桌面头 POST /office/v3/setting_info（照 Magpie 的 raccoon 插件）"),
}


def _expand(p):
    return (str(p)
            .replace("{LOCALAPPDATA}", os.environ.get("LOCALAPPDATA", ""))
            .replace("{APPDATA}", os.environ.get("APPDATA", ""))
            .replace("%APPDATA%", os.environ.get("APPDATA", ""))
            .replace("{PROGRAMFILES}", os.environ.get("PROGRAMFILES", "")))


def _hits_glob(glob):
    """目录里的形状字段允许 ~/、%APPDATA%/、相对 home 三种写法，统一判存在。"""
    g = _expand(glob)
    if g.startswith("~/"):
        g = g[2:]
    try:
        if len(g) > 1 and g[1] == ":":                 # 已经展开成绝对路径
            p = Path(g)
            return p.exists() if "*" not in g else next(p.parent.glob(p.name), None) is not None
        return next(HOME.glob(g), None) is not None    # 相对 home
    except (OSError, ValueError):
        return False


def installed_probe(a):
    """在装判断：可执行文件 / 配置目录 / 记忆或会话文件，三者命中任一即算装着。"""
    for cand in a.get("exe") or []:
        if Path(_expand(cand)).exists():
            return True
    cfg = a.get("config") or ""
    if cfg:
        if cfg.startswith("."):
            if (HOME / cfg).exists():
                return True
        elif cfg.startswith("~"):
            if Path(_expand(cfg)).exists():
                return True
        elif (Path(os.environ.get("APPDATA", "")) / cfg).exists():
            return True
    return any(_hits_glob(g) for g in (a.get("memory") or []) + (a.get("sessions") or []))


def catalog_build():
    """把内置目录 + 本机实测（哪些装着、记忆接了哪几条）合成 ~/.agents/catalog.json。"""
    src_globs = {}
    for product, g in load_sources():
        src_globs.setdefault(product, []).append(g)
    out = []
    for a in CATALOG_SEED:
        e = dict(a)
        e["installed"] = installed_probe(a)
        e["memory_synced"] = sorted(src_globs.get(a["id"], []))
        st, url, path, auth, note = QUOTA.get(a["id"], (None, None, None, None, ""))
        e["quota"] = {"status": st, "url": url, "path": path, "auth": auth, "evidence": note}
        if a["id"] in CAPS:
            e["capabilities"] = dict(CAPS[a["id"]])
        e["connector"] = connector_of(a["id"])
        caps = e.get("capabilities") or {}
        conn = e["connector"]
        e["card"] = {
            "id": a["id"], "name": a["name"], "vendor": a["vendor"],
            "installed": bool(e["installed"]),
            "capabilities": caps,
            "connector": conn,
            "limits": {
                "max_parallel": 1 if conn.get("type") == "bridge" else 2,
                "max_depth": MAX_CHAIN_DEPTH,
            },
            "io": {
                "input": ["text"],
                "output": ["text"] + (["files"] if caps.get("can_write_files") else []),
            },
        }
        out.append(e)
    CATALOG_PATH.parent.mkdir(parents=True, exist_ok=True)
    doc = {"version": 1, "updated": dt.datetime.now().strftime("%Y-%m-%d %H:%M"),
           "note": "跨工具共享的 Agent 索引；只写本机验证过的事实。驾驶舱与 agenthub/AgentFind 共用。",
           "agents": out}
    CATALOG_PATH.write_text(json.dumps(doc, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return doc


def cmd_catalog(con, args):
    if args.catalog_act == "build":
        doc = catalog_build()
        ins = [a["name"] for a in doc["agents"] if a["installed"]]
        print(f"已写出 {CATALOG_PATH}")
        print(f"目录收录 {len(doc['agents'])} 家；本机在装 {len(ins)} 家：{'、'.join(ins)}")
        return
    doc = json.loads(CATALOG_PATH.read_text(encoding="utf-8")) if CATALOG_PATH.exists() else None
    if not doc:
        print("还没有目录，先跑：agenthub catalog build")
        return
    if args.catalog_act == "card":
        aid = str(args.product or "").strip()
        for a in doc["agents"]:
            if a["id"] == aid:
                print(json.dumps(a.get("card") or {}, ensure_ascii=False, indent=2))
                return
        raise SystemExit(f"目录里没有「{aid}」；可选："
                         f"{'、'.join(x['id'] for x in doc['agents'])}")
    for a in doc["agents"]:
        q = (a.get("quota") or {}).get("status") or "—"
        print(f"  {'●' if a['installed'] else '○'} {a['name']:<20}{a['vendor']:<10}"
              f"记忆 {len(a['memory_synced']):>2} 条  余额[{q}]")
        conn = a.get("connector") or {}
        if conn.get("type") and conn["type"] != "none":
            how = f"（{conn['bin']}）" if conn.get("bin") else ""
            print(f"      接入 {conn['type']}{how}")
        if args.detail and (a.get("quota") or {}).get("url"):
            print(f"      {a['quota']['url']}")
            print(f"      鉴权 {a['quota']['auth']}｜{a['quota']['evidence'][:70]}")
    print(f"\n生成于 {doc['updated']}，文件：{CATALOG_PATH}")


# ---------------------------------------------------------------------------
# 本机凭据直读：Qoder 系的积分本来就存在它自己的 state.vscdb 里（Chromium os_crypt v10）。
# 全程只读、不联网、不解他人账号——DPAPI 主密钥只对当前 Windows 登录用户有效。
# ---------------------------------------------------------------------------
QODER_STORES = {"qoder-cn": "QoderCN", "qoder": "Qoder"}

# Qoder 0.4.x 换了壳。实测（2026-10-08）：
#   新壳（0.4.x，`Qoder.exe` / `Qoder CN.exe` 本体）把登录令牌写成
#     %APPDATA%\com.<bundle>.app.stable\auth.v1.dat —— v10 加密的一段 JSON
#     {token, refreshToken, expiresAt, user}；运行时会话家目录是 ~/.qoder。
#   旧壳（VS Code 内核）写的是
#     %APPDATA%\<产品>\User\globalStorage\state.vscdb 的 secret://aicoding.auth.userInfo。
# 只认旧壳会读到几个月前的旧账：国际版那个 state.vscdb 停在 8-27、令牌 9-08 就过期了，
# 界面却把那份「剩 500/500」当实时余额显示——所以两份都要认，新壳优先。
# 键同时收产品 id 与档案目录名（QODER_STORES 传下来的是后者），与 QODER_API 一致
QODER_NEW_SHELL = {
    "qoder": "com.qoder.app.stable", "Qoder": "com.qoder.app.stable",
    "qoder-cn": "com.qodercn.app.stable", "QoderCN": "com.qodercn.app.stable",
}
FOREVER_MS = 253402214400000      # 9999-12-31：厂商拿它当"无限期"哨兵
EXPIRY_UNKNOWN = "未给出"          # 接口根本没给这一桶的到期日 ≠ 无限期，别混为一谈

# 只能读"本机累计已用"、读不到余额/到期日的店（余额要走厂商接口，凭据不落明文）。
# workbuddy 已升级到服务端实时余额（workbuddy_full），不再走 usage_only；
# 这里留空占位，workbuddy_credit 仍可作为 workbuddy_full 失败时的累计已用兜底。
LOCAL_USAGE_STORES = set()


def _dpapi(data):
    import ctypes
    from ctypes import wintypes

    class BLOB(ctypes.Structure):
        _fields_ = [("cbData", wintypes.DWORD), ("pbData", ctypes.POINTER(ctypes.c_char))]

    buf = ctypes.create_string_buffer(data, len(data))
    src = BLOB(len(data), ctypes.cast(buf, ctypes.POINTER(ctypes.c_char)))
    out = BLOB()
    if not ctypes.windll.crypt32.CryptUnprotectData(
            ctypes.byref(src), None, None, None, None, 0, ctypes.byref(out)):
        raise RuntimeError("CryptUnprotectData 失败：这份凭据不是当前 Windows 用户加密的")
    res = ctypes.create_string_buffer(out.cbData)
    ctypes.memmove(res, out.pbData, out.cbData)
    ctypes.windll.kernel32.LocalFree(out.pbData)
    return res.raw


def _aes_key(ls_path):
    """Local State 里的 os_crypt.encrypted_key = base64("DPAPI" + DPAPI 包裹的 32B AES 主密钥)。"""
    wrapped = base64.b64decode(json.loads(ls_path.read_text(encoding="utf-8"))["os_crypt"]["encrypted_key"])
    if wrapped[:5] != b"DPAPI":
        raise RuntimeError("encrypted_key 缺 DPAPI 前缀，格式变了")
    return _dpapi(wrapped[5:])


def _read_secret(con, aes, key):
    row = con.execute("select value from ItemTable where key=?", (key,)).fetchone()
    if not row:
        return None
    blob = bytes(json.loads(row[0])["data"])
    if blob[:3] != b"v10":
        return None
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    return json.loads(AESGCM(aes).decrypt(blob[3:15], blob[15:], None))


# 各家 Qoder 的用量接口（客户端 getAccountUsage 用的就是这条，服务端实时数据）。
# 键同时收产品 id 与档案目录名（QODER_STORES 传下来的是后者）。
QODER_API = {
    "qoder-cn": "https://openapi.qoder.com.cn", "QoderCN": "https://openapi.qoder.com.cn",
    "qoder": "https://openapi.qoder.sh", "Qoder": "https://openapi.qoder.sh",
}


def _qoder_profile(appdata_name, profile=None):
    """profile 给定就直接用那个目录（驾驶舱按账号目录分别读的关键）；
    否则用 %APPDATA%/<appdata_name>（该产品当前的默认档案）。"""
    return Path(profile) if profile else Path(os.environ.get("APPDATA", "")) / appdata_name


def _qoder_candidate_dirs(appdata_name, profile=None):
    """该产品的登录档案目录，按优先级：新壳 → 旧壳。
    给了 profile（驾驶舱按账号读）就只认它——绝不能拿别的目录顶替，否则就是串号。"""
    if profile:
        return [Path(profile)]
    root = Path(os.environ.get("APPDATA", ""))
    out = []
    new = QODER_NEW_SHELL.get(appdata_name)
    if new and new != appdata_name:
        out.append(root / new)
    out.append(root / appdata_name)
    return out


def _qoder_read_login(d):
    """从一个档案目录里取出登录态。返回 (info, err)；info 里带 from（实际读的目录/文件）。

    新壳 auth.v1.dat：v10(AES-GCM) 加密的 JSON，直接有 token / expiresAt / user。
    旧壳 state.vscdb：secret://aicoding.auth.userInfo（也有 token），外加
    secret://aicoding.auth.creditUsage 这份"客户端上次联网时的缓存"。
    """
    ls = d / "Local State"
    if not ls.is_file():
        return None, f"{d} 里没有 Local State"
    try:
        aes = _aes_key(ls)
    except Exception as e:      # noqa: BLE001 主密钥取不到就没法往下走，如实报
        return None, f"{d} 的主密钥取不到：{e}"

    auth = d / "auth.v1.dat"
    if auth.is_file():
        try:
            blob = auth.read_bytes()
            if blob[:3] == b"v10":
                from cryptography.hazmat.primitives.ciphers.aead import AESGCM
                j = json.loads(AESGCM(aes).decrypt(blob[3:15], blob[15:], None))
                if j.get("token"):
                    return {
                        "token": j["token"], "who": j.get("user") or {},
                        "expires_at": j.get("expiresAt") or "",
                        "from": str(auth), "kind": "auth.v1.dat",
                        "cached": None,
                    }, None
        except Exception as e:  # noqa: BLE001
            return None, f"{auth} 解不开：{e}"

    db = d / "User" / "globalStorage" / "state.vscdb"
    if not db.is_file():
        return None, f"{d} 里既没有 auth.v1.dat，也没有 state.vscdb"
    try:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        try:
            cached = _read_secret(con, aes, "secret://aicoding.auth.creditUsage")
            who = _read_secret(con, aes, "secret://aicoding.auth.userInfo")
        finally:
            con.close()
    except Exception as e:      # noqa: BLE001
        return None, f"{type(e).__name__}: {e}"
    token = (who or {}).get("token") or ""
    exp = (who or {}).get("expireTime") or 0
    exp_iso = ""
    if isinstance(exp, (int, float)) and exp > 10 ** 11:
        exp_iso = dt.datetime.fromtimestamp(exp / 1000, tz=dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    if not token:
        return None, f"{db} 里没有登录令牌（可能没登录过）"
    return {
        "token": token, "who": who, "expires_at": exp_iso,
        "from": str(db), "kind": "state.vscdb", "cached": cached,
    }, None


def qoder_credit(appdata_name, profile=None, live=True):
    """读 Qoder 积分。优先走服务端实时接口（用登录 token），失败再退回客户端本地缓存。
    只读，不写任何产品文件。

    为什么优先服务端：本地那份 creditUsage 是客户端上次联网时写下的缓存，会过期——
    实测同一账号本地写着附加包 1200、服务端已经是 1700。

    返回里带 live_error：服务端没调通的原因。"只能读本地缓存"必须让调用方知道，
    否则会把几个月前的旧数当成实时余额报给用户（令牌过期就是这么露出来的）。
    另外带 auth_from / auth_kind：这份登录态是从哪个档案、哪种文件读到的。
    """
    try:
        import cryptography  # noqa: F401  探测可选依赖
    except ImportError:
        return {"error": "缺 cryptography 库（pip install cryptography）"}

    dirs = _qoder_candidate_dirs(appdata_name, profile)
    tried, login, last_err = [], None, "没找到任何登录档案目录"
    for ad in dirs:
        info, err = _qoder_read_login(ad)
        tried.append(f"{ad}：{err or '读到登录态'}")
        if info:
            login = info
            break
        last_err = err or "读不到登录态"
    if login is None:
        return {
            "error": f"这个档案目录里没有登录数据（{dirs[0]}）——该账号还没在这台机器上登录过",
            "tried": tried if len(dirs) > 1 else None,
        }

    token = login["token"]
    live_err = None
    if live:
        got, live_err = _qoder_fetch_usage(appdata_name, token)
        if got is not None:
            return {
                "data": got, "who": login["who"], "source": "server",
                "auth_from": login["from"], "auth_kind": login["kind"],
            }
    elif not token:
        live_err = "登录信息里没有 token"

    cached = login.get("cached")
    if cached is None:
        # 新壳不在 state.vscdb 里存 creditUsage：服务端读不成时去 main.sqlite 找兜底
        cached = _qoder_cached_from_sqlite(Path(login["from"]).parent)
    if cached is None:
        return {
            "error": "服务端读不到，本地也没有可用的额度缓存"
                     + (f"（{live_err}）" if live_err else ""),
            "auth_from": login["from"], "auth_kind": login["kind"],
        }
    out = {
        "data": cached, "who": login["who"], "source": "local-cache",
        "auth_from": login["from"], "auth_kind": login["kind"],
    }
    if live_err:
        out["live_error"] = live_err
    return out


def _qoder_cached_from_sqlite(d):
    """新壳把额度快照放在 main.sqlite（key/value 形态）。给一个只读兜底：
    服务端连不上时至少能报出"上次联网时的数"，而不是什么都没有。"""
    db = d / "main.sqlite"
    if not db.is_file():
        return None
    try:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        try:
            tabs = [r[0] for r in con.execute(
                "select name from sqlite_master where type='table'").fetchall()]
            for t in tabs:
                cols = [c[1] for c in con.execute(f'pragma table_info("{t}")').fetchall()]
                blobcols = [c for c in cols if c.lower() in ("value", "json", "data", "payload")]
                keycols = [c for c in cols if "key" in c.lower() or "name" in c.lower()]
                if not blobcols or not keycols:
                    continue
                rows = con.execute(
                    f'select {keycols[0]}, {blobcols[0]} from "{t}"').fetchall()
                for k, v in rows:
                    if not isinstance(v, (str, bytes)):
                        continue
                    s = v.decode("utf-8", "replace") if isinstance(v, bytes) else v
                    if "quota" in str(k).lower() or '"isQuotaExceeded"' in s:
                        try:
                            j = json.loads(s)
                        except Exception:       # noqa: BLE001
                            continue
                        if isinstance(j, dict) and "isQuotaExceeded" in j:
                            return j
        finally:
            con.close()
    except sqlite3.Error:
        return None
    return None


def _qoder_fetch_usage(appdata_name, token):
    """调服务端 /api/v2/quota/usage（客户端 getAccountUsage 同一条）。

    返回 (data, err)：成功是 (dict, None)，失败是 (None, 原因字符串)。
    以前这里把异常一律吞成 None，驾驶舱就只能拿到过期缓存、也不知道为什么——
    401 TOKEN_EXPIRE 会被静默降级成"读到了一份旧余额"。
    """
    base = QODER_API.get(appdata_name)
    if not base:
        return None, f"没有登记 {appdata_name} 的用量接口"
    req = UrlRequest(f"{base}/api/v2/quota/usage", headers={
        "Authorization": f"Bearer {token}",
        "Accept": "application/json",
        "User-Agent": "agenthub",
    })
    try:
        with direct_open(req, timeout=10) as r:
            if r.status != 200:
                return None, f"HTTP {r.status}"
            d = json.loads(r.read().decode("utf-8", "replace"))
    except HTTPError as e:
        body = ""
        try:
            body = e.read().decode("utf-8", "replace")[:160]
        except Exception:
            pass
        return None, f"HTTP {e.code} {body}".strip()
    except Exception as e:
        return None, f"{type(e).__name__}: {e}"
    if not isinstance(d, dict) or "userId" not in d:
        return None, "服务端返回的结构不认识"
    # 服务端字段与客户端缓存同形，这里补齐客户端会补的默认值
    d.setdefault("expiresAt", 0)
    d.setdefault("isQuotaExceeded", False)
    # dedicatedResourcePackages：每笔资源包自带 expiresAt，逐笔过期就靠它
    packs = d.get("dedicatedResourcePackages") or d.get("dedicated_resource_packages") or []
    if packs:
        d["dedicatedResourcePackages"] = packs
    return d, None


# ---------------------------------------------------------------------------
# WorkBuddy 服务端实时余额（2026-10-10 打通）：静态破解了登录态加密。
# 登录态在 %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\*.info，
# accessToken 用 $wbEncrypted 信封（AES-256-GCM + 自定义 AAD）加密，
# 根密钥 atRestSecretKey 硬编码在 WorkBuddy.exe（TEA-18 加密），对所有安装通用。
#   K = SHA256(atRestSecretKey)，解密登录态字段（field framing）。
#   额度接口：POST {endpoint}/billing/meter/get-user-resource-summary（body {}，
#   Header: Bearer token + X-User-Id + User-Agent，国内版不带 UA 会 403）。
#   workbuddy-desktop.info = 国内版（endpoint www.workbuddy.cn）
#   workbuddy-desktop-ai.info = 国际版（endpoint www.workbuddy.ai）
# ---------------------------------------------------------------------------
WORKBUDDY_AT_REST_SECRET_KEY = "Sik9U5aXhCdwTVEwsEySDOmDoB9r9ntFxHF1fst9LQI="


def _wb_aad(framing, scheme, suite, key_id):
    """逆向自 byteCrypto 的 buildAuthenticatedContextAad（sym-v1）。"""
    tag = {"file": "WBEF1", "field": "WBEV1", "record": "WBER1", "stream": "WBES1"}[framing]
    ftype = {"file": 1, "field": 2, "record": 3, "stream": 4}[framing]

    def u32(n):
        return n.to_bytes(4, "big")

    def lp(s):
        b = s.encode("utf-8")
        return u32(len(b)) + b

    return (b"WB-AAD\x00" + b"\x01" + lp(tag) + lp(scheme)
            + u32(suite) + lp(key_id) + bytes([ftype]) + b"\x00\x00")


def _wb_decrypt_field(key, field):
    """解密 $wbEncrypted 字段（field framing, sym-v1），返回明文 str。"""
    if isinstance(field, str):
        return field
    if not isinstance(field, dict) or field.get("$wbEncrypted") != 1:
        raise ValueError("不是 $wbEncrypted 字段")
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    env = json.loads(base64.b64decode(field["envelope"]).decode("utf-8"))
    aad = _wb_aad("field", "sym-v1", int(env["suite"]), env["keyId"])
    nonce = base64.b64decode(env["nonce"])
    ct = base64.b64decode(env["ciphertext"]) + base64.b64decode(env["authTag"])
    return AESGCM(key).decrypt(nonce, ct, aad).decode("utf-8")


def _wb_read_login(intl=False, auth_file=None):
    """读 WorkBuddy 登录态（解密 accessToken）。返回 (info, err)。

    auth_file：驾驶舱"按账号查"时给的登录态文件路径——同一个产品在这台机器上只可能有
    一份 .info（路径写死），所以要查别的号，只能由调用方先把那个号的包铺到别处再指过来。
    给了就只认它，绝不回落默认路径（回落就是串号）。"""
    if auth_file:
        path = Path(auth_file)
    else:
        auth_dir = Path(os.environ.get("LOCALAPPDATA", "")) / "CodeBuddyExtension" / "Data" / "Public" / "auth"
        fname = "workbuddy-desktop-ai.info" if intl else "workbuddy-desktop.info"
        path = auth_dir / fname
    if not path.is_file():
        return None, f"没找到登录态 {path}（该版本可能没在这台机器登录过）"
    try:
        obj = json.loads(path.read_text(encoding="utf-8"))
    except Exception as e:      # noqa: BLE001
        return None, f"登录态读不了：{e}"
    try:
        import hashlib as _h
        K = _h.sha256(WORKBUDDY_AT_REST_SECRET_KEY.encode("utf-8")).digest()
        token = _wb_decrypt_field(K, obj["auth"]["accessToken"])
        nickname = _wb_decrypt_field(K, obj["account"].get("nickname"))
    except Exception as e:      # noqa: BLE001
        return None, f"登录态解不开：{e}"
    return {
        "uid": obj["account"].get("uid", ""),
        "nickname": nickname,
        "token": token,
        "domain": obj["auth"].get("domain", ""),
        "expires_at": obj["auth"].get("expiresAt", 0),
        "from": str(path),
    }, None


def _wb_fetch_usage(domain, token, uid):
    """调 WorkBuddy 服务端额度接口。返回 (data, err)。"""
    endpoint_map = {
        "www.codebuddy.cn": "https://www.workbuddy.cn",
        "www.workbuddy.cn": "https://www.workbuddy.cn",
        "www.workbuddy.ai": "https://www.workbuddy.ai",
    }
    base = endpoint_map.get(domain, "https://www.workbuddy.cn")
    url = base + "/billing/meter/get-user-resource-summary"
    req = UrlRequest(url, data=b"{}", headers={
        "Content-Type": "application/json",
        "Accept": "application/json",
        "Authorization": f"Bearer {token}",
        "X-User-Id": uid,
        "User-Agent": "WorkBuddy/1.0",   # 国内版不带 UA 会 403「请求不合法」
    })
    try:
        op = build_opener(ProxyHandler({}))
        with op.open(req, timeout=15) as r:
            if r.status != 200:
                return None, f"HTTP {r.status}"
            d = json.loads(r.read().decode("utf-8", "replace"))
    except HTTPError as e:
        body_txt = ""
        try:
            body_txt = e.read().decode("utf-8", "replace")[:160]
        except Exception:
            pass
        return None, f"HTTP {e.code} {body_txt}".strip()
    except Exception as e:
        return None, f"{type(e).__name__}: {e}"
    if d.get("code") != 0 or "data" not in d:
        return None, f"服务端返回异常：{d.get('msg') or d}"
    return d["data"], None


# 逐笔到期日不在 summary 里（Packages[] 只有 Cycle 容量，没有任何日期字段），
# 而在 free/paid-packages 的 Accounts[]：CycleEndTime（"YYYY-MM-DD HH:MM:SS"）、
# DeductionEndTime（毫秒）、ExpiredTime、Status（0=有效 / 3=已用完）。
# WorkBuddy 客户端自己就是并行调这三个接口再合并（expireAt = free 码 ? CycleEndTime
# : DeductionEndTime）。这两个接口 body 必须带 PackageCodes（缺了报 10001
# "PackageCodes required"），下面用 app.asar 里的枚举全集。
WORKBUDDY_PACK_CODES = [
    "TCACA_code_001_PqouKr6QWV", "TCACA_code_002_AkiJS3ZHF5",
    "TCACA_code_003_FAnt7lcmRT", "TCACA_code_005_maRGyrHhw1",
    "TCACA_code_006_DbXS0lrypC", "TCACA_code_007_nzdH5h4Nl0",
    "TCACA_code_008_cfWoLwvjU4", "TCACA_code_009_0XmEQc2xOf",
    "TCACA_code_023_4xbGhMrE6q", "TCACA_code_026_BaESVICNoi",
    "TCACA_code_027_0FCGVA6vSa", "TCACA_code_028_NtpWi0jzXs",
    "TCACA_code_029_6wCGEWquYy", "TCACA_code_030_BjSt89qTvr",
    "TCACA_code_035_ArVxJcGDsm", "TCACA_code_036_lupO5WgNdG",
    "TCACA_code_037_WxOD3MpI2o", "TCACA_code_038_OhvqZtiPKr",
    "TCACA_code_039_KRcQj7wUat", "TCACA_code_040_mi9rCYg46x",
]

def _wb_base(domain):
    """domain → API base。国内版与 codebuddy.cn 都走 www.workbuddy.cn。"""
    return {
        "www.codebuddy.cn": "https://www.workbuddy.cn",
        "www.workbuddy.cn": "https://www.workbuddy.cn",
        "www.workbuddy.ai": "https://www.workbuddy.ai",
    }.get(domain, "https://www.workbuddy.cn")


def _wb_ms(v):
    """把到期字段统一成毫秒：数字/数字串→毫秒；'YYYY-MM-DD HH:MM:SS'→毫秒；失败→0。"""
    if v is None:
        return 0
    if isinstance(v, (int, float)):
        return int(v)
    s = str(v).strip()
    if not s:
        return 0
    if s.isdigit():
        return int(s)
    for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d"):
        try:
            return int(dt.datetime.strptime(s, fmt).timestamp() * 1000)
        except ValueError:
            continue
    return 0


def _wb_fetch_pack_expiry(base, token, uid, path):
    """调 free/paid-packages 拿逐笔资源（含到期日）。失败返回 []，不阻断余额读取。

    本机网络 / TLS 拦截层偶发抖动（实测同一请求会间歇性失败），故重试一次再放弃；
    拿不到只影响逐笔到期日（退「未给出」），余额本身来自另一个接口、不受牵连。
    """
    body = json.dumps({
        "PageNumber": 1, "PageSize": 200, "PackageCodes": WORKBUDDY_PACK_CODES,
        "Status": [0, 3], "NeedRenewInfo": True,
    }).encode()
    for _attempt in (1, 2):
        req = UrlRequest(base + path, data=body, headers={
            "Content-Type": "application/json", "Accept": "application/json",
            "Authorization": f"Bearer {token}", "X-User-Id": uid,
            "User-Agent": "WorkBuddy/1.0",
        })
        try:
            with direct_open(req, timeout=15) as r:
                if r.status != 200:
                    continue
                d = json.loads(r.read().decode("utf-8", "replace"))
            return (d.get("data") or {}).get("Accounts") or []
        except Exception:      # noqa: BLE001  拿不到到期日不影响余额
            continue
    return []


def _wb_pack_accounts(base, token, uid):
    """free + paid 两个接口的逐笔账目合到一张表上，带 _paid 标记。

    一次调用，既给 _wb_pack_expiry 算最近到期，也给 _wb_pack_items 出逐笔明细——
    两个接口各只发一次请求，别为一个账号发四遍。
    """
    rows = []
    for path, paid in (("/billing/meter/get-user-resource-free-packages", False),
                       ("/billing/meter/get-user-resource-paid-packages", True)):
        for a in _wb_fetch_pack_expiry(base, token, uid, path):
            a["_paid"] = paid
            rows.append(a)
    return rows


def _wb_cycle_end(a):
    """这一笔哪天作废。CycleEndTime = 本轮周期结束（循环包到这里未用的额度就没了）；
    DeductionEndTime = 权益扣减窗口结束（循环包给的是 2034 那种哨兵值）。
    取两者较早的一个：一次性资源两值相等，循环包则取到本轮作废日。"""
    cand = [x for x in (_wb_ms(a.get("CycleEndTime")), _wb_ms(a.get("DeductionEndTime"))) if x > 0]
    return min(cand) if cand else 0


def _wb_category(a):
    """WorkBuddy 官方用量弹窗那三档：套餐基础积分 / 平台奖励积分 / 购买积分。
    判据来自账目自己的产品名——赠送包（SubProductCode 带 bonus_pack）算平台奖励，
    paid 接口回来的算购买，剩下的（体验版本体那笔）才是套餐基础积分。"""
    if a.get("_paid"):
        return "购买积分"
    tag = f"{a.get('SubProductName') or ''}{a.get('SubProductCode') or ''}".lower()
    if "赠送" in tag or "bonus" in tag:
        return "平台奖励积分"
    return "套餐基础积分"


def _wb_num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return 0.0


def _wb_pack_items(accounts):
    """PackageCode → 逐笔明细（到期时间精确到分）。

    一个包码底下可以有几十笔子账（007 裂变包实测 64 笔），各自到期时间不同——
    官方「查看明细」列的就是这些，只给汇总数会让人以为一笔能用很久。
    """
    out = {}
    for a in accounts:
        code = a.get("PackageCode") or ""
        size = _wb_num(a.get("CycleCapacitySizePrecise") or a.get("CycleCapacitySize"))
        remain = _wb_num(a.get("CycleCapacityRemainPrecise") or a.get("CycleCapacityRemain"))
        used = _wb_num(a.get("CycleCapacityUsedPrecise") or a.get("CycleCapacityUsed"))
        if used <= 0 and size >= remain:
            used = size - remain
        grant = ""
        for at in a.get("AccountAttributes") or []:
            if at.get("Key") == "grantReason":
                grant = str(at.get("Value") or "")
                break
        out.setdefault(code, []).append({
            "label": a.get("PackageName") or code or "积分",
            "detail": grant,
            "total": size,
            "used": used,
            "left": remain,
            "expires_on": dt_text(_wb_cycle_end(a)),
            "unit": a.get("CapacityUnit") or "credits",
        })
    return out


def _wb_pack_expiry(accounts):
    """每个 PackageCode 取「有效笔」中最早的到期日（毫秒），供逐笔过期提醒用。

    Status!=0（已用完）不计——那笔已经不占额度了，提醒它只会添乱。
    """
    nearest = {}
    for a in accounts:
        if a.get("Status") != 0:              # 0=有效，3=已用完；只算还在的
            continue
        code = a.get("PackageCode") or ""
        exp = _wb_cycle_end(a)
        if exp > 0 and (code not in nearest or exp < nearest[code]):
            nearest[code] = exp
    return nearest


def workbuddy_full(intl=False, profile=None, live=True):
    """WorkBuddy 服务端实时余额。返回与 qoder_credit 同形：{data, who, source, auth_from}。"""
    # A staged .info file selects one saved account. Never substitute the current
    # desktop login when that explicit file is missing or expired.
    auth_file = profile if profile and str(profile).lower().endswith('.info') else None
    login, err = _wb_read_login(intl, auth_file=auth_file)
    if login is None:
        return {"error": err}
    got, live_err = _wb_fetch_usage(login["domain"], login["token"], login["uid"])
    if got is None:
        return {"error": f"额度接口读不到（{live_err}）", "auth_from": login["from"]}
    packs = got.get("Packages") or []
    total = sum(float(p.get("CycleTotalCapacity") or 0) for p in packs)
    remain = sum(float(p.get("CycleRemainCapacity") or 0) for p in packs)
    used = sum(float(p.get("CycleUsedCapacity") or 0) for p in packs)
    # 逐笔到期日与逐笔明细：summary 接口都不给，补调 free/paid-packages 取回
    # （失败则空表，逐笔退「未给出」，余额本身不受牵连）
    accs = _wb_pack_accounts(_wb_base(login["domain"]), login["token"], login["uid"])
    pack_exp = _wb_pack_expiry(accs)
    pack_items = _wb_pack_items(accs)
    cat_by_code = {}
    for a in accs:
        code = a.get("PackageCode") or ""
        if code:
            cat_by_code.setdefault(code, _wb_category(a))
    dedicated = []
    for p in packs:
        t = float(p.get("CycleTotalCapacity") or 0)
        r = float(p.get("CycleRemainCapacity") or 0)
        u = float(p.get("CycleUsedCapacity") or 0)
        if t <= 0 and u <= 0:
            continue
        code = p.get("PackageCode") or ""
        dedicated.append({
            "name": code[:24] or "套餐",
            "total": t, "used": u, "remaining": r, "unit": "credits",
            # 该包「最近会作废的一笔」的到期日（毫秒）；取不到 = 0 → 界面显示「未给出」
            "expiresAt": pack_exp.get(code, 0),
            "category": cat_by_code.get(code, "平台奖励积分"),
            "items": pack_items.get(code, []),
        })
    shaped = {
        "userId": login["uid"],
        "userType": "pro" if got.get("IsPaidUser") else "credits",
        # 不设 userQuota：WorkBuddy 的额度就是各套餐之和，设了会跟 dedicatedResourcePackages 重复加总
        "isQuotaExceeded": remain <= 0 if total else False,
        "totalUsagePercentage": (used / total) if total else 0,
        "expiresAt": 0,
        "dedicatedResourcePackages": dedicated,
        # 官方用量弹窗顶部那个套餐名（体验版 / 专业版…）——只用来标身份，不参与求和
        "subscriptionPackageName": got.get("SubscriptionPackageName") or "",
    }
    who = {"name": login["nickname"], "userId": login["uid"]}
    return {
        "data": shaped, "who": who, "source": "server",
        "auth_from": login["from"], "auth_kind": "wbEncrypted",
    }


def workbuddy_intl_full(profile=None, live=True):
    """WorkBuddy 国际版（workbuddy-desktop-ai.info）服务端实时余额。"""
    return workbuddy_full(intl=True, profile=profile, live=live)


def workbuddy_credit(profile=None):
    """WorkBuddy 本地主库 workbuddy.db 的 session_usage.credit_json 记着每个会话按模型消耗的
    credits——聚合起来就是这个档案目录的累计已用。余额/到期日仍要走 copilot.tencent.com 的
    billing 接口（凭据不落明文），本地拿不到。只读，不写任何文件。

    profile 必须尊重：WorkBuddy 靠 junction 把默认档案目录指到某个账号，每个账号目录里都有
    自己的一份 workbuddy.db。早先这里写死 ~/.workbuddy，--profile 传了也白传——当时 junction
    指向谁的档案就算谁的数，换号之后这张卡的会话数会跳到别的账号上。"""
    base = Path(profile).expanduser() if profile else Path.home() / ".workbuddy"
    db = base / "workbuddy.db"
    if not db.is_file():
        return {"error": f"没找到 {db}（这个档案目录里没有 WorkBuddy 的使用库，"
                         f"该账号可能还没在这台机器上用过 WorkBuddy）"}
    try:
        # 用 as_uri() 拼 file: URI：账号目录里带空格、中文、#、% 都可能让手写的
        # "file:<path>?mode=ro" 指到别处去（sqlite 会对路径做百分号解码）
        con = sqlite3.connect(db.absolute().as_uri() + "?mode=ro", uri=True)
        try:
            rows = con.execute(
                "select used, updated_at, credit_json from session_usage").fetchall()
        finally:
            con.close()
    except sqlite3.Error as e:
        return {"error": f"读不了 {db}：{e}"}
    if not rows:
        return {"error": "session_usage 是空的（可能刚装好还没聊过）"}
    per_model, used_tokens, last_ts = {}, 0, 0
    for used, ts, cj in rows:
        if used:
            used_tokens += used
        if ts and ts > last_ts:
            last_ts = ts
        if not cj:
            continue
        try:
            d = json.loads(cj)
        except (ValueError, TypeError):
            continue
        for k, v in d.items():
            try:
                per_model[k] = per_model.get(k, 0.0) + float(v)
            except (TypeError, ValueError):
                pass
    used_credits = round(sum(per_model.values()), 2)
    top = sorted(per_model.items(), key=lambda x: -x[1])[:5]
    last = dt.datetime.fromtimestamp(last_ts / 1000).strftime("%Y-%m-%d %H:%M") if last_ts else ""
    return {"data": {
        "used_credits": used_credits, "sessions": len(rows),
        "used_tokens": used_tokens, "last_active": last,
        "db": str(db),
        "top_models": [{"id": k, "used": round(v, 2)} for k, v in top],
    }}


# WorkBuddy 的按天消耗来自本地主库：session_usage.credit_json 逐会话记了各资源包
# 扣掉多少 credits，配合 sessions 的时间就能聚成日粒度。
# 其余各家能不能聚、聚出来是什么单位，一律看下面的 daily_trend 那张表。


def wb_daily(profile=None, intl=False, days=365):
    """按天聚合 WorkBuddy 的 credits 消耗——驾驶舱波形图与热力图的数据源。

    归日只能用会话的最后活动时间：session_usage 没有逐笔时刻，一场跨天的会话会被
    整笔算到最后那天，所以界面必须写明"按会话最后活动时间归日"，不能当逐笔流水看。
    只读 workbuddy.db，不联网、不写任何文件；读不到就返回 error，不牵连整轮额度读数。
    profile 优先：将来 WorkBuddy 真按账号隔离档案时，每个目录里各有一份 workbuddy.db。
    """
    base = Path(profile).expanduser() if profile else \
        Path.home() / (".workbuddy-ai" if intl else ".workbuddy")
    db = base / "workbuddy.db"
    if not db.is_file():
        return {"error": f"没找到 {db}（这个档案目录里没有 WorkBuddy 的使用库）"}
    try:
        con = sqlite3.connect(db.absolute().as_uri() + "?mode=ro", uri=True)
        try:
            rows = con.execute(
                "select su.credit_json, su.updated_at, s.updated_at "
                "from session_usage su left join sessions s on s.id = su.session_id"
            ).fetchall()
        finally:
            con.close()
    except sqlite3.Error as e:
        return {"error": f"读不了 {db}：{e}"}
    cutoff = dt.date.today() - dt.timedelta(days=max(days, 1) - 1)
    by_day = {}
    for cj, su_ts, s_ts in rows:
        if not cj:
            continue
        try:
            credits = sum(float(v) for v in json.loads(cj).values())
        except (ValueError, TypeError):
            continue
        ts = s_ts or su_ts
        if not ts:
            continue
        d = dt.datetime.fromtimestamp(ts / 1000).date()
        if d < cutoff:
            continue
        cur = by_day.setdefault(d, [0.0, 0])
        cur[0] += credits
        cur[1] += 1
    out = [{"date": d.strftime("%Y-%m-%d"), "credits": round(v[0], 3), "sessions": v[1]}
           for d, v in sorted(by_day.items())]
    if not out:
        return {"error": f"{db} 近 {days} 天没有消耗记录"}
    total = round(sum(x["credits"] for x in out), 3)
    peak = max(out, key=lambda x: x["credits"])
    return {"data": {
        "unit": "credits", "days": out, "total": total,
        "from": out[0]["date"], "to": out[-1]["date"],
        "peak": peak["credits"], "peak_date": peak["date"],
        "db": str(db), "attribution": "session-last-activity",
    }}


# ---------------------------------------------------------------------------
# 其余几家的「按天消耗」。统一放在这一个出口后面：驾驶舱只读 quota --json 里的
# trend 一个键，哪家有哪家没有、单位是积分还是 token，都在这一个地方判。
# ---------------------------------------------------------------------------

def _astudio_cookie(sess):
    """AStudio 的 agent.xfyun.cn 认的是这四个 cookie（登录态明文落在
    「AStudio Data/userdata/astron-session.json」）。同一个 ssoSessionId 要
    同时按驼峰和下划线两种写法给，服务端两处各读各的。"""
    sid = sess.get("ssoSessionId") or sess.get("token")
    return "; ".join(x for x in [
        f"ssoSessionId={sid}" if sid else "",
        f"sso_sessionid={sid}" if sid else "",
        f"account_id={sess.get('accountId') or sess.get('uid') or ''}",
        f"token={sess.get('token') or ''}",
    ] if x)


def astudio_daily(profile=None, days=365):
    """ASTudio 积分按天消耗：points/details?direction=DEBIT 是逐笔扣减流水，
    每笔带 createTime（本地时间、串上没有时区标记），归日直接取前 10 个字符。
    DEBIT 里会混进 EXPIRE/REFUND/MIGRATE，只算 operation=CONSUME——
    把过期作废算成"用掉了"会凭空多出消耗。单位是积分，不是 credits。"""
    sess = None
    for d in _astudio_candidates(profile):
        info, _err = _astudio_read_session(d)
        if info:
            sess = info
            break
    if sess is None:
        return {"error": "没找到 AStudio 的登录档案（AStudio Data/userdata/astron-session.json）"}
    cookie = _astudio_cookie(sess)
    cutoff = dt.date.today() - dt.timedelta(days=max(days, 1) - 1)
    by_day, page, total, err = {}, 1, None, ""
    while page <= 40:
        url = ("https://agent.xfyun.cn/xingchen-studio/points/details?"
               + urlencode({"direction": "DEBIT", "pageNum": page, "pageSize": 100}))
        req = UrlRequest(url, headers={"Accept": "application/json", "Cookie": cookie,
                                       "User-Agent": "agenthub"})
        try:
            with direct_open(req, timeout=12) as r:
                d = json.loads(r.read().decode("utf-8", "replace"))
        except Exception as e:      # noqa: BLE001
            err = f"第 {page} 页没读到：{type(e).__name__}: {e}"
            break
        # 讯飞鉴权失败也回 HTTP 200，只看状态会漏
        if not isinstance(d, dict) or d.get("flag") is not True or d.get("code") != 0:
            err = f"服务端返回异常：{str(d)[:120]}"
            break
        dd = d.get("data") or {}
        items = dd.get("items") or []
        if total is None:
            total = int(dd.get("total") or 0)
        for it in items:
            if (it.get("operation") or "").upper() != "CONSUME":
                continue
            day = str(it.get("createTime") or "")[:10]
            if len(day) != 10:
                continue
            try:
                if dt.date.fromisoformat(day) < cutoff:
                    continue
            except ValueError:
                continue
            cur = by_day.setdefault(day, [0.0, 0])
            try:
                cur[0] += float(it.get("amount") or 0)
            except (TypeError, ValueError):
                pass
            cur[1] += 1
        page += 1
        if not items or len(by_day) >= total:
            break
    out = [{"date": k, "credits": round(v[0], 4), "count": v[1]}
           for k, v in sorted(by_day.items())]
    if not out:
        return {"error": err or "近一年没有 CONSUME 记录（DEBIT 流水是空的）"}
    return {"data": {
        "unit": "积分", "days": out, "total": round(sum(x["credits"] for x in out), 4),
        "peak": max(x["credits"] for x in out),
        "peak_date": max(out, key=lambda x: x["credits"])["date"],
        "from": out[0]["date"], "to": out[-1]["date"],
        "source": "讯飞积分流水接口",
    }}


def minimax_daily(profile=None, days=365):
    """MiniMax Code 按天 token 消耗：官方 token 计划用量接口本身就按天分好，
    逐日还分模型（客户端那张热力图就是读它）。**积分口径拿不到**——
    credit/details 的 direction 参数实测被完全忽略，只有累计 consumed_amount、没有消耗时刻。
    所以这张图只能画 token，单位照实写 token。"""
    files = _minimax_auth_files(profile)
    if not files:
        return {"error": "没找到 MiniMax Code 的凭据（~/.minimax/auth/**/mcode-public/auth.json）"}
    try:
        cred = json.loads(files[0][0].read_text(encoding="utf-8"))
        key = next(iter(cred.get("records") or {}), None)
        token = (cred["records"][key].get("accessToken") or "") if key else ""
    except Exception as e:      # noqa: BLE001
        return {"error": f"读不了 {files[0][0]}：{e}"}
    if not token:
        return {"error": "凭据里没有 access token"}
    req = UrlRequest("https://www.minimaxi.com/backend/account/token_plan/usage_summary",
                     headers={"Accept": "application/json", "Authorization": f"Bearer {token}"})
    try:
        with direct_open(req, timeout=12) as r:
            d = json.loads(r.read().decode("utf-8", "replace"))
    except Exception as e:      # noqa: BLE001
        return {"error": f"用量接口没读到：{type(e).__name__}: {e}"}
    dd = d.get("data") or d or {}
    rows = dd.get("date_model_usage")
    if not isinstance(rows, list):
        # 形状对不上就如实报，绝不猜一个字段名把图画出来
        return {"error": f"用量接口字段形状对不上，实际键：{sorted(dd.keys())[:12]}"}
    out = []
    for it in rows:
        day = str(it.get("date") or "")[:10]
        v = it.get("total_token")
        if len(day) != 10 or not isinstance(v, (int, float)):
            continue
        out.append({"date": day, "credits": float(v), "count": len(it.get("models") or [])})
    if not out:
        return {"error": "这份用量里一条按天记录都没有（可能还没订阅 token 计划）"}
    top = max(out, key=lambda x: x["credits"])
    return {"data": {
        "unit": "token", "days": out, "total": round(sum(x["credits"] for x in out), 2),
        "peak": top["credits"], "peak_date": top["date"],
        "from": out[0]["date"], "to": out[-1]["date"],
        "source": "MiniMax 官方按天用量接口（只有 token 口径，积分口径官方不给）",
    }}


def zcode_daily(profile=None, days=365):
    """ZCode 按天 token 消耗：本地库 model_usage 一次请求一行，started_at 是毫秒。
    **它整库没有 credits/cost 列**，只能画 token——单位写错量级就差几个数量级。
    status=error 的那些行 token 本来就是 0（含 exceed quota limit），照常计入不影响求和。"""
    root = _zcode_root(profile)
    db = root / "cli" / "db" / "db.sqlite"
    if not db.is_file():
        return {"error": f"没找到 {db}"}
    try:
        con = sqlite3.connect(db.absolute().as_uri() + "?mode=ro", uri=True)
        try:
            rows = con.execute(
                "select started_at, input_tokens, output_tokens, reasoning_tokens, "
                "cache_read_input_tokens, cache_creation_input_tokens, session_id "
                "from model_usage").fetchall()
        finally:
            con.close()
    except sqlite3.Error as e:
        return {"error": f"读不了 {db}：{e}"}
    cutoff = dt.date.today() - dt.timedelta(days=max(days, 1) - 1)
    by_day = {}
    for ts, i, o, rs, cr, cc, sess in rows:
        if not ts:
            continue
        try:
            d = dt.datetime.fromtimestamp(int(ts) / 1000).date()
        except (TypeError, ValueError, OSError):
            continue
        if d < cutoff:
            continue
        tot = sum(int(x or 0) for x in (i, o, rs, cr, cc))
        cur = by_day.setdefault(d.isoformat(), [0, set()])
        cur[0] += tot
        if sess:
            cur[1].add(sess)
    out = [{"date": k, "credits": float(v[0]), "sessions": len(v[1])}
           for k, v in sorted(by_day.items()) if v[0] > 0]
    if not out:
        return {"error": f"{db} 近 {days} 天没有 token 记录"}
    top = max(out, key=lambda x: x["credits"])
    return {"data": {
        "unit": "token", "days": out, "total": sum(x["credits"] for x in out),
        "peak": top["credits"], "peak_date": top["date"],
        "from": out[0]["date"], "to": out[-1]["date"],
        "source": "ZCode 本地请求库 model_usage",
    }}


def _zstd_bytes(data):
    """解一段 zstd。三级：Python 3.14+ 的 compression.zstd → pip 的 zstandard →
    外部 zstd.exe（走 stdin/stdout，不落临时文件）。
    **必须留第三条**：agenthub 的 .cmd 包装器跑的是 ~/.agenthub/venv 那个 Python 3.13，
    前两条都没有，而驾驶舱调的正是这个包装器——只写前两条的话 DSH 那张图在程序里永远出不来。"""
    try:
        from compression import zstd as _z
        return _z.decompress(data)
    except Exception:       # noqa: BLE001
        pass
    try:
        import zstandard as _z2
        return _z2.ZstdDecompressor().decompress(data)
    except Exception:       # noqa: BLE001
        pass
    import shutil
    import subprocess
    exe = shutil.which("zstd")
    if exe:
        kwargs = {}
        if os.name == "nt":
            kwargs["creationflags"] = 0x08000000      # CREATE_NO_WINDOW
        try:
            r = subprocess.run([exe, "-dc"], input=data, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, timeout=60, **kwargs)
        except Exception as e:      # noqa: BLE001
            raise RuntimeError(f"zstd.exe 跑失败：{type(e).__name__}: {e}") from e
        if r.returncode == 0 and r.stdout:
            return r.stdout
        raise RuntimeError(f"zstd.exe 退出码 {r.returncode}，没解出内容")
    raise RuntimeError("本机没有 zstd 解压能力（compression.zstd / pip zstandard / zstd.exe 都没有）")


def dsh_daily(profile=None, days=365):
    """DeepSeek Harness 按天 token：会话日志 session.v4.jsonl.zstd 里每条
    assistant/message 自带 data.usage.totalTokens 与毫秒 time，逐条归日，
    比 WorkBuddy 那种"按会话最后活动时间整笔归一天"更硬。
    单位是 token——DSH 的钱包是人民币元，这里画的不是钱。"""
    root = Path(profile) if profile else (Path.home() / ".dsh")
    files = sorted((root / "sessions").glob("*/*/session.v4.jsonl.zstd")) \
        if (root / "sessions").is_dir() else []
    if not files:
        return {"error": f"{root / 'sessions'} 下没有 session.v4.jsonl.zstd（这台机器没跑过 DSH 会话）"}
    cutoff = dt.date.today() - dt.timedelta(days=max(days, 1) - 1)
    by_day = {}
    why = ""
    for f in files:
        try:
            raw = _zstd_bytes(f.read_bytes())
        except Exception as e:      # noqa: BLE001
            why = str(e)[:120]
            continue
        sess = f.parent.name
        for line in raw.decode("utf-8", "replace").splitlines():
            if '"usage"' not in line:
                continue
            try:
                j = json.loads(line)
            except ValueError:
                continue
            if j.get("type") != "assistant/message":
                continue
            u = (j.get("data") or {}).get("usage")
            ts = j.get("time")
            if not isinstance(u, dict) or not ts:
                continue
            try:
                d = dt.datetime.fromtimestamp(int(ts) / 1000).date()
            except (TypeError, ValueError, OSError):
                continue
            if d < cutoff:
                continue
            cur = by_day.setdefault(d.isoformat(), [0, set()])
            cur[0] += int(u.get("totalTokens") or 0)
            cur[1].add(sess)
    out = [{"date": k, "credits": float(v[0]), "sessions": len(v[1])}
           for k, v in sorted(by_day.items()) if v[0] > 0]
    if not out:
        return {"error": f"{len(files)} 个会话日志里没有带 usage 的消息"
                         + (f"（解压这一步就失败了：{why}）" if why else "")}
    top = max(out, key=lambda x: x["credits"])
    return {"data": {
        "unit": "token", "days": out, "total": sum(x["credits"] for x in out),
        "peak": top["credits"], "peak_date": top["date"],
        "from": out[0]["date"], "to": out[-1]["date"],
        "source": "DSH 本机会话日志逐条 usage",
    }}


def codex_daily(profile=None, days=365):
    """Codex CLI 按天 token：rollout jsonl 里 token_count 事件带
    `payload.info.last_token_usage`（这一段的增量）与 UTC 的 `timestamp`。
    **只用增量**：`total_token_usage` 是文件内累计，会被压缩/续跑打断，
    本机实测"每文件末值之和"193.7M、"增量之和"211.7M、threads 表 207.1M，三者互不相等。
    行内时间是 UTC 而目录名是本地日期，必须先转本地时区再归日。"""
    root = Path(profile) if profile else (Path.home() / ".codex")
    sess = root / "sessions"
    files = sorted(sess.glob("**/*.jsonl")) if sess.is_dir() else []
    if not files:
        return {"error": f"{sess} 下没有会话日志（这台机器还没用过 Codex CLI）"}
    cutoff = dt.date.today() - dt.timedelta(days=max(days, 1) - 1)
    by_day = {}
    for f in files:
        try:
            fh = open(f, encoding="utf-8", errors="replace")
        except OSError:
            continue
        with fh:
            for line in fh:
                if '"token_count"' not in line:
                    continue
                try:
                    j = json.loads(line)
                except ValueError:
                    continue
                p = j.get("payload") or {}
                if p.get("type") != "token_count":
                    continue
                inc = (p.get("info") or {}).get("last_token_usage") or {}
                ts = j.get("timestamp")
                if not inc or not ts:
                    continue
                try:
                    d = dt.datetime.fromisoformat(str(ts).replace("Z", "+00:00")).astimezone().date()
                except ValueError:
                    continue
                if d < cutoff:
                    continue
                tot = int(inc.get("total_tokens") or 0) or sum(
                    int(inc.get(k) or 0) for k in
                    ("input_tokens", "output_tokens", "cached_input_tokens",
                     "reasoning_output_tokens", "cache_write_input_tokens"))
                if tot <= 0:
                    continue
                cur = by_day.setdefault(d.isoformat(), [0, set()])
                cur[0] += tot
                cur[1].add(f.name)
    out = [{"date": k, "credits": float(v[0]), "sessions": len(v[1])}
           for k, v in sorted(by_day.items()) if v[0] > 0]
    if not out:
        return {"error": f"{len(files)} 个日志文件里没有可用的 token 增量"}
    top = max(out, key=lambda x: x["credits"])
    return {"data": {
        "unit": "token", "days": out, "total": sum(x["credits"] for x in out),
        "peak": top["credits"], "peak_date": top["date"],
        "from": out[0]["date"], "to": out[-1]["date"],
        "source": "Codex 本机会话日志逐条增量",
    }}


def marvis_daily(profile=None, days=365):
    """Marvis 按天 token：本地库 llm_token_usage 每行自带 usage_date（厂商已经按本地日切好），
    直接 group by。注意 .marvis 是指向 %APPDATA%\\Tencent\\Marvis\\User\\<openid> 的符号链接，
    链接目标里编着账号，是产品自己换号的机制，所以要 resolve 之后再开库。"""
    cand = Path(profile) if profile else (Path.home() / ".marvis")
    try:
        cand = cand.resolve()
    except OSError:
        pass
    db = cand / "database" / "data.db"
    if not db.is_file():
        return {"error": f"没找到 {db}"}
    try:
        con = sqlite3.connect(db.absolute().as_uri() + "?mode=ro", uri=True)
        try:
            rows = con.execute(
                "select usage_date, total_tokens, conversation_id from llm_token_usage"
            ).fetchall()
        finally:
            con.close()
    except sqlite3.Error as e:
        return {"error": f"读不了 {db}：{e}"}
    cutoff = (dt.date.today() - dt.timedelta(days=max(days, 1) - 1)).isoformat()
    by_day = {}
    for day, tot, conv in rows:
        day = str(day or "")[:10]
        if len(day) != 10 or day < cutoff:
            continue
        cur = by_day.setdefault(day, [0, set()])
        cur[0] += int(tot or 0)
        if conv:
            cur[1].add(conv)
    out = [{"date": k, "credits": float(v[0]), "sessions": len(v[1])}
           for k, v in sorted(by_day.items()) if v[0] > 0]
    if not out:
        return {"error": f"{db} 近 {days} 天没有 token 记录"}
    top = max(out, key=lambda x: x["credits"])
    return {"data": {
        "unit": "token", "days": out, "total": sum(x["credits"] for x in out),
        "peak": top["credits"], "peak_date": top["date"],
        "from": out[0]["date"], "to": out[-1]["date"],
        "source": "Marvis 本地库 llm_token_usage（厂商已按日切好）",
    }}


def qwenwork_daily(profile=None, days=365):
    """千问办公的按天消耗只能我们自己攒：实测它这台网关上
    Qoder 那三条（cost-center/credits/daily-trend、ai-conversations/credits-heatmap、
    big_model_credits/histories）全部 404，wallets 与 quota/usage 都只是当前值快照。
    所以每次读一遍三档钱包余额，与上次快照比，减少的量记到当天。

    **这是我们记的数，不是厂商给的**：只有驾驶舱轮询过的天数会有值，中间没轮询的日子
    会连成一片零——那不是"没用"，是"没记"。余额变大（每日重置、充值、领赠送）一律
    不当消耗，只把基线挪过去。快照存在我们自己的目录里，不碰产品任何文件。"""
    login = None
    for d in _qwenwork_dirs(profile):
        info, _err = _qwenwork_login(d)
        if info:
            login = info
            break
    if login is None:
        return {"error": "没找到千问办公的登录数据（auth-v2.dat）"}
    data, err = _qwenwork_get(QWENWORK_WALLETS, login["token"])
    if data is None:
        return {"error": f"钱包读不到：{err}"}
    remaining = 0.0
    for k in ("daily", "monthly", "longterm"):
        tier = data.get(k) or {}
        try:
            remaining += float(tier.get("total_balance") or 0)
        except (TypeError, ValueError):
            pass
    snap_dir = Path(os.environ.get("AGENTHUB_HOME") or (Path.home() / ".agenthub"))
    snap = snap_dir / "qwenwork-daily.json"
    today = dt.date.today().isoformat()
    try:
        st = json.loads(snap.read_text(encoding="utf-8")) if snap.is_file() else {}
    except Exception:       # noqa: BLE001
        st = {}
    if not isinstance(st.get("days"), dict):
        st = {"days": {}}
    last = st.get("last")
    if isinstance(last, dict):
        try:
            delta = float(last.get("remaining") or 0) - remaining
        except (TypeError, ValueError):
            delta = 0.0
        if delta > 0:
            key = last.get("at") if last.get("at") == today else today
            try:
                st["days"][key] = round(float(st["days"].get(key) or 0) + delta, 4)
            except (TypeError, ValueError):
                st["days"][key] = round(delta, 4)
    st["last"] = {"at": today, "remaining": remaining}
    try:
        snap_dir.mkdir(parents=True, exist_ok=True)
        tmp = snap.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(st, ensure_ascii=False), encoding="utf-8")
        tmp.replace(snap)
    except OSError as e:
        return {"error": f"快照写不了（{e}），差分记不下"}
    cutoff = (dt.date.today() - dt.timedelta(days=max(days, 1) - 1)).isoformat()
    out = [{"date": k, "credits": float(v)} for k, v in sorted(st["days"].items())
           if k >= cutoff and float(v) > 0]
    if not out:
        return {"error": "刚开始记：至少要被轮询过两次，才会有第一天的消耗"}
    top = max(out, key=lambda x: x["credits"])
    return {"data": {
        "unit": "积分", "days": out, "total": round(sum(x["credits"] for x in out), 4),
        "peak": top["credits"], "peak_date": top["date"],
        "from": out[0]["date"], "to": out[-1]["date"],
        "source": "驾驶舱每日轮询钱包余额自记的差分（不是厂商给的，没轮询的日子是空白）",
    }}


def daily_trend(store, profile=None):
    """按天消耗序列的唯一出口。返回 {"data":…} 或 {"error":…}；
    返回 None = 这个产品目前没有可落地的按天数据源（界面就不出现图）。
    新增一家只在这张表里加一行，别把判据散到调用方去。"""
    if store in ("workbuddy", "workbuddy-intl"):
        return wb_daily(profile=profile, intl=(store == "workbuddy-intl"))
    if store == "astudio":
        return astudio_daily(profile=profile)
    if store == "minimax":
        return minimax_daily(profile=profile)
    if store == "zcode":
        return zcode_daily(profile=profile)
    if store == "dsh":
        return dsh_daily(profile=profile)
    if store == "codex":
        return codex_daily(profile=profile)
    if store == "marvis":
        return marvis_daily(profile=profile)
    if store == "qwenwork":
        return qwenwork_daily(profile=profile)
    return None


# ---------------------------------------------------------------------------
# 第二批：走服务端接口、但凭据落在本机明文的店。
# 这两家的共同点：登录态不加密（AStudio 是明文 JSON；MiniMax 是 OAuth 令牌文件），
# 所以"本机直读 + 调官方接口"完全可行——不像 WorkBuddy/ZCode 那样凭据不落明文。
# ---------------------------------------------------------------------------
ASTUDIO_STORES = {"astudio"}
MINIMAX_STORES = {"minimax"}

# AStudio（科大讯飞「星辰/星火」）：产品自带一个本地内核服务，渲染层调的是
#   127.0.0.1:<port>/api/astron-auth/points-balance；它背后转发到
#   https://agent.xfyun.cn/xingchen-studio/points/balance（Cookie 鉴权）。
# 登录态就写在安装目录同级的「AStudio Data/userdata/astron-session.json」里，明文：
#   {accountId, uid, ssoSessionId, token, modelBearerToken, nickname, mobile}
# 数据目录规则（asar 里 resolveWindowsDataLocationRoot）：<安装目录的上一级>/AStudio Data/userdata
ASTUDIO_BALANCE_URL = "https://agent.xfyun.cn/xingchen-studio/points/balance"
# 明细页 https://agent.xfyun.cn/studio-credits 背后那两个接口（实测 2026-10-11）：
#   /points/details?direction=CREDIT  获得明细，逐笔带自己的 expireTime
#   /membership/me                    套餐名（体验版/标准版/高级版/畅享版）
# pageSize 服务端硬上限 100（101 起返回「参数无效」），不传时间范围＝全生命周期。
ASTUDIO_DETAILS_URL = "https://agent.xfyun.cn/xingchen-studio/points/details"
ASTUDIO_MEMBERSHIP_URL = "https://agent.xfyun.cn/xingchen-studio/membership/me"

# 档位名与说明一律照 AStudio 客户端「订阅和积分」页的原话，不许自己另起一套：
# 那页就两张卡，「通用积分」和「星火专属积分」，卡上的说明文字正好是"能花在什么上"，
# 拿它当 scope 传给界面，星火那档才会被标成"只能喂讯飞模型"而不是又一堆积分。
ASTUDIO_ASSET = {
    "GENERAL_POINTS": ("通用积分", "用于所有模型，以及插件、Skill 调用等费用"),
    "SPARK_POINTS": ("星火专属积分", "仅用于星火（讯飞）模型的消耗，不含插件、Skill 调用等其他费用"),
}
# 「入账」列的第二段（明细页 bundle 里的 kY 表）：同一笔积分是套餐发的、活动送的还是买的
ASTUDIO_ENTRY = {
    "GENERAL_POINTS": {"PURCHASE": "购买积分", "MEMBERSHIP": "套餐基础", "ACTIVITY": "平台奖励",
                       "CMP": "平台奖励", "REDEEM_CODE": "平台奖励"},
    "SPARK_POINTS": {"PURCHASE": "购买积分", "MEMBERSHIP": "套餐赠送", "ACTIVITY": "活动赠送",
                     "CMP": "活动赠送", "REDEEM_CODE": "活动赠送"},
}

# MiniMax Code：OAuth2 设备码登录，令牌是 mmoat_/mmort_ 前缀，落
#   ~/.minimax/auth/<buildEnv>/<region>/mcode-public/auth.json（+ auth-state.json）
# 积分接口是网页端那条 www.minimaxi.com/backend/account/token_plan_credit（Bearer 鉴权）。
# ⚠ refresh_token 是「轮换 + 重放检测」的：用它换过一次新令牌后，旧令牌立刻作废；
#   若再拿旧的重放，服务端会把整族令牌一并撤销（需重新登录）。所以这里刷新成功后
#   必须立刻把新令牌写回两个文件，且绝不对同一个 refresh_token 重试第二次。
MINIMAX_TOKEN_URL = "https://account.minimax.cn/oauth2/token"
# 积分明细（客户端「积分明细」弹层的数据源，实测 2026-10-11）：逐笔给获得时间/类别/总量/
# 剩余/到期时间。旧的 www.minimaxi.com/backend/account/token_plan_credit 是「Token 订阅
# 计划」那一套，本账号在那儿恒为 0/0，签到积分一条都不在里面——别再往那儿找。
MINIMAX_CREDIT_URL = "https://agent.minimax.cn/minimax-cloud/api/v1/credit/details"
MINIMAX_WEB_ORIGIN = "https://agent.minimax.cn"
# 厂商 asar 里的枚举（模块 87905）：Purchased=1、Free=2，界面上分别印成"购买""签到"
_MINIMAX_CREDIT_TYPE = {1: "购买", 2: "签到"}
MINIMAX_CLIENT_ID = "mcode-public"


def _minimax_user_id():
    """sharedUser.realUserID：只当查询参数用，拿不到也不影响读数。"""
    try:
        p = (Path(os.environ.get("APPDATA") or (Path.home() / "AppData" / "Roaming"))
             / "MiniMax" / "minimax-agent-cn-config.json")
        return str((json.loads(p.read_text(encoding="utf-8")).get("sharedUser") or {})
                   .get("realUserID") or "")
    except Exception:       # noqa: BLE001
        return ""


def _iso_ms(s):
    """把 ISO 日期串（2026-11-02T23:56:28 / 2026-11-02）换成毫秒时间戳；认不出就 0。"""
    if not s:
        return 0
    if isinstance(s, (int, float)):
        return int(s) if s > 10 ** 11 else int(s) * 1000
    t = str(s).strip().replace("Z", "+00:00")
    for fmt in ("%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"):
        try:
            return int(dt.datetime.strptime(t[:len(fmt) + 4 if "T" in fmt else len(fmt)],
                                            fmt).timestamp() * 1000)
        except ValueError:
            continue
    try:
        return int(dt.datetime.fromisoformat(t).timestamp() * 1000)
    except ValueError:
        return 0


def _astudio_allocate(grants, debits, now_ms):
    """把消耗流水摊派到每个积分包上，算出逐包剩余（厂商自己不给这个数）。

    厂商的「获得明细」只给每个包发了多少、各自什么时候到期；消耗明细里
    deductions[] 只带 {amount, asset_code}，**不带 grantId**——所以「哪一笔消耗
    扣的是哪个包」官方没讲。本函数按**先到期先扣（FEFO）**摊派，并当场用官方
    余额自证：对不上就返回 None，界面继续显示「官方只给了获得数」，绝不显示假数。

    FEFO 不是猜的，有一条可证伪的判据（实测 2026-11-01）：
      官方 balance 给了 activityNextExpireTime = 「下一个还有余额的包」的到期时间。
      按 FEFO 摊派后推出来的值是 2026-11-09T00:16:04，与官方**精确到秒一致**；
      按 FIFO（先到账先扣）推出来的是 2026-11-03T00:01:08，差 6 天。
      也就是说厂商确实是先扣快到期的那个包。

    扣款时**不看包是否已过期**：厂商扣的是「当前还剩着余额的最早到期那个包」，
    过期是另一件事。实测 2026-10-31 那批 757 分消耗，扣掉的是 10-02 到账、
    11-02 到期的那几个包——它们在消耗发生时都还没过期，随后才到期作废。
    若按「消耗当时已过期就跳过」来摊，757 会扣到更晚到账的包上，
    未过期余额就凑不出官方的 8343。

    过期作废：官方余额只算**没过期**的包（实测星火档流水净额 61162，
    官方余额 35000，差额 26162 正是已过期那几包被作废掉的）。
    流水里没有任何 EXPIRE 记录，所以作废这件事只能靠 expireTime 判定。

    grants:  逐笔获得（键名与生产侧一致：asset/total/_exp_ms/_grant_ms）
    debits: 逐笔消耗（沿用原厂字段：assetCode/amount/_use_ms）
    返回: {包下标: {"left": 剩余, "used": 已用}}，对不上账返回 None。
    """
    # 按到期时间排（同到期再按到账时间），这就是 FEFO 的扣减顺序
    order = sorted(
        range(len(grants)),
        key=lambda i: (grants[i].get("_exp_ms") or 0, grants[i].get("_grant_ms") or 0))
    left = {i: _f(grants[i].get("total")) for i in range(len(grants))}
    used = {i: 0.0 for i in range(len(grants))}

    spends = []
    for d in debits:
        amt = _f(d.get("amount"))
        if amt <= 0:
            continue
        spends.append((d.get("_use_ms") or 0, d.get("assetCode") or "", amt))
    spends.sort()

    for t_ms, asset, amt in spends:
        need = amt
        for i in order:
            if need <= 0:
                break
            g = grants[i]
            if (g.get("asset") or "") != asset:
                continue
            if (g.get("_grant_ms") or 0) > t_ms:
                continue          # 这一笔消耗发生时这个包还没到账
            take = min(need, left[i])
            left[i] -= take
            used[i] += take
            need -= take
        # need > 0 意味着扣不完（余额与流水对不上），交由调用方的对账去发现

    return {"left": left, "used": used}


def _astudio_verify_alloc(grants, alloc, asset, official_balance, now_ms):
    """拿推算结果和官方余额对账。对得上才算数，对不上返回 False。

    官方余额只算没过期的包（过期的已作废），所以这里也只加没过期那部分。
    """
    if not alloc:
        return False
    total = 0.0
    for i, g in enumerate(grants):
        if (g.get("asset") or "") != asset:
            continue
        exp = g.get("_exp_ms") or 0
        if exp and exp <= now_ms:
            continue          # 已过期，官方余额里没有它
        total += alloc["left"].get(i, 0.0)
    return abs(total - _f(official_balance)) < 0.01


def _astudio_candidates(profile=None):
    """AStudio 的登录档案目录候选。
    给了 profile（驾驶舱按账号目录读）就只认它（再顺带看一层 userdata）；
    否则扫几个常见安装根下的「AStudio Data/userdata」。"""
    if profile:
        p = Path(profile)
        return [p, p / "userdata"]
    out = []
    env = os.environ.get("ASTUDIO_HOME")
    if env:
        out += [Path(env), Path(env) / "userdata"]
    roots = [Path(os.environ.get("LOCALAPPDATA", "")) / "Programs",
             Path(os.environ.get("LOCALAPPDATA", "")) / "Programs",
             Path(os.environ.get("LOCALAPPDATA", "")),
             Path("C:/Program Files"), Path("C:/Program Files (x86)")]
    for r in roots:
        for name in ("AStudio Data", "AStudio Canary Data", "AStudio Dev Data"):
            out.append(r / name / "userdata")
    return out


def _astudio_read_session(d):
    f = d / "astron-session.json"
    if not f.is_file():
        return None, f"{f} 不存在"
    try:
        j = json.loads(f.read_text(encoding="utf-8"))
    except Exception as e:      # noqa: BLE001
        return None, f"{f} 解不开：{e}"
    if not (j.get("token") or j.get("ssoSessionId")):
        return None, f"{f} 里没有登录令牌（可能还没登录）"
    j["_from"] = str(f)
    return j, None


def astudio_credit(profile=None, live=True):
    """读 AStudio 积分。返回与 qoder_credit 同形的结构（data 是「Qoder 用量」形状），
    这样驾驶舱的渲染/绑定校验链路可以原样复用。只读，不写任何产品文件。

    服务端数据（实测 2026-10-09）：
      data.totalAmount/totalBalance      通用积分（活动/购买）
      data.sparkTotalAmount/sparkTotalBalance  星火积分
      data.activityNextExpireTime        到期时间（ISO）
    """
    sess, tried, last = None, [], "没找到 AStudio 的登录档案目录"
    for d in _astudio_candidates(profile):
        info, err = _astudio_read_session(d)
        tried.append(f"{d}：{err or '读到登录态'}")
        if info:
            sess = info
            break
        last = err or "读不到登录态"
    if sess is None:
        return {"error": f"没找到 AStudio 登录数据（{last}）", "tried": tried}

    auth_from = sess["_from"]
    if not live:
        return {"error": "AStudio 余额只有服务端接口，本地没有可退的缓存",
                "auth_from": auth_from, "auth_kind": "astron-session.json"}
    cookie = _astudio_cookie(sess)
    kind = "astron-session.json"

    def get(url, params=None):
        """带 cookie 打一条 GET。返回 (data, err)——err 说人话，不猜。"""
        full = url + (("?" + urlencode(params)) if params else "")
        req = UrlRequest(full, headers={
            "Accept": "application/json", "Cookie": cookie, "User-Agent": "agenthub",
        })
        try:
            with direct_open(req, timeout=12) as r:
                d = json.loads(r.read().decode("utf-8", "replace"))
        except HTTPError as e:
            body = ""
            try:
                body = e.read().decode("utf-8", "replace")[:160]
            except Exception:       # noqa: BLE001
                pass
            return None, f"HTTP {e.code} {body}".strip()
        except Exception as e:      # noqa: BLE001
            return None, f"{type(e).__name__}: {e}"
        # 讯飞这套接口鉴权失败也回 200，只是 code=80000「登录异常」——只看 HTTP 状态会漏
        if not isinstance(d, dict) or d.get("flag") is not True or d.get("code") != 0:
            return None, f"服务端返回异常：{str(d)[:160]}"
        return d.get("data"), None

    data, err = get(ASTUDIO_BALANCE_URL)
    if data is None:
        return {"error": err, "auth_from": auth_from, "auth_kind": kind}
    plan_name, _perr = get(ASTUDIO_MEMBERSHIP_URL)
    plan_name = (plan_name or {}).get("planName") or ""

    # 逐笔获得明细：厂商的「获得明细」表就是这张，一笔一行、各自一个到期时间。
    # 它**不给逐笔剩余**（DEBIT 的 deductions 只带金额与资产码、不带 grantId），
    # 所以 left 一律留 None——按"用完 0"画会凭空造出一堆红色已过期。
    grants, gnote, page = [], None, 1
    while page <= 20:
        det, derr = get(ASTUDIO_DETAILS_URL,
                        {"direction": "CREDIT", "pageNum": page, "pageSize": 100})
        if det is None:
            gnote = derr
            break
        for it in det.get("items") or []:
            if not isinstance(it, dict):
                continue
            asset = it.get("assetCode") or "GENERAL_POINTS"
            entry = (ASTUDIO_ENTRY.get(asset) or {}).get(it.get("sourceType") or "") or ""
            grants.append({
                "asset": asset,
                "label": it.get("reason") or (it.get("sourceType") or "积分入账"),
                "total": it.get("amount"), "used": None, "left": None,
                "expires_on": dt_text(_iso_ms(it.get("expireTime"))),
                "ms": _iso_ms(it.get("expireTime")),
                "unit": "积分", "detail": entry,
                # 摊派用的内部字段（下面对外输出时会剔掉）
                "_exp_ms": _iso_ms(it.get("expireTime")),
                "_grant_ms": _iso_ms(it.get("createTime")),
            })
        if len(det.get("items") or []) < int(det.get("total") or 0):
            page += 1
        else:
            break

    now_ms = int(time.time() * 1000)

    # 逐笔剩余：官方只给「每个包发了多少」，不给「这个包还剩多少」。
    # 消耗明细是另一条接口（direction=DEBIT），它给每笔消耗的时间与金额，
    # 但 deductions[] 不带 grantId，扣的是哪个包官方没说。这里按先到期先扣摊派，
    # 并用官方余额对账：对不上就整批退回「只给获得数」，不显示推算出来的数。
    debits, dnote = [], None
    dpage = 1
    while dpage <= 20:
        det, derr = get(ASTUDIO_DETAILS_URL,
                        {"direction": "DEBIT", "pageNum": dpage, "pageSize": 100})
        if det is None:
            dnote = derr
            break
        for it in det.get("items") or []:
            if not isinstance(it, dict):
                continue
            it = dict(it)
            it["_use_ms"] = _iso_ms(it.get("createTime"))
            debits.append(it)
        if len(det.get("items") or []) < int(det.get("total") or 0):
            dpage += 1
        else:
            break

    alloc = _astudio_allocate(grants, debits, now_ms) if debits else None
    # 逐档对账：官方余额只算没过期的包，推算也得对得上才敢显示
    alloc_ok = {}
    if alloc:
        for asset in ASTUDIO_ASSET:
            if asset == "GENERAL_POINTS":
                obal = (data.get("memberBalance", 0) + data.get("activityBalance", 0)
                        + data.get("buyBalance", 0))
            else:
                obal = data.get("sparkTotalBalance") or 0
            alloc_ok[asset] = _astudio_verify_alloc(
                grants, alloc, asset, obal, now_ms)
            if not alloc_ok[asset]:
                break

    groups = []
    for asset, (gname, scope) in ASTUDIO_ASSET.items():
        if asset == "GENERAL_POINTS":
            gt, gr = (data.get("memberTotal", 0) + data.get("activityTotal", 0)
                      + data.get("buyTotal", 0),
                      data.get("memberBalance", 0) + data.get("activityBalance", 0)
                      + data.get("buyBalance", 0))
        else:
            gt, gr = (data.get("sparkTotalAmount") or 0, data.get("sparkTotalBalance") or 0)
        gt, gr = _f(gt), _f(gr)
        items = [g for g in grants if g["asset"] == asset]
        live_usable = [g for g in items if g["ms"] > now_ms]
        if gt <= 0 and not items:
            continue        # 这一档这个账号压根没有，别摆一行 0 出来
        # 逐包剩余：只有对过账的那一档才给数
        if alloc and alloc_ok.get(asset):
            for i, g in enumerate(grants):
                if g["asset"] != asset:
                    continue
                left_v = alloc["left"].get(i, 0.0)
                used_v = alloc["used"].get(i, 0.0)
                # 过期的包剩余已被官方作废（流水里没有 EXPIRE 记录，作废不记流水），
                # 显示成「过期但还剩 N」是在画一笔不存在的钱——归零，used 补上。
                if g["ms"] and g["ms"] <= now_ms:
                    used_v += left_v
                    left_v = 0.0
                g["left"] = round(left_v, 3)
                g["used"] = round(used_v, 3)
        groups.append({"name": gname, "scope": scope, "unit": "积分",
                       "total": gt, "used": max(gt - gr, 0.0), "remaining": gr,
                       "expiresAt": min((g["ms"] for g in live_usable), default=0),
                       "items": [{k: v for k, v in g.items()
                                  if k not in ("asset", "ms", "_exp_ms", "_grant_ms")}
                                 for g in items]})

    total = sum(g["total"] for g in groups)
    remain = sum(g["remaining"] for g in groups)
    exps = [g["expiresAt"] for g in groups if g["expiresAt"]]
    shaped = {
        "userId": str(sess.get("accountId") or sess.get("uid") or ""),
        "userType": "astron" + ("·星火会员" if (data.get("sparkMemberTotal") or 0) else ""),
        "subscriptionPackageName": plan_name,
        "expiresAt": min(exps, default=0),
        "isQuotaExceeded": remain <= 0,
        "totalUsagePercentage": ((total - remain) / total) if total else 0,
        # 档名走 vendorGroups，所以这里不再塞 userQuota——cmd_quota 对 buckets 求和，
        # 汇总数与逐档同时进 buckets 会把余额算两遍。
        "vendorGroups": groups,
        "raw": data,
    }
    who = {"name": sess.get("nickname") or sess.get("mobile") or "AStudio",
           "accountId": str(sess.get("accountId") or "")}
    out = {"data": shaped, "who": who, "source": "server",
           "auth_from": auth_from, "auth_kind": kind}
    if gnote:
        out["live_error"] = f"逐笔获得明细没读到（{gnote}），上面只有汇总数"
    elif dnote:
        out["live_error"] = f"逐笔消耗明细没读到（{dnote}），逐包剩余算不出来"
    elif alloc and not all(alloc_ok.values()):
        bad = [a for a, ok in alloc_ok.items() if not ok]
        out["live_error"] = ("逐包剩余是按「先到期先扣」推算的，与官方余额没对上"
                             f"（{'、'.join(bad)}），所以没显示逐包剩余")
    return out


def _minimax_auth_files(profile=None):
    """MiniMax 的凭据文件候选：(auth.json, auth-state.json)。
    给了 profile 就只认它；否则扫 ~/.minimax/auth/<buildEnv>/<region>/mcode-public/。"""
    if profile:
        p = Path(profile)
        cands = [p / "auth.json"] if (p / "auth.json").is_file() else []
        if not cands:
            cands = sorted(p.glob("**/mcode-public/auth.json"))
        return [(a, a.parent / "auth-state.json") for a in cands]
    root = Path(os.environ.get("MINIMAX_HOME") or (Path.home() / ".minimax")) / "auth"
    return [(a, a.parent / "auth-state.json")
            for a in sorted(root.glob("*/*/mcode-public/auth.json"))]


def _minimax_refresh(auth_path, state_path, key, rec, gen):
    """拿 refresh_token 换新令牌，并**立刻写回**两个文件（generation 同步 +1）。
    成功返回新的 credential dict，失败抛异常。绝不重试同一个 refresh_token。"""
    body = urlencode({
        "grant_type": "refresh_token", "refresh_token": rec["refreshToken"],
        "client_id": MINIMAX_CLIENT_ID, "scope": " ".join(rec.get("scopes") or ["agent.default"]),
        "audience": rec.get("audience") or "agent-backend",
    }).encode()
    req = UrlRequest(MINIMAX_TOKEN_URL, data=body, headers={
        "Content-Type": "application/x-www-form-urlencoded", "Accept": "application/json"})
    now_ms = int(time.time() * 1000)
    try:
        with direct_open(req, timeout=20) as r:
            resp = json.loads(r.read().decode("utf-8", "replace"))
    except HTTPError as e:
        body_txt = ""
        try:
            body_txt = e.read().decode("utf-8", "replace")[:200]
        except Exception:       # noqa: BLE001
            pass
        if "invalid_grant" in body_txt:
            raise RuntimeError("登录令牌已失效，需要重新登录 MiniMax Code（refresh token 被撤销）")
        raise RuntimeError(f"刷新令牌失败：HTTP {e.code} {body_txt}".strip())
    if "access_token" not in resp:
        raise RuntimeError(f"刷新返回异常：{str(resp)[:160]}")
    rec["accessToken"] = resp["access_token"]
    rec["refreshToken"] = resp.get("refresh_token") or rec["refreshToken"]
    rec["tokenType"] = resp.get("token_type", "Bearer")
    if resp.get("scope"):
        rec["scopes"] = resp["scope"].split()
    elif not rec.get("scopes"):
        rec["scopes"] = ["agent.default"]
    rec["expiresAtMs"] = now_ms + int(resp.get("expires_in", 3600)) * 1000
    rec["generation"] = gen
    # 两个文件一起写，generation 必须一致（否则官方客户端会判 credential 与 state 不匹配）
    cred = json.loads(auth_path.read_text(encoding="utf-8"))
    cred["records"][key] = rec
    tmp_auth = auth_path.with_name("auth.json.agenthub-tmp")
    tmp_auth.write_text(json.dumps(cred, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    os.replace(tmp_auth, auth_path)
    if state_path.is_file():
        try:
            st = json.loads(state_path.read_text(encoding="utf-8"))
            st["status"] = "authenticated"
            st["generation"] = gen
            st["expiresAtMs"] = rec["expiresAtMs"]
            st["scopes"] = rec["scopes"]
            tmp_state = state_path.with_name("auth-state.json.agenthub-tmp")
            tmp_state.write_text(json.dumps(st, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            os.replace(tmp_state, state_path)
        except Exception as e:       # noqa: BLE001
            raise RuntimeError("新令牌已保存，但登录状态同步失败，请重新登录 MiniMax Code") from e
    return rec



@contextlib.contextmanager
def _minimax_refresh_lock(auth_path):
    """Serialize AgentHub processes while refreshing the rotating token.

    The lock is released by the OS even if the process terminates. The official
    client does not use this lock, so avoid refreshing concurrently with it.
    """
    lock_path = auth_path.with_name("agenthub-refresh.lock")
    with open(lock_path, "a+b") as lock:
        lock.seek(0)
        if os.name == "nt":
            import msvcrt
            deadline = time.monotonic() + 30
            while True:
                try:
                    msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise RuntimeError("等待 MiniMax 令牌刷新锁超时")
                    time.sleep(0.1)
            try:
                yield
            finally:
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl
            fcntl.flock(lock, fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(lock, fcntl.LOCK_UN)


def _minimax_access(profile=None, live=True):
    """Read and, when required, refresh once under a cross-process lock."""
    files = _minimax_auth_files(profile)
    if not files:
        raise RuntimeError("没找到 MiniMax Code 登录凭据，请先登录")
    auth_path, state_path = files[0]
    with _minimax_refresh_lock(auth_path):
        cred = json.loads(auth_path.read_text(encoding="utf-8"))
        key = next(iter(cred.get("records") or {}), None)
        if not key:
            raise RuntimeError("MiniMax 登录档案没有凭据记录，请重新登录")
        rec = cred["records"][key]
        if state_path.is_file():
            state = json.loads(state_path.read_text(encoding="utf-8"))
            if (state.get("generation") is not None and rec.get("generation") is not None
                    and state["generation"] != rec["generation"]):
                raise RuntimeError("MiniMax 凭据与登录状态版本不一致，请重新登录")
        refreshed = False
        if int(rec.get("expiresAtMs") or 0) - int(time.time() * 1000) < 60_000:
            if not live:
                raise RuntimeError("登录令牌已过期，且 --local 模式下不刷新")
            # Once a request starts its outcome may be unknown (e.g. a timeout).
            # Never replay that rotating refresh token; a new login changes it.
            fingerprint = hashlib.sha256((rec.get("refreshToken") or "").encode()).hexdigest()
            marker = auth_path.with_name("agenthub-refresh-attempt.json")
            try:
                previous = json.loads(marker.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                previous = {}
            if previous.get("fingerprint") == fingerprint:
                raise RuntimeError("上次续期结果不确定或被拒，避免重放令牌；请重新登录 MiniMax Code")
            tmp = marker.with_suffix(".tmp")
            tmp.write_text(json.dumps({"fingerprint": fingerprint}), encoding="utf-8")
            os.replace(tmp, marker)
            gen = max(int(rec.get("generation") or 0), 0) + 1
            rec = _minimax_refresh(auth_path, state_path, key, rec, gen)
            refreshed = True
        token = rec.get("accessToken") or ""
        if not token:
            raise RuntimeError("MiniMax 凭据里没有 access token")
        return token, str(auth_path), refreshed


def minimax_credit(profile=None, live=True):
    """读 MiniMax Code 的 Token 计划积分。返回与 qoder_credit 同形的结构。

    服务端数据（实测 2026-10-09）：
      total_credits / used_credits / remaining_credits
      balance_breakdown.{total_balance, buckets[]}
      api_key（sk-cp-… 计划专用密钥）
    只读；唯一的写操作是：access token 过期时刷新后把新令牌写回官方 auth 文件
    （不写回的话旧 refresh token 已作废，官方客户端下次启动会掉登录）。
    """
    files = _minimax_auth_files(profile)
    if not files:
        return {"error": "没找到 MiniMax Code 的登录凭据（~/.minimax/auth/**/mcode-public/auth.json）"}
    try:
        token, auth_from, refreshed = _minimax_access(profile, live=live)
    except Exception as e:      # noqa: BLE001
        return {"error": str(e), "auth_kind": "mcode-oauth"}
    headers = {
        "Accept": "application/json, text/plain, */*", "Content-Type": "application/json",
        "Authorization": f"Bearer {token}",
        "Origin": MINIMAX_WEB_ORIGIN, "Referer": MINIMAX_WEB_ORIGIN + "/",
        "User-Agent": "MiniMaxAgent Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
                      "AppleWebKit/537.36 (KHTML, like Gecko) MiniMax/3.1.1 Safari/537.36",
    }
    gid = os.environ.get("MINIMAX_GROUP_ID")
    if gid:
        headers["X-Group-Id"] = gid
    # 积分明细：客户端「积分明细」弹层的数据源。之前用的 token_plan_credit 是
    # 「Token 订阅计划」那套，本账号在那儿是 0/0，签到积分全在这个 minimax-cloud 接口里。
    # 请求头里那一大堆查询参数是客户端自己带的埋点，实测缺了照样 200，
    # 只有 Origin/Referer 与 Bearer 是真必需的。
    uid = _minimax_user_id()
    packs, page, seen, total_count = [], 1, 0, 0
    while page <= 30:
        qs = urlencode({
            "page_number": page, "page_size": 20, "include_expired": "true",
            "user_id": uid or "0", "client": "desktop"})
        req = UrlRequest(f"{MINIMAX_CREDIT_URL}?{qs}", headers=headers)
        try:
            with direct_open(req, timeout=12) as r:
                d = json.loads(r.read().decode("utf-8", "replace"))
        except HTTPError as e:
            body = ""
            try:
                body = e.read().decode("utf-8", "replace")[:160]
            except Exception:       # noqa: BLE001
                pass
            if e.code in (401, 403):
                return {"error": f"HTTP {e.code}：access token 已失效，请在 MiniMax Code 里重新登录",
                        "auth_from": auth_from, "auth_kind": "mcode-oauth"}
            return {"error": f"HTTP {e.code} {body}".strip(),
                    "auth_from": auth_from, "auth_kind": "mcode-oauth"}
        except Exception as e:      # noqa: BLE001
            return {"error": f"{type(e).__name__}: {e}",
                    "auth_from": auth_from, "auth_kind": "mcode-oauth"}
        if (d.get("base_resp") or {}).get("status_code") not in (0, None):
            return {"error": f"服务端返回异常：{str(d)[:160]}",
                    "auth_from": auth_from, "auth_kind": "mcode-oauth"}
        det = d.get("details") or []
        total_count = int(d.get("total_count") or 0)
        for t in det:
            if not isinstance(t, dict):
                continue
            g, rm = _f(t.get("granted_amount")), _f(t.get("remaining_amount"))
            packs.append({
                # 类别是厂商自己的枚举：1=购买、2=签到。界面按它分档，不再叫"主额度"。
                "name": _MINIMAX_CREDIT_TYPE.get(int(_f(t.get("credit_type"))), "积分"),
                "total": g, "used": _f(t.get("consumed_amount")), "remaining": max(rm, 0.0),
                "unit": "credits", "expiresAt": int(_f(t.get("expire_at_ms"))),
                "grantedAtMs": int(_f(t.get("granted_at_ms"))),
            })
        seen += len(det)
        if not det or seen >= total_count:
            break
        page += 1
    # 按厂商的类别名分档，档名直接用它自己的词（签到 / 购买），
    # 每一笔带自己的到期时间——这正是"今天签到的 400 什么时候作废"唯一能回答的地方。
    now_ms = int(time.time() * 1000)
    merged, order = {}, []
    for p in packs:
        g = merged.setdefault(p["name"], {"name": p["name"], "unit": "credits", "total": 0.0,
                                          "used": 0.0, "remaining": 0.0, "items": [], "ms": []})
        order.append(p["name"])
        g["total"] += p["total"]
        g["used"] += p["used"]
        g["remaining"] += p["remaining"]
        g["items"].append({"label": p["name"], "total": p["total"], "used": p["used"],
                           "left": p["remaining"], "unit": "credits",
                           "expires_on": dt_text(p["expiresAt"]),
                           "detail": (dt_text(p["grantedAtMs"])[:10] + " 到账")
                                     if p["grantedAtMs"] else ""})
        if p["expiresAt"] > now_ms and p["remaining"] > 0:
            g["ms"].append(p["expiresAt"])
    groups = []
    for nm in dict.fromkeys(order):
        g = merged[nm]
        g["expiresAt"] = min(g.pop("ms"), default=0)
        groups.append(g)
    remain = sum(g["remaining"] for g in groups)
    gtot = sum(g["total"] for g in groups)
    used = sum(g["used"] for g in groups)
    exps = [g["expiresAt"] for g in groups if g["expiresAt"]]
    shaped = {
        "userId": uid, "userType": "MiniMax 积分" if packs else "免费",
        "expiresAt": min(exps, default=0),
        "isQuotaExceeded": bool(packs) and remain <= 0,
        "totalUsagePercentage": (used / gtot) if gtot else 0,
        "vendorGroups": groups,
        "raw": {"total_count": total_count},
    }
    who = {"name": "MiniMax Code", "accountId": uid}
    out = {"data": shaped, "who": who, "source": "server",
           "auth_from": auth_from, "auth_kind": "mcode-oauth"}
    if refreshed:
        out["note"] = "已自动刷新登录令牌并写回"
    return out


# ---------------------------------------------------------------------------
# TRAE CN（原 TraeWork，2026-10-11 升级改名为 TRAE CN）：登录态加密在
#   %APPDATA%\Trae CN\User\globalStorage\storage.json 的（旧安装是 %APPDATA%\TRAE SOLO CN）
#   iCubeAuthInfo://icube.cloudide —— 不是 DPAPI，是 Trae 自己的 byteCrypto.js：
#   自定义 AES-128-CBC + SHA-512 密钥派生，密钥（qoe/zoe 两段 64B）硬编码在 main.js 里，
#   静态可解、不依赖机器密钥。解密出 {token, refreshToken, expiredAt, userId, host, account}，
#   再用 token 调 POST {host}/trae/api/v2/pay/ide_user_ent_usage（服务端实时）。
# 响应 usage_summary.{consumed_amount,total_amount} 是唯一权威余额，
# user_entitlement_pack_list 逐笔套餐（免费额度 + 每月登录赠送 + 每日签到奖励）。
# 只读，不写任何产品文件。
# ---------------------------------------------------------------------------
TRAE_STORES = {"trae-cn"}

# byteCrypto.js 硬编码密钥（AES 分支）：qoe / zoe 各 64 字节，IUe 里做逐字节 XOR
TRAE_QOE = bytes([82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251,
                  124, 227, 57, 130, 155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203,
                  84, 123, 148, 50, 166, 194, 35, 61, 238, 76, 149, 11, 66, 250, 195, 78,
                  8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109, 139, 209, 37])
TRAE_ZOE = bytes([31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95,
                  96, 81, 127, 169, 25, 181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239,
                  160, 224, 59, 77, 174, 42, 245, 176, 200, 235, 187, 60, 131, 83, 153, 97,
                  23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33, 12, 125])
TRAE_A = bytes(TRAE_QOE[i] ^ TRAE_ZOE[i] for i in range(64))
TRAE_MAGIC = bytes([116, 99, 5, 16, 0, 0])  # 'tc\x05\x10\x00\x00'


def _trae_decrypt_blob(blob):
    """byteCrypto.js 的 RUe 解密。返回去掉 64B SHA-512 校验头与 PKCS7 padding 后的明文。"""
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
    if blob[:6] != TRAE_MAGIC:
        raise ValueError("登录态魔数不对，格式可能变了")
    r = blob[6:38]          # 32B 随机 key
    ct = blob[38:]          # AES-128-CBC 密文
    o = hashlib.sha512(r).digest()
    c = hashlib.sha512(o + TRAE_A).digest()
    n = c + TRAE_A
    dec = Cipher(algorithms.AES(n[:16]), modes.CBC(n[16:32])).decryptor()
    pt = dec.update(ct) + dec.finalize()
    pad = pt[-1]
    if 1 <= pad <= 16:
        pt = pt[:-pad]
    return pt[64:]


def _trae_read_login(d):
    """从 Trae 档案目录取出登录态。返回 (info, err)。"""
    storage = d / "User" / "globalStorage" / "storage.json"
    if not storage.is_file():
        return None, f"{d} 里没有 User/globalStorage/storage.json"
    try:
        obj = json.loads(storage.read_text(encoding="utf-8"))
    except Exception as e:      # noqa: BLE001
        return None, f"storage.json 读不了：{e}"
    key = next((k for k in obj if k.startswith("iCubeAuthInfo://icube.cloudide")), None)
    if not key:
        return None, f"{storage} 里没有 iCubeAuthInfo://icube.cloudide 登录态（可能没登录过）"
    try:
        blob = base64.b64decode(obj[key])
        info = json.loads(_trae_decrypt_blob(blob))
    except Exception as e:      # noqa: BLE001
        return None, f"登录态解不开：{e}"
    if not info.get("token"):
        return None, f"{storage} 的登录态里没有 token"
    acct = info.get("account") or {}
    who = dict(acct)
    who.setdefault("name", acct.get("username") or acct.get("name") or "")
    who.setdefault("email", acct.get("email") or "")
    who.setdefault("userId", info.get("userId") or "")
    return {
        "token": info["token"], "who": who,
        "expires_at": info.get("expiredAt") or "",
        "host": info.get("host") or "https://api.trae.cn",
        "user_id": info.get("userId") or "",
        "from": str(storage),
    }, None


def _trae_fetch_usage(host, token, user_id):
    """调 Trae 服务端额度接口。返回 (data, err)。"""
    url = host.rstrip("/") + "/trae/api/v2/pay/ide_user_ent_usage"
    body = json.dumps({
        "require_usage": True, "req_source": 1,
        "trae_client": "IDE", "device_id": user_id,
    }).encode()
    req = UrlRequest(url, data=body, headers={
        "Content-Type": "application/json",
        "Authorization": f"Cloud-IDE-JWT {token}",
        "x-device-id": user_id,
        "User-Agent": "agenthub",
    })
    try:
        with direct_open(req, timeout=15) as r:
            if r.status != 200:
                return None, f"HTTP {r.status}"
            d = json.loads(r.read().decode("utf-8", "replace"))
    except HTTPError as e:
        body_txt = ""
        try:
            body_txt = e.read().decode("utf-8", "replace")[:160]
        except Exception:
            pass
        return None, f"HTTP {e.code} {body_txt}".strip()
    except Exception as e:
        return None, f"{type(e).__name__}: {e}"
    if not isinstance(d, dict) or "usage_summary" not in d:
        return None, "服务端返回的结构不认识"
    return d, None


def _trae_pack_expiry(d):
    """取所有套餐里最晚的 expire_time（秒），转毫秒给 exp_text。"""
    mx = 0
    for p in d.get("user_entitlement_pack_list") or []:
        e = p.get("expire_time") or 0
        if e > mx:
            mx = e
    return mx * 1000 if mx else 0


def _trae_ent_date(ent_id):
    """从 entitlement_id 里抠出到账日：checkin_20260911_<uid> / monthly_bonus_202610_<uid>。
    认不出返回空串——这个只用来给逐笔起一个认得出的名字，不该拿它做判据。"""
    m = re.search(r"(20\d{2})(\d{2})(\d{2})", str(ent_id or ""))
    return f"{m.group(1)}-{m.group(2)}-{m.group(3)}" if m else ""


def _trae_shape(d, who, expires_at, host, auth_from):
    """把 Trae 额度响应整形成 Qoder 用量形状。

    usage_summary.{consumed_amount,total_amount} 是唯一权威的「总已用/总额度」，
    而 user_entitlement_pack_list 逐笔的 credits_limit 求和实测正好等于 total_amount
    （2026-10-11 本机：28 笔合计 3800 = total_amount 3800），所以逐笔就是总额的拆分——
    按厂商自己的组名摊成 vendorGroups，既不再另列一个"主额度"（那会把 3800 算两遍），
    也把「每月登录积分 / 每日签到」这两档和每一笔各自的到期日如实端出来。
    """
    us = d.get("usage_summary") or {}
    consumed = float(us.get("consumed_amount") or 0)
    total = float(us.get("total_amount") or 0)
    now_ms = int(time.time() * 1000)
    merged, order = {}, []
    for p in d.get("user_entitlement_pack_list") or []:
        ebi = p.get("entitlement_base_info") or {}
        quota = ebi.get("quota") or {}
        limit = float(quota.get("credits_limit") or 0)
        if limit <= 0:
            continue        # 「免费」那笔只有功能开关、没有额度，摊进来就是一行 0
        used = float((p.get("usage") or {}).get("credits_amount") or 0)
        gname = p.get("group_name") or p.get("display_desc") or "其他积分"
        g = merged.setdefault(gname, {"name": gname, "unit": "credits", "total": 0.0,
                                      "used": 0.0, "remaining": 0.0, "items": [], "ms": []})
        order.append(gname)
        g["total"] += limit
        g["used"] += used
        g["remaining"] += max(limit - used, 0.0)
        exp_ms = int(p.get("expire_time") or 0) * 1000
        ent = ebi.get("entitlement_id") or ""
        g["items"].append({
            "label": p.get("display_desc") or gname,
            "total": limit, "used": used, "left": max(limit - used, 0.0),
            "expires_on": dt_text(exp_ms), "unit": "credits",
            "detail": (_trae_ent_date(ent) + " 到账") if _trae_ent_date(ent) else "",
        })
        if exp_ms > now_ms and max(limit - used, 0.0) > 0:
            g["ms"].append(exp_ms)
    groups = []
    for gname in dict.fromkeys(order):
        g = merged[gname]
        g["expiresAt"] = min(g.pop("ms"), default=0)
        groups.append(g)
    user_id = (who.get("userId") or who.get("id") or
               (d["user_entitlement_pack_list"][0].get("user_id") or "")
               if d.get("user_entitlement_pack_list") else "")
    return {
        "userId": str(user_id),
        "userType": "credits" if d.get("is_credits_billing") else "Free",
        "isQuotaExceeded": consumed >= total if total else False,
        "totalUsagePercentage": (consumed / total) if total else 0,
        "expiresAt": _trae_pack_expiry(d),
        "vendorGroups": groups,
        "raw": d,
    }


def trae_credit(profile=None, live=True):
    """读 TraeWork CN 的额度。走服务端实时接口（byteCrypto 解密登录态拿 token）。

    profile：该账号的档案目录（驾驶舱按账号分别读）。None 用 %APPDATA% 下的
    产品 userData（新版 Trae CN 优先、旧安装 TRAE SOLO CN 回落）。
    只读，不写任何产品文件。返回与 qoder_credit 同形。
    """
    try:
        import cryptography  # noqa: F401
    except ImportError:
        return {"error": "缺 cryptography 库（pip install cryptography）"}
    if profile:
        d = Path(profile)
    else:
        ad = Path(os.environ.get("APPDATA", ""))
        # 新目录优先：旧目录只是迁移残留，新客户端登录后它永远不再更新，
        # 读它拿到的是冻结的登录快照
        d = next((ad / n for n in ("Trae CN", "TRAE SOLO CN") if (ad / n).is_dir()),
                 ad / "Trae CN")
    login, err = _trae_read_login(d)
    if login is None:
        return {"error": f"这个档案目录里没有 Trae 登录数据（{d}）——该账号还没在这台机器上登录过"
                         + (f"：{err}" if err else "")}
    if not live:
        return {"error": "Trae 额度只能走服务端实时接口，没有本地缓存可读"}
    got, live_err = _trae_fetch_usage(login["host"], login["token"], login["user_id"])
    if got is None:
        return {
            "error": f"服务端读不到（{live_err}）",
            "auth_from": login["from"], "auth_kind": "byteCrypto",
        }
    shaped = _trae_shape(got, login["who"], login["expires_at"], login["host"], login["from"])
    return {
        "data": shaped, "who": login["who"], "source": "server",
        "auth_from": login["from"], "auth_kind": "byteCrypto",
    }


def _trae_profile(arg):
    """反代用的档案目录：显式优先，否则 %APPDATA% 下新版优先、旧安装回落。"""
    if arg:
        return Path(arg)
    ad = Path(os.environ.get("APPDATA", ""))
    return next((ad / n for n in ("Trae CN", "TRAE SOLO CN") if (ad / n).is_dir()),
                ad / "Trae CN")


def _trae_models_map(profile):
    """从 state.vscdb 里取服务端下发的模型目录。

    线上 batch_get_detail_param 对 solo_* 恒返回空（即使请求头全对），
    **真目录只在这份缓存里**，而对话端点认的就是这里的 config_name。
    缓存新鲜度 = IDE 上次运行时间，所以把 state.vscdb 的 mtime 一并带出去。
    """
    db = profile / "User" / "globalStorage" / "state.vscdb"
    if not db.is_file():
        return None, f"{db} 不存在"
    try:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        try:
            con.text_factory = bytes
            row = con.execute(
                "select key, value from ItemTable "
                "where key like '%AI.agent.model.model_list_map' "
                "order by length(value) desc limit 1").fetchone()
        finally:
            con.close()
    except Exception as e:      # noqa: BLE001
        return None, f"state.vscdb 打不开：{e}"
    if not row:
        return None, "state.vscdb 里没有 AI.agent.model.model_list_map"
    try:
        raw = json.loads(row[1].decode("utf-8"))
    except Exception as e:      # noqa: BLE001
        return None, f"模型目录解析失败：{e}"
    out = {}
    for channel, arr in raw.items():
        if not isinstance(arr, list):
            continue
        items = []
        for m in arr:
            if not isinstance(m, dict):
                continue
            cw = m.get("context_window_size") or {}
            # 这个字段的源形状不统一：solo_agent 是 [256000]，solo_work_lite 是 256000
            cwv = cw.get("max") or cw.get("default") or 0
            if isinstance(cwv, list):
                cwv = max((x for x in cwv if isinstance(x, (int, float))), default=0)
            items.append({
                "config_name": m.get("config_name") or m.get("name") or "",
                "display_name": m.get("display_name") or m.get("name") or "",
                "max_tokens": m.get("max_tokens") or 0,
                "context_window": cwv,
                "max_mode": bool(m.get("max_mode")),
                "fee_model_level": m.get("fee_model_level"),
                "multimodal": bool(m.get("multimodal")),
                "thinking": (m.get("reasoning_effort_config") or {}).get("support_thinking"),
                "selectable": bool(m.get("selectable")),
                "status": bool(m.get("status")),
            })
        out[channel] = items
    return out, None


def cmd_trae(con, args):
    """Trae 反代用的只读探针：--token 吐 access token，--models 吐模型目录。

    只读，不写任何产品文件；**永远不输出 refreshToken**（它一次性，用一次会把 IDE 踢下线）。
    """
    d = _trae_profile(args.profile)
    if args.models:
        got, err = _trae_models_map(d)
        if err:
            print(json.dumps({"ok": False, "error": err, "profile": str(d)}, ensure_ascii=False))
            return
        db = d / "User" / "globalStorage" / "state.vscdb"
        print(json.dumps({
            "ok": True, "profile": str(d), "channels": got,
            "source_mtime": int(db.stat().st_mtime * 1000),
            "counts": {k: len(v) for k, v in got.items()},
        }, ensure_ascii=False))
        return
    if args.token:
        login, err = _trae_read_login(d)
        if login is None:
            print(json.dumps({"ok": False, "error": err, "profile": str(d)}, ensure_ascii=False))
            return
        exp, hours = login.get("expires_at") or "", None
        try:
            t = dt.datetime.strptime(exp.replace("Z", "+0000"), "%Y-%m-%dT%H:%M:%S.%f%z")
            hours = round((t - dt.datetime.now(dt.timezone.utc)).total_seconds() / 3600, 1)
        except Exception:      # noqa: BLE001
            pass
        print(json.dumps({
            "ok": True, "profile": str(d), "token": login["token"],
            "expires_at": exp, "hours_left": hours,
            "user_id": login["user_id"], "host": login["host"],
            "who": login["who"], "from": login["from"],
        }, ensure_ascii=False))
        return
    print(json.dumps({"ok": False, "error": "要么 --token 要么 --models"}, ensure_ascii=False))


# ---------------------------------------------------------------------------
# ZCode（智谱 GLM 编程助手，Electron）：凭据在 <数据根>/v2/credentials.json，
#   值形如 enc:v1:<b64url(iv12)>.<b64url(tag16)>.<b64url(ct)>，AES-256-GCM。
#   密钥纯本地派生：key = SHA256(env.ZCODE_CREDENTIAL_SECRET
#     ?? "zcode-credential-fallback:{platform}:{homedir}:{username}")，
#   不依赖 DPAPI、不需要抓包（逆向自 app.asar 的 createCredentialCipherProvider）。
#   解出 zcodejwttoken 后调 GET {origin}/api/v1/zcode-plan/billing/balance?app_version=<ver>。
#   ⚠ 服务端认来源头，其中 X-Device-Mid 是硬性必需：缺即 400 code:3001 parameter error，
#   实测「只带 Authorization + X-Device-Mid」就能 200；deviceMid 在
#   <数据根>/v2/telemetry-state.json。余额取
#   data.balances[].{total_units,used_units,remaining_units,expires_at}。
#   ⚠ 服务端有频率限制，短时间内连发会 429，别在循环里反复刷。
#   只读，不写任何产品文件。
# ---------------------------------------------------------------------------
ZCODE_ORIGIN = "https://zcode.z.ai"
ZCODE_APP_VERSION = "3.14.5"


def _zcode_root(profile=None):
    """ZCode 数据根：给了 profile（驾驶舱按账号目录读）就只认它，否则 ~/.zcode。

    注意 ~/.zcode 常被驾驶舱换成指向 instances/zcode/<账号> 的 junction，
    所以按账号读时 profile 就是那份实例目录，凭据在 <profile>/v2/credentials.json。"""
    if profile:
        return Path(profile)
    return Path(os.environ.get("ZCODE_HOME") or (Path.home() / ".zcode"))


def _zcode_key():
    """复刻 defaultCredentialSecret：环境变量优先，否则用「平台:主目录:用户名」拼串。"""
    secret = os.environ.get("ZCODE_CREDENTIAL_SECRET") or (
        "zcode-credential-fallback:{0}:{1}:{2}".format(
            sys.platform, Path.home(),
            os.environ.get("USERNAME") or os.environ.get("USER") or "unknown"))
    return hashlib.sha256(secret.encode("utf-8")).digest()


def _zcode_decrypt(blob, key):
    """解 enc:v1 信封（AES-256-GCM，iv 12B + tag 16B + ct）。非 enc:v1 原样返回。"""
    prefix = "enc:v1:"
    if not isinstance(blob, str) or not blob.startswith(prefix):
        return blob
    parts = blob[len(prefix):].split(".")
    if len(parts) != 3:
        raise ValueError("密文格式非法")
    iv, tag, ct = (base64.urlsafe_b64decode(p + "=" * (-len(p) % 4)) for p in parts)
    if len(iv) != 12 or len(tag) != 16:
        raise ValueError("IV/AuthTag 长度非法")
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    return AESGCM(key).decrypt(iv, ct + tag, None).decode("utf-8")


def zcode_credit(profile=None, live=True):
    """读 ZCode（智谱）编程套餐余额。返回与 qoder_credit 同形的结构。

    服务端数据（实测 2026-10-11）：data.balances[] 每笔一个额度桶，show_name 是模型名
    （如 GLM-5.3-Flash），total/used/remaining_units 是 token 数，expires_at 是秒级时间戳；
    data.plans[] 是套餐（status=active 才算数）。无套餐时 balances 为空数组。
    """
    root = _zcode_root(profile)
    cred_file = root / "v2" / "credentials.json"
    if not cred_file.is_file():
        return {"error": f"没找到 ZCode 登录数据（{cred_file} 不存在）"}
    auth_from = str(cred_file)
    kind = "enc:v1(AES-256-GCM)"
    try:
        raw = json.loads(cred_file.read_text(encoding="utf-8"))
    except Exception as e:      # noqa: BLE001
        return {"error": f"{cred_file} 解不开：{e}", "auth_from": auth_from, "auth_kind": kind}
    try:
        store = {k: _zcode_decrypt(v, _zcode_key()) for k, v in raw.items()}
    except Exception as e:      # noqa: BLE001
        return {"error": f"凭据解密失败：{e}", "auth_from": auth_from, "auth_kind": kind}
    jwt = (store.get("zcodejwttoken") or "").strip()
    if not jwt:
        return {"error": "credentials.json 里没有 zcodejwttoken（可能还没登录 ZCode）",
                "auth_from": auth_from, "auth_kind": kind}
    if not live:
        return {"error": "ZCode 余额只有服务端接口，本地没有可退的缓存",
                "auth_from": auth_from, "auth_kind": kind}
    device_mid = ""
    try:
        device_mid = (json.loads((root / "v2" / "telemetry-state.json")
                                 .read_text(encoding="utf-8")).get("deviceMid") or "")
    except Exception:           # noqa: BLE001
        pass
    if not device_mid:
        return {"error": f"读不到 deviceMid（{root / 'v2' / 'telemetry-state.json'}）"
                         "——服务端靠它认客户端，缺了必报 parameter error",
                "auth_from": auth_from, "auth_kind": kind}
    url = f"{ZCODE_ORIGIN}/api/v1/zcode-plan/billing/balance?app_version={ZCODE_APP_VERSION}"
    req = UrlRequest(url, headers={
        "Accept": "application/json",
        "Authorization": f"Bearer {jwt}",
        "User-Agent": f"ZCode/{ZCODE_APP_VERSION}",
        "HTTP-Referer": ZCODE_ORIGIN,
        "X-Title": "Z Code@electron",
        "X-ZCode-App-Version": ZCODE_APP_VERSION,
        "X-Device-Mid": device_mid,
    })
    try:
        with direct_open(req, timeout=12) as r:
            d = json.loads(r.read().decode("utf-8", "replace"))
    except HTTPError as e:
        body = ""
        try:
            body = e.read().decode("utf-8", "replace")[:160]
        except Exception:       # noqa: BLE001
            pass
        return {"error": f"HTTP {e.code} {body}".strip(), "auth_from": auth_from, "auth_kind": kind}
    except Exception as e:      # noqa: BLE001
        return {"error": f"{type(e).__name__}: {e}", "auth_from": auth_from, "auth_kind": kind}
    if not isinstance(d, dict) or d.get("code") != 0:
        return {"error": f"服务端返回异常：{str(d)[:160]}", "auth_from": auth_from, "auth_kind": kind}
    data = d.get("data") or {}
    balances = [b for b in (data.get("balances") or []) if isinstance(b, dict)]
    plans = [p for p in (data.get("plans") or []) if isinstance(p, dict)]
    active = [p for p in plans if str(p.get("status") or "").lower() == "active"]
    total = sum(b.get("total_units") or 0 for b in balances)
    used = sum(b.get("used_units") or 0 for b in balances)
    remain = sum((b.get("remaining_units") if b.get("remaining_units") is not None
                  else b.get("available_units")) or 0 for b in balances)
    exps = sorted(b["expires_at"] for b in balances if b.get("expires_at"))
    packs = [{"name": b.get("show_name") or b.get("entitlement_id") or "额度",
              "total": b.get("total_units"),
              "used": b.get("used_units"),
              "remaining": (b.get("remaining_units") if b.get("remaining_units") is not None
                            else b.get("available_units")),
              "unit": b.get("unit_type") or "token",
              "expiresAt": int(b.get("expires_at") or 0) * 1000} for b in balances]
    info = {}
    try:
        info = json.loads(store.get("oauth:bigmodel:user_info") or "{}")
    except Exception:           # noqa: BLE001
        pass
    shaped = {
        "userId": str(info.get("id") or ""),
        "userType": " / ".join(p.get("name") or "" for p in active)
                    or (plans[0].get("name") if plans else "无套餐"),
        "expiresAt": (exps[0] * 1000) if exps else 0,
        "isQuotaExceeded": bool(balances) and remain <= 0,
        "totalUsagePercentage": (used / total) if total else 0,
        # 不设 userQuota：ZCode 的每个 balance 桶本身就是一笔独立额度（按模型/套餐分），
        # 全放进 dedicatedResourcePackages 即可。若再在 userQuota 里汇总一遍，
        # credit_buckets 会把同一份额度算两遍（实测 left 直接翻倍）。
        "userQuota": None,
        "dedicatedResourcePackages": packs,
        "raw": data,
    }
    who = {"name": info.get("displayName") or info.get("username") or "ZCode",
           "accountId": str(info.get("id") or "")}
    return {"data": shaped, "who": who, "source": "server",
            "auth_from": auth_from, "auth_kind": kind}


# ---------------------------------------------------------------------------
# 千问办公（QwenWorkCN，阿里）：壳是 QoderWork 的马甲（versions.json 键名还叫 qoderwork），
#   登录态 %APPDATA%\QwenWorkCN\auth-v2.dat —— Chromium os_crypt 的 v10 信封
#   （AES-256-GCM，主密钥在同目录 Local State 的 os_crypt.encrypted_key），明文是
#   {schemaVersion, token(HS256 JWT), refreshToken, expiresAt, user{id,name,planName}}。
#   ⚠ 同目录 auth.dat 是 v1 旧壳，实测里面 token 是 "placeholder-…" 占位串，
#     必须优先 auth-v2.dat 且只认 eyJ 开头的真 JWT——拿占位符去撞接口必然 401。
#   额度（实测 2026-10-11，网关只认 Authorization: Bearer）：
#     GET /api/v1/adapter/user/account-context?include=user,plan,quota,…  → quota.remaining
#     GET /api/v1/adapter/user/wallets?page_size=100                     → 逐笔 valid_to
#   daily/monthly/longterm 三档互不相交，实测求和正好等于 quota.remaining，
#   所以三档各成一档输出、不再另列汇总数（否则 credit_buckets 会把同一笔钱算两遍）。
#   ⚠ 只拿 access token 发 GET；绝不用 refreshToken（ory_rt_…）换票，换一次旧的即作废。
# ---------------------------------------------------------------------------
QWENWORK_API = "https://gateway.qwenwork.cn"
QWENWORK_CONTEXT = ("/api/v1/adapter/user/account-context"
                    "?include=user,plan,quota,page,data_sharing")
QWENWORK_WALLETS = "/api/v1/adapter/user/wallets?page_size=100"


def _qwenwork_dirs(profile=None):
    if profile:
        return [Path(profile)]
    root = Path(os.environ.get("APPDATA", ""))
    out = [Path(os.environ["QWENWORK_HOME"])] if os.environ.get("QWENWORK_HOME") else []
    # QwenWorkCN 是当前安装用的目录名，QwenWork 是 constants 里的 userDataDirBase；
    # 只作回落，绝不反过来盖住新壳。
    out += [root / n for n in ("QwenWorkCN", "QwenWork") if root]
    la = Path(os.environ.get("LOCALAPPDATA", ""))
    if la:
        out.append(la / "QwenWorkCN")
    return [p for p in out if str(p)]


def _qwenwork_login(d):
    """解一个档案目录里的登录态。返回 (info, err)，err 说人话。"""
    ls = d / "Local State"
    if not ls.is_file():
        return None, f"{d} 里没有 Local State（os_crypt 主密钥在这）"
    try:
        aes = _aes_key(ls)
    except Exception as e:      # noqa: BLE001
        return None, f"{ls} 的主密钥取不到：{e}"
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    last = "没有 auth-v2.dat / auth.dat"
    for name in ("auth-v2.dat", "auth.dat"):
        f = d / name
        if not f.is_file():
            last = f"{f} 不存在"
            continue
        blob = f.read_bytes()
        if blob[:3] != b"v10":
            last = f"{f} 不是 v10(os_crypt) 信封，格式可能变了"
            continue
        try:
            j = json.loads(AESGCM(aes).decrypt(blob[3:15], blob[15:], None))
        except Exception as e:  # noqa: BLE001
            last = f"{f} 解不开：{e}"
            continue
        tok = j.get("token") or ""
        if not tok.startswith("eyJ"):
            last = f"{f} 里的 token 是占位符（{tok[:14]}…），这份是迁移残留的旧壳"
            continue
        u = j.get("user") or {}
        return {"token": tok, "expires_at": j.get("expiresAt") or "",
                "user": u, "from": str(f), "kind": name + "(v10 os_crypt)"}, None
    return None, last


def _qwenwork_get(path, token, timeout=15):
    req = UrlRequest(QWENWORK_API + path, headers={
        "Accept": "application/json", "Authorization": f"Bearer {token}",
        "User-Agent": "agenthub"})
    try:
        with direct_open(req, timeout=timeout) as r:
            body = r.read().decode("utf-8", "replace")
    except HTTPError as e:
        txt = ""
        try:
            txt = e.read().decode("utf-8", "replace")[:180]
        except Exception:       # noqa: BLE001
            pass
        if e.code in (401, 403):
            return None, f"HTTP {e.code} 登录令牌已不可用（{txt}）——请在千问办公里重新登录一次"
        return None, f"HTTP {e.code} {txt}".strip()
    except Exception as e:      # noqa: BLE001
        return None, f"{type(e).__name__}: {e}"
    try:
        d = json.loads(body)
    except Exception as e:      # noqa: BLE001
        return None, f"返回的不是 JSON：{body[:120]}（{e}）"
    if isinstance(d, dict) and d.get("code") not in (None, "ok", 0):
        return None, f"服务端返回异常：{str(d)[:160]}"
    return (d.get("data") if isinstance(d, dict) and "data" in d else d), None


def qwenwork_credit(profile=None, live=True):
    """读千问办公（QwenWorkCN）积分。返回与 qoder_credit 同形。只读，不写任何产品文件。"""
    login, tried, last = None, [], "没找到任何档案目录"
    for d in _qwenwork_dirs(profile):
        info, err = _qwenwork_login(d)
        tried.append(f"{d}：{err or '读到登录态'}")
        if info:
            login = info
            break
        last = err or "读不到登录态"
    if login is None:
        return {"error": f"没找到千问办公的登录数据（{last}）——这台机器上可能还没登录过",
                "tried": tried}
    auth_from, kind = login["from"], login["kind"]
    if not live:
        return {"error": "千问办公余额只有服务端接口，本地没有可退的额度缓存"
                         "（auth-v2.dat 里只有登录态）", "auth_from": auth_from, "auth_kind": kind}
    exp_ms = _iso_ms(login["expires_at"])
    if exp_ms and exp_ms < time.time() * 1000:
        return {"error": "登录令牌已于 "
                + dt.datetime.fromtimestamp(exp_ms / 1000).strftime("%Y-%m-%d %H:%M")
                + " 过期，请在千问办公里重新登录一次",
                "auth_from": auth_from, "auth_kind": kind}
    tok = login["token"]
    ctx, e1 = _qwenwork_get(QWENWORK_CONTEXT, tok)
    wal, e2 = _qwenwork_get(QWENWORK_WALLETS, tok)
    if ctx is None and wal is None:
        return {"error": f"千问办公服务端读不到：{e1 or e2}",
                "auth_from": auth_from, "auth_kind": kind}
    ctx, wal = ctx or {}, wal or {}
    user, plan, quota = ctx.get("user") or {}, ctx.get("plan") or {}, ctx.get("quota") or {}
    ws = [w for w in ((wal.get("active_wallets") or {}).get("wallets") or []) if isinstance(w, dict)]

    def tier(k):
        return _f((wal.get(k) or {}).get("total_balance"))

    daily, monthly, longterm = tier("daily_credits"), tier("monthly_credits"), tier("longterm_credits")
    now_ms = int(time.time() * 1000)
    dated = sorted(w["ms"] for w in
                   [dict(x, ms=_iso_ms(x.get("valid_to"))) for x in ws]
                   if w.get("ms") and w["ms"] > now_ms and _f(w.get("balance")) > 0)

    def pack(name, amount, scope):
        """一档钱包。到期时间优先取"金额正好等于这一档"的那笔钱包，取不到就按
        最早到期算（宁早不晚，临期提醒才不会漏）。"""
        hit = next((w for w in ws
                    if abs(_f(w.get("balance")) - amount) < 1e-6 and _iso_ms(w.get("valid_to"))), None)
        return {"name": name, "total": amount, "used": 0, "remaining": amount, "unit": "积分",
                "scope": scope,
                "expiresAt": _iso_ms(hit.get("valid_to")) if hit else (dated[0] if dated else 0)}

    groups = [pack(n, v, s) for n, v, s in (
        ("每日赠送积分", daily, "每天登录赠送，通常次日作废"),
        ("套餐积分", monthly, "按套餐月度发放"),
        ("长期积分", longterm, "长期有效钱包"))]
    groups = [g for g in groups if g["total"] > 0]
    left = _f(quota.get("remaining")) or (daily + monthly + longterm)
    plan_name = plan.get("name") or user.get("planName") or ""
    shaped = {
        "userId": str(user.get("id") or ""),
        "userType": plan.get("user_type") or "personal",
        "subscriptionPackageName": plan_name,
        # 顶层到期 = 名下最早作废的那一笔（只看还没用完的），临期提醒就认这个
        "expiresAt": dated[0] if dated else 0,
        "isQuotaExceeded": bool(quota.get("exceeded")) if quota.get("exceeded") is not None
                           else left <= 0,
        # 千问办公不返回累计已用（quota.total/used 实测为 null），所以不编数、也不给分母
        "totalUsagePercentage": 0,
        "vendorGroups": groups or [{"name": "积分", "total": left, "used": None,
                                    "remaining": left, "unit": "积分",
                                    "expiresAt": dated[0] if dated else 0, "items": []}],
        "raw": {"quota": quota, "wallets": wal},
    }
    who = {"name": user.get("name") or user.get("username") or "千问办公",
           "email": user.get("email") or "", "userId": str(user.get("id") or ""),
           "plan": plan_name}
    out = {"data": shaped, "who": who, "source": "server",
           "auth_from": auth_from, "auth_kind": kind}
    if ctx is None or wal is None:
        out["live_error"] = "；".join(x for x in (e1, e2) if x)
        out["source"] = "partial"
    return out


# ---------------------------------------------------------------------------
# 妙手（美团 CatPaw，桌面端 product.json 里 dataFolderName = .meituan-catpaw）
#   账号：~/.meituan-catpaw/auth.json 是**明文**，{auth:{accessToken,tokenType}, account:{uid,name}}
#         （%APPDATA%\catpaw-moon\catx-credential.json 里那份 ssoTokenEnc 只是同一枚令牌的
#          AES-256-GCM 信封，密钥 = sha256(机器码 MachineGuid + ":catpaw-desk-token-v2")，
#          解开还是这个 accessToken，没有第二枚凭证可用）
#   额度：GET  https://catx.nocode.cn/api/gateway/credit/balance   头 X-Auth-Token
#               → availableCredits + userPlan{planId,planName,nextRefreshTime}
#   身份：GET  .../api/gateway/passport/current-user → userId/userName/mobile
#   模型：POST https://catpaw-agent.meituan.com/api/agent/maas/model-types
#               {tenant:"CatDesk",scene:"CATX_APP",env:"EXTERNAL"}
#               → 10 个模型，各带 modelTypeId 与 extendedInfo.rateMultiplier
#
#   ⚠ 妙手不能当模型上游（2026-10-26 逐条实测，别再重试一遍）：
#     · 客户端里没有任何 /chat/completions 调用，推理在云端 agent 里，桌面只收 AG-UI 事件；
#       agent-host 的 dist 里也搜不到 chat/completions——模型不是本地调的。
#     · nocode.cn 的 /api/chat/* 是「建站」产品：建会话、发消息都回 200（甚至给出
#       conversationId/roundId），但 assistant 正文恒空、余额一分不扣（体验版实测 1170.50→1170.50），
#       不是能用的对话通道。
#     · /api/agent/conversation/create 要 source 白名单：外部账号枚举 CatPaw*/catpaw_*/CATPAW_*/
#       desk/web/api 等几十种取值，全部回「source 不存在或未启用」。
#     · 官方开放 API api.catpaw.meituan.com/v1（确实有 /v1/chat/completions 与 /v1/models）
#       只认 x-api-key；桌面 token 放 X-Auth-Token / access-token / X-Passport-Token / Bearer
#       全是 401。想用它当上游，只能去妙手 Console 申请 API Key。
#   所以这里只出账号与额度，不出上游。
# ---------------------------------------------------------------------------
CATPAW_API = "https://catx.nocode.cn"
CATPAW_AGENT = "https://catpaw-agent.meituan.com"


def _catpaw_dirs(profile=None):
    """档案目录：默认 ~/.meituan-catpaw（换号时整目录换的就是它）。"""
    if profile:
        p = Path(profile).expanduser()
        return [c for c in (p, p / ".meituan-catpaw") if (c / "auth.json").exists()] or [p]
    d = HOME / ".meituan-catpaw"
    return [d] if d.exists() else []


def _catpaw_login(d):
    p = d / "auth.json"
    try:
        j = json.loads(p.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return None, f"没找到 {p}"
    except Exception as e:      # noqa: BLE001
        return None, f"{p} 读不出来：{type(e).__name__}"
    # 令牌一过期，妙手自己会把 auth.json 写成 `null`（不是删掉）。不判这一下就是
    # AttributeError: 'NoneType' object has no attribute 'get'，整个额度页跟着炸。
    if not isinstance(j, dict):
        return None, "登录态已失效（auth.json 是空的）：打开妙手重新登录"
    tok = ((j.get("auth") or {}).get("accessToken") or "").strip()
    if not tok:
        return None, "auth.json 里没有 accessToken（这台机器上没登录过）"
    acc = j.get("account") or {}
    return {"token": tok, "userId": str(acc.get("uid") or ""),
            "name": acc.get("name") or acc.get("loginName") or "",
            "from": str(p)}, ""


def _catpaw_req(url, headers, body=None, timeout=15):
    """打妙手的网关。GET 时 body 为 None。返回 (data, err)。"""
    data = None if body is None else json.dumps(body).encode()
    req = UrlRequest(url, data=data, headers=headers,
                     method="GET" if body is None else "POST")
    try:
        with direct_open(req, timeout=timeout) as r:
            d = json.loads(r.read().decode("utf-8", "replace"))
    except HTTPError as e:
        raw = ""
        try:
            raw = e.read().decode("utf-8", "replace")[:160]
        except Exception:       # noqa: BLE001
            pass
        if e.code in (401, 403):
            return None, f"HTTP {e.code}——登录态被拒，需要在妙手里重新登录一次"
        return None, f"HTTP {e.code} {raw}".strip()
    except Exception as e:      # noqa: BLE001
        return None, f"{type(e).__name__}: {e}"
    if not isinstance(d, dict) or d.get("code") != 0:
        return None, f"服务端返回异常：{str(d)[:160]}"
    return d.get("data"), ""


def catpaw_credit(profile=None, live=True):
    """读妙手（美团 CatPaw）积分。返回与 qoder_credit 同形。只读，不写任何产品文件。"""
    login, tried, last = None, [], "没找到任何档案目录"
    for d in _catpaw_dirs(profile):
        info, err = _catpaw_login(d)
        tried.append(f"{d}：{err or '读到登录态'}")
        if info:
            login = info
            break
        last = err or "读不到登录态"
    if login is None:
        return {"error": f"没找到妙手的登录数据（{last}）——这台机器上可能还没登录过",
                "tried": tried}
    auth_from, kind = login["from"], "明文 accessToken（auth.json）"
    if not live:
        return {"error": "妙手余额只有服务端接口，本地没有额度缓存"
                         "（auth.json 里只有登录态）", "auth_from": auth_from, "auth_kind": kind}
    tok = login["token"]
    hdr = {"X-Auth-Token": tok, "Accept": "application/json", "User-Agent": "agenthub"}
    me, e0 = _catpaw_req(CATPAW_API + "/api/gateway/passport/current-user", hdr)
    bal, e1 = _catpaw_req(CATPAW_API + "/api/gateway/credit/balance", hdr)
    if bal is None:
        return {"error": f"妙手服务端读不到余额：{e1 or e0}",
                "auth_from": auth_from, "auth_kind": kind}
    bal = bal or {}
    plan = bal.get("userPlan") or {}
    av = _f(bal.get("availableCredits"))
    plan_name = plan.get("planName") or plan.get("planId") or ""
    ref_ms = int(plan.get("nextRefreshTime") or 0)
    ref_txt = dt_text(ref_ms) if ref_ms else ""
    # 模型目录只用来标"哪些是免费档"，读不到不影响余额
    models, em = _catpaw_req(
        CATPAW_AGENT + "/api/agent/maas/model-types",
        {"X-Passport-Token": tok, "Accept": "application/json",
         "Content-Type": "application/json", "client-id": "catdesk", "User-Agent": "agenthub"},
        {"tenant": "CatDesk", "scene": "CATX_APP", "env": "EXTERNAL"})
    ml, free = [], []
    for m in (models or []):
        if not isinstance(m, dict):
            continue
        ext = m.get("extendedInfo") or {}
        rate = ext.get("rateMultiplier")
        ml.append({"id": m.get("modelTypeName"), "modelTypeId": m.get("modelTypeId"),
                   "rate": rate, "copy": ext.get("marketingCopy") or ""})
        if _f(rate) == 0 and m.get("modelTypeName"):
            free.append(m["modelTypeName"])
    scope = "妙手内置模型通用（按模型倍率扣减）"
    if free:
        scope += f"；当前免费：{'、'.join(free)}"
    # ── 逐包：套餐包 / 加量包 ────────────────────────────────────────────
    # 积分中心只对外开放 5 个接口（balance / register / subscription/{plans,current} /
    # subscription/addon/list），**没有「按包拆余额」的接口**：2026-10-26 实测 40 多个
    # 候选路径（detail/packs/ledger/records/addon/mine…）全 404，客户端源码里也只有那 5 个。
    # 所以每个包的「剩余」服务端不给 —— 这里只摆每个包的**规格与状态**，
    # 数字留空而不是拿总余额冒充某一包。
    sub, _es = _catpaw_req(CATPAW_API + "/api/gateway/credit/subscription/current", hdr)
    addons, _ea = _catpaw_req(CATPAW_API + "/api/gateway/credit/subscription/addon/list", hdr)
    plans, _ep = _catpaw_req(CATPAW_API + "/api/gateway/credit/subscription/plans", hdr)
    cur = sub or {}
    cycle_credit, cycle_desc = None, ""
    cur_pid = cur.get("planId") or plan.get("planId") or ""
    for pl in (plans or []):
        if (pl.get("id") or "") != cur_pid and not pl.get("current"):
            continue
        cycle_desc = str(pl.get("subscriptionPeriodDesc") or "")
        for f in (pl.get("features") or []):
            mm = re.search(r"(\d+)\s*Credits", str(f.get("label") or ""))
            if mm:
                cycle_credit = int(mm.group(1))
                break
        break
    pack_items = [{
        "label": f"{plan_name or '套餐'}（套餐包）",
        "total": cycle_credit, "used": None, "left": None, "unit": "积分",
        "expires_on": ref_txt,
        "detail": ("每期发放 %s Credits" % cycle_credit if cycle_credit else "官方未给出每期额度")
                  + (" · " + cycle_desc if cycle_desc else "")
                  + (" · " + ref_txt + " 刷新" if ref_txt else "")
                  + " · 当前余额含历史累积，服务端不拆每包剩余",
    }]
    for ad in (addons or []):
        fee = (ad.get("priceFen") or 0) / 100.0
        pack_items.append({
            "label": ad.get("name") or "加量包",
            "total": ad.get("creditAmount"), "used": None, "left": None, "unit": "积分",
            "expires_on": "",
            "detail": "%s Credits / %g 元 / 有效期 %s 个月 · %s" % (
                ad.get("creditAmount") or "?", fee, ad.get("validMonths") or "?",
                "可购买" if ad.get("purchasable")
                else (ad.get("notPurchasableReason") or "当前不可购买")),
        })
    today_tok, _conv = None, None
    try:
        _now = int(time.time() * 1000)
        _st = _now - 86400 * 1000
        _s, _ = _catpaw_req(CATPAW_API + "/api/gateway/v1/usage/token/summary"
                            "?startTime=%d&endTime=%d" % (_st, _now), hdr)
        _c, _ = _catpaw_req(CATPAW_API + "/api/gateway/v1/usage/conversation"
                            "?startTime=%d&endTime=%d" % (_st, _now), hdr)
        today_tok = (_s or {}).get("totalTokens")
        _conv = (_c or {}).get("totalConversations")
    except Exception:      # noqa: BLE001
        _conv = None
    groups = [{
        "name": "积分包", "unit": "积分", "total": None, "used": None,
        "remaining": round(av, 8), "expiresAt": 0, "scope": scope,
        # 妙手只给"当前余额"这一个数，没有发放总额，所以 total 留空而不是编一个
        "note": f"{plan_name or '套餐'}积分按模型倍率扣减"
                + (f"；{ref_txt} 刷新" if ref_txt else "")
                + "；接口只给聚合余额，不拆每包剩余"
                + (f"；近 24 小时 {today_tok:,} token / {_conv} 个对话" if today_tok is not None else ""),
        "items": pack_items,
    }]
    shaped = {
        "userId": login["userId"] or str((me or {}).get("userId") or ""),
        "userType": "妙手账号",
        "subscriptionPackageName": plan_name,
        "expiresAt": 0,          # 积分不按日期作废，nextRefreshTime 是"刷新"不是"到期"
        "isQuotaExceeded": av <= 0,
        "totalUsagePercentage": 0,
        "vendorGroups": groups,
        "raw": {"balance": bal, "user": me, "models": ml},
    }
    who = {"name": login["name"] or (me or {}).get("userName") or "妙手",
           "userId": shaped["userId"], "mobile": (me or {}).get("mobile") or "",
           "plan": plan_name}
    out = {"data": shaped, "who": who, "source": "server",
           "auth_from": auth_from, "auth_kind": kind}
    errs = [x for x in (e0, em) if x]
    if errs:
        out["live_error"] = "；".join(errs)
        out["source"] = "partial"
    return out


# ---------------------------------------------------------------------------
# DeepSeek Harness（DSH，DeepSeek 官方编程 Agent）：凭据在 ~/.dsh\.credentials.yaml，
#   **完全明文**（无 DPAPI 无加密），取 grant `deepseek-account-platform/default` 的 token。
#   令牌放自定义头 x-dsh-auth-token（不是 Authorization，实测带 Authorization 会被拒）。
#   实测 2026-10-11 三条 GET 全 200：
#     /auth-api/v0/users/current              → id/email/mobile/id_profile.name
#     /api/v0/users/get_user_summary          → normal_wallets / bonus_wallets / total_costs
#     /api/v0/users/get_unnotified_bonuses    → 每笔赠送自己的 expires_at
#   ⚠ 它不是积分产品：余额单位是人民币元，接口只给"当前余额"，总额度得用
#     余额 + total_costs(累计花费) 反推；账号 token 本身永不到期，
#     expiresAt 只能来自未提醒的 bonus 明细。
# ---------------------------------------------------------------------------
DSH_DEFAULT_ORIGIN = "https://platform.deepseek.com"


def _dsh_dirs(profile=None):
    out = [Path(profile)] if profile else [Path.home() / ".dsh"]
    env = Path(os.environ.get("DSH_HOME") or "")
    if env.is_dir():
        out.append(env)
    return [p for p in out if str(p) and p.is_dir()]


def _dsh_grants(path):
    """极简 YAML 读：DSH 的凭据是 `records:` 下每个 grant 一段，字段再缩进一层在
    `payload:` 底下（version/token/issuer 都是）。不引 yaml 库。"""
    grants, cur, in_payload = {}, None, False
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        m = re.match(r"^([^\s:]+):\s*(.*)$", s)
        if not m:
            continue
        key, val = m.group(1).strip("'\""), m.group(2).strip().strip("'\"")
        indent = len(line) - len(line.lstrip())
        if indent == 0:
            cur, in_payload = None, False     # 顶层 version:/records:
            continue
        if indent == 2:
            cur, in_payload = key, False      # grant 名那一层
            grants.setdefault(cur, {})
            continue
        if cur is None:
            continue
        if key == "payload" and not val:
            in_payload = True
            continue
        if in_payload and val:
            grants[cur][key] = val
    return grants


def _dsh_login(profile=None):
    tried = []
    for d in _dsh_dirs(profile):
        f = d / ".credentials.yaml"
        if not f.is_file():
            tried.append(f"{f} 不存在")
            continue
        try:
            g = _dsh_grants(f)
        except Exception as e:  # noqa: BLE001
            tried.append(f"{f} 读不了：{e}")
            continue
        ap = g.get("deepseek-account-platform/default") or {}
        if not ap.get("token"):
            tried.append(f"{f} 里没有 deepseek-account-platform 的 token（没登录过 DSH 账号）")
            continue
        tried.append(f"{f}：读到登录态")
        return {"token": ap["token"],
                "origin": (ap.get("issuer") or DSH_DEFAULT_ORIGIN).rstrip("/"),
                "device_id": (g.get("deepseek-account-platform/device") or {}).get("id") or "",
                "from": str(f)}, tried
    return None, tried or ["没找到 DSH 档案目录（~/.dsh）"]


def _dsh_get(origin, path, token):
    req = UrlRequest(origin + path, headers={
        "Accept": "application/json", "x-dsh-auth-token": token, "User-Agent": "agenthub"})
    try:
        with direct_open(req, timeout=15) as r:
            d = json.loads(r.read().decode("utf-8", "replace"))
    except HTTPError as e:
        body = ""
        try:
            body = e.read().decode("utf-8", "replace")[:160]
        except Exception:       # noqa: BLE001
            pass
        if e.code == 401:
            return None, "HTTP 401——本地 token 已被平台拒绝，需要在 DSH 里重新登录一次"
        return None, f"HTTP {e.code} {body}".strip()
    except Exception as e:      # noqa: BLE001
        return None, f"{type(e).__name__}: {e}"
    if not isinstance(d, dict) or d.get("code") != 0:
        return None, f"服务端返回异常：{str(d)[:160]}"
    inner = d.get("data") or {}
    if inner.get("biz_code") not in (0, None):
        return None, f"业务码 {inner.get('biz_code')}：{inner.get('biz_msg')}"
    return inner.get("biz_data"), None


def dsh_credit(profile=None, live=True):
    """读 DeepSeek Harness 账号余额（人民币）。返回与 qoder_credit 同形。只读。"""
    login, tried = _dsh_login(profile)
    if login is None:
        return {"error": "没找到 DSH 登录数据（~/.dsh/.credentials.yaml 里没有 token）",
                "tried": tried}
    origin, token, auth_from = login["origin"], login["token"], login["from"]
    kind = ".credentials.yaml(明文 grant)"
    if not live:
        return {"error": "DSH 余额只有平台接口，本地没有可退的额度快照（~/.dsh 不存）",
                "auth_from": auth_from, "auth_kind": kind}
    summary, err = _dsh_get(origin, "/api/v0/users/get_user_summary", token)
    if summary is None:
        return {"error": err, "auth_from": auth_from, "auth_kind": kind}
    prof, _pe = _dsh_get(origin, "/auth-api/v0/users/current", token)
    bon, _be = _dsh_get(origin, "/api/v0/users/get_unnotified_bonuses", token)
    prof, bon = prof or {}, (bon if isinstance(bon, list) else [])
    cur = {"CNY": "元"}.get(prof.get("currency") or "CNY", prof.get("currency") or "元")
    normal = sum(_f(w.get("balance")) for w in (summary.get("normal_wallets") or [])
                 if isinstance(w, dict))
    bonus = sum(_f(w.get("balance")) for w in (summary.get("bonus_wallets") or [])
                if isinstance(w, dict))
    spent = sum(_f(c.get("amount")) for c in (summary.get("total_costs") or [])
                if isinstance(c, dict))
    groups = [{"name": "充值余额", "unit": cur, "total": round(normal + spent, 8),
               "used": round(spent, 8), "remaining": round(normal, 8), "expiresAt": 0,
               "scope": "DeepSeek 平台按量计费余额，不到期", "items": []}]
    if bonus > 0:
        groups.append({"name": "赠送余额", "unit": cur, "total": round(bonus, 8),
                       "used": 0, "remaining": round(bonus, 8),
                       "expiresAt": min((_iso_ms(b.get("expires_at")) for b in bon
                                         if _iso_ms(b.get("expires_at"))), default=0),
                       "scope": "运营赠送，逐笔各自有到期日", "items": [
                           {"label": b.get("campaign") or "赠送额度", "total": _f(b.get("amount")),
                            "used": 0, "left": _f(b.get("amount")), "unit": cur,
                            "expires_on": dt_text(_iso_ms(b.get("expires_at"))), "detail": ""}
                           for b in bon if _f(b.get("amount")) > 0]})
    ident = prof.get("id_profile") or {}
    shaped = {
        "userId": str(prof.get("id") or login.get("device_id") or ""),
        "userType": "DeepSeek " + ("微信" if ident.get("provider") == "WECHAT" else "账号"),
        "expiresAt": 0,
        "isQuotaExceeded": (normal + bonus) <= 0,
        "totalUsagePercentage": (spent / (normal + bonus + spent)) if (normal + bonus + spent) else 0,
        "vendorGroups": groups,
        "raw": {"summary": summary, "profile": prof},
    }
    who = {"userId": shaped["userId"], "name": ident.get("name") or "",
           "email": prof.get("email") or "", "mobile": prof.get("mobile_number") or ""}
    return {"data": shaped, "who": who, "source": "server",
            "auth_from": auth_from, "auth_kind": kind}


# ---------------------------------------------------------------------------
# Marvis（腾讯桌面 AI 助理）：Qt + CEF，不是 Electron，档案 ~/.marvis 是一个
#   **指向 %APPDATA%\Tencent\Marvis\User\<openid> 的符号链接**——链接目标里就编着账号，
#   是产品自己管理换号的机制，驾驶舱再往上挂 junction 等于抢它的链接。
#   登录态 <档案>\mobile_address.json 明文（access_token / mobile_open_id）。
#   额度**没有可直读的云端接口**：云端 /v3/marvis_get_wallet 要 AK/SK，而本机
#   data.db 的 aksks 表是 0 行（拿不到就得铸新的，那是侵入式操作，不做）。
#   可行通道只有它自己的本地网关 ws://127.0.0.1:10123/?token=<会话UUID>，
#   token 不落文件、只在 Marvis.log 里印出来；程序没在跑时退回解析日志里
#   最后一条 token.wallet 响应（日志把响应截到 ~364 字符，但 avail_tokens 在截断点之前）。
# ---------------------------------------------------------------------------
MARVIS_LOG_ROOTS = [Path(os.environ.get("LOCALAPPDATA", "")) / "Programs" / "Marvis" / "Application",
                    Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Marvis" / "Application"]


def _marvis_dirs(profile=None):
    home = Path.home()
    if profile:
        return [Path(profile)]
    out = [home / ".marvis"]
    u = home / "AppData" / "Roaming" / "Tencent" / "Marvis" / "User"
    if u.is_dir():
        out += [p for p in sorted(u.iterdir()) if p.is_dir() and p.name != "default_user"]
    return [p for p in out if p.is_dir()]


def _marvis_login(profile=None):
    tried = []
    for d in _marvis_dirs(profile):
        f = d / "mobile_address.json"
        if not f.is_file():
            tried.append(f"{f} 不存在")
            continue
        try:
            j = json.loads(f.read_text(encoding="utf-8"))
        except Exception as e:  # noqa: BLE001
            tried.append(f"{f} 解不开：{e}")
            continue
        if not j.get("access_token"):
            tried.append(f"{f} 没有 access_token（可能没登录）")
            continue
        tried.append(f"{f}：读到登录态")
        return {"openid": j.get("mobile_open_id") or "", "guid": j.get("mobile_guid") or "",
                "login_type": j.get("login_type") or "", "from": str(f)}, tried
    return None, tried or ["没找到 Marvis 档案目录（~/.marvis）"]


def _marvis_logs():
    out = []
    for r in MARVIS_LOG_ROOTS:
        if r.is_dir():
            out += sorted(r.glob("*/logs/Marvis.log"),
                          key=lambda p: p.stat().st_mtime, reverse=True)
    return out


def _marvis_wallet_from_log():
    """退路：日志里最后一条 token.wallet 响应。只读数，不碰任何文件。

    ⚠ Marvis 的日志把响应印在 JS 字符串里，引号是**双重**转义的（文件里实际是 \\"avail_tokens\\"，
    两个反斜杠），所以正则得用 \\* 而不是 \\?——用 \\? 时 16 条钱包响应一条都匹配不上（实测踩过）。
    而且整行截断到 ~364 字符、**结尾没有右花括号**：按"到行尾"取片段，再逐个键值对要求它以
    引号收尾；被截断的那种（total_tokens":"1000…）宁可丢掉，也不能把半截数字当真实总额报出去。
    """
    for lg in _marvis_logs():
        try:
            txt = lg.read_text(encoding="utf-8", errors="replace")
        except Exception:       # noqa: BLE001
            continue
        hits = re.findall(r'\\*"action\\*"\s*:\s*\\*"token\.wallet\\*"\s*,\s*'
                          r'\\*"data\\*"\s*:\s*\{(.*)', txt)
        if not hits:
            continue
        kv = dict(re.findall(r'\\*"([a-z_]+)\\*"\s*:\s*\\*"(-?[\d.]+)\\*"(?=,|\s*$)', hits[-1]))
        if "avail_tokens" in kv:
            return kv, str(lg)
    return None, None


def _marvis_gateway():
    """网关 token 是每次启动的会话 UUID，只印在日志里。返回 (token, port) 或 (None, None)。"""
    for lg in _marvis_logs():
        try:
            txt = lg.read_text(encoding="utf-8", errors="replace")
        except Exception:       # noqa: BLE001
            continue
        m = re.findall(r'token= ([0-9a-fA-F-]{36}) , port= (\d+)', txt)
        if m:
            return m[-1]
    return None, None


def _marvis_wallet_via_ws(token, port, timeout=6.0):
    """裸 socket 的最小 WebSocket 客户端（标准库没有 WS，不为此引第三方依赖）。
    只发一条 agent.action、读一条 ack，不写任何东西。"""
    import base64 as _b64
    import struct

    key = _b64.b64encode(os.urandom(16)).decode()
    req = (f"GET /?token={quote(token)} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
           f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
           f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
    # Marvis 没在跑时端口根本没人听——连接被拒是**常态**，必须变成"读不到"的说明，
    # 不能让异常穿到 cmd_quota 把整个读数打挂
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    except Exception as e:      # noqa: BLE001
        return None, f"本地网关 {port} 连不上（{type(e).__name__}: {e}）——Marvis 没在运行"
    try:
        s.sendall(req.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = s.recv(4096)
            if not chunk:
                return None, "网关握手就断了（token 已失效或 Marvis 已退出）"
            buf += chunk
        head, rest = buf.split(b"\r\n\r\n", 1)
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            return None, f"网关拒绝握手：{head[:80]!r}"

        def need(n):
            nonlocal rest
            while len(rest) < n:
                c = s.recv(65536)
                if not c:
                    raise OSError("网关关闭了连接")
                rest += c
            out, rest = rest[:n], rest[n:]
            return out

        body = json.dumps({"event": "agent.action", "requestId": "1",
                           "payload": {"action": "token.wallet"}}).encode()
        mask = os.urandom(4)
        hdr = bytearray([0x81, 0x80 | len(body)]) if len(body) < 126 else \
            bytearray([0x81, 0x80 | 126]) + struct.pack(">H", len(body))
        s.sendall(bytes(hdr) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(body)))
        deadline = time.time() + timeout
        while time.time() < deadline:
            b0, b1 = need(2)[0], need(2)[1]
            ln = b1 & 0x7F
            if ln == 126:
                ln = struct.unpack(">H", need(2))[0]
            elif ln == 127:
                ln = struct.unpack(">Q", need(8))[0]
            if b1 & 0x80:
                need(4)
            payload = need(ln)
            if not payload:
                continue
            try:
                data = json.loads(payload.decode("utf-8", "replace"))
            except Exception:   # noqa: BLE001
                continue
            if data.get("type") == "ack" and data.get("requestId") == "1":
                d = data.get("data") or {}
                return (d, None) if d else (None, "网关回包里没有 data")
            if data.get("type") == "error":
                return None, f"网关报错：{data.get('message')}"
        return None, "等网关回包超时"
    except Exception as e:      # noqa: BLE001
        return None, f"{type(e).__name__}: {e}"
    finally:
        s.close()


def marvis_credit(profile=None, live=True):
    """读 Marvis（腾讯）的 Token 钱包。只读，不写任何产品文件。

    ⚠ 它是 token 计数、没有"档"的概念，也没有到期时间（实测钱包字段只有 status）；
    而且额度只在它自己进程内的本地网关里，程序没跑就只能读日志缓存——
    那种数必须标成 local-cache + 说明，不能当实时余额用。
    """
    login, tried = _marvis_login(profile)
    if login is None:
        return {"error": "没找到 Marvis 登录态（Marvis 可能没在这台机器上登录过）", "tried": tried}
    openid, auth_from = login["openid"], login["from"]
    kind = "mobile_address.json(明文)"
    data, source, note = None, "local-cache", ""
    if live:
        tok, port = _marvis_gateway()
        if not tok:
            note = "日志里没有网关 token——Marvis 需要在运行中"
        else:
            raw, werr = _marvis_wallet_via_ws(tok, int(port or 10123))
            if raw:
                data, source, note = raw, "server", ""
            else:
                note = f"本地网关读不到（{werr}）"
    if data is None:
        kv, lg = _marvis_wallet_from_log()
        if kv:
            data, note = kv, (f"读的是 Marvis 日志里最后一次钱包响应（{lg}）"
                              f"{'；' + note if note else ''}")
        else:
            # 登录态读得到、额度读不到：仍然把账号报出来（向导要靠它认人），
            # 但额度一律留空 —— 留空会被界面画成"额度读不到"，报 0 才是撒谎。
            return {"data": {"userId": openid, "userType": "Marvis", "expiresAt": 0,
                             "isQuotaExceeded": False, "totalUsagePercentage": 0,
                             "vendorGroups": [],
                             "raw": {"why": note or "日志里也没有 token.wallet 响应"}},
                    "who": {"userId": openid, "name": ""}, "source": "local-cache",
                    "auth_from": auth_from, "auth_kind": kind,
                    "live_error": "Marvis 的额度只在它自己进程的本地网关里，"
                                  "程序没在运行就取不到；"
                                  + (note or "日志里也没有钱包响应")}
    avail = _f(data.get("avail_tokens", data.get("remaining_tokens")))
    frozen, saved = _f(data.get("frozen_tokens")), _f(data.get("saved_tokens"))
    cost = _f(data.get("total_cost"))
    total = _f(data.get("total_tokens")) or (avail + frozen + cost)
    shaped = {
        "userId": str(data.get("openid") or openid),
        "userType": "Marvis" + (f"·status{data.get('status')}" if data.get("status") else ""),
        "expiresAt": 0,
        "isQuotaExceeded": avail <= 0,
        "totalUsagePercentage": (cost / total) if total else 0,
        "vendorGroups": [{"name": "Token 钱包", "unit": "token", "total": total,
                          "used": cost, "remaining": avail, "expiresAt": 0,
                          "scope": "按 token 计数，Marvis 没有分档也没有到期"
                                    + (f"（冻结 {frozen:g}｜省下 {saved:g}）" if (frozen or saved) else ""),
                          "items": []}],
        "raw": data,
    }
    out = {"data": shaped, "who": {"userId": openid, "name": ""}, "source": source,
           "auth_from": auth_from, "auth_kind": kind}
    if note and source != "server":
        out["live_error"] = note
    return out


# ---------------------------------------------------------------------------
# Xiaomi MiMo（小米）：**只能认出"登的是谁"，读不到额度**，登记进来是为了不再对
#   用户说"没登记直读通道"那句假话。
#   登录态：%APPDATA%\Xiaomi MiMo\Partitions\xiaomi-account\Network\Cookies
#   （Electron 独立分区，SQLite，值走 Chromium os_crypt v10），里面 userId=… 就是账号。
#   额度接口确实存在（/api/user/usage、/api/user/xiaomi/subscription/self），
#   但实测 401：持久化的 passToken 静默过不了 sid=mimopc，小米会把它踢到交互式
#   登录页（要短信验证）；而 mimo-server 的业务会话 cookie 根本不落盘
#   （userData 里只有那 5 条 xiaomi.com 的 cookie，没有任何 xiaomimimo.com 条目）。
#   所以这里只出账号、不出数字——left/total 一律 None，界面据此显示"额度读不到"，
#   绝不拿 0 冒充余额。
# ---------------------------------------------------------------------------
def xiaomi_mimo_credit(profile=None, live=True):
    root = Path(profile) if profile else Path(os.environ.get("APPDATA", "")) / "Xiaomi MiMo"
    ck = root / "Partitions" / "xiaomi-account" / "Network" / "Cookies"
    if not ck.is_file():
        ck = root / "Network" / "Cookies"
    if not ck.is_file():
        return {"error": f"没找到 Xiaomi MiMo 的 cookie 库（{ck} 不存在）——该账号可能没在这台机器上登录过"}
    ls = root / "Local State"
    if not ls.is_file():
        ls = root.parent / "Local State"
    try:
        # Cookies 是产品自己正在写的 SQLite：拷一份到临时目录再查，绝不以写方式打开原文件。
        # 实测这台机器上小米把 userId/passToken 存在**明文 value 列**（encrypted_value 是空
        # blob），所以 value 优先；哪天它改成 os_crypt 了，下面的 v10 分支才用得上。
        tmp = Path(tempfile.gettempdir()) / "agenthub-mimo-cookies.sqlite"
        for suf in ("", "-journal", "-wal", "-shm"):
            src0 = Path(str(ck) + suf)
            if src0.is_file():
                shutil.copy2(src0, str(tmp) + suf)
        con = sqlite3.connect(f"file:{tmp.as_posix()}?mode=ro", uri=True)
        rows = con.execute(
            "select host_key, name, value, encrypted_value from cookies "
            "where name in ('userId','passToken','cUserId')").fetchall()
        con.close()
    except Exception as e:      # noqa: BLE001
        return {"error": f"读 MiMo cookie 库失败：{e}", "auth_from": str(ck)}
    aes = None
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM
        aes = _aes_key(ls)
    except Exception:           # noqa: BLE001
        AESGCM = None
        aes = None
    vals = {}
    for host, name, plain, blob in rows:
        v = plain if isinstance(plain, str) else ""
        raw = bytes(blob or b"")
        if not v and raw[:3] == b"v10" and aes is not None:
            try:
                v = AESGCM(aes).decrypt(raw[3:15], raw[15:], None).decode("utf-8", "replace")
            except Exception:   # noqa: BLE001
                continue
        if v:
            vals[name] = v
    uid = vals.get("userId") or ""
    if not uid:
        return {"error": "MiMo 的 cookie 里没有 userId——这台机器上没登录过小米账号",
                "auth_from": str(ck)}
    shaped = {
        "userId": uid, "userType": "小米账号",
        "expiresAt": 0, "isQuotaExceeded": False, "totalUsagePercentage": 0,
        # 没有任何额度可报：空 vendorGroups 让 cmd_quota 输出 null，
        # 界面就会老实写"还没读到额度"，而不是画一个 0
        "vendorGroups": [],
        "raw": {"hosts": sorted({h for h, *_r in rows})},
    }
    return {"data": shaped, "who": {"name": f"小米账号 {uid}", "userId": uid},
            "source": "local-cache", "auth_from": str(ck),
            "auth_kind": "Cookies(os_crypt v10)",
            "live_error": "MiMo 的用量接口只认小米交互式登录后的业务会话，"
                          "那份 cookie 不落盘（实测带已存的 passToken 仍 401），额度读不到"}


# 服务端直读的第二批店：cmd_quota 里按产品名分发（data 一律是「Qoder 用量」形状）
def _antigravity_auth(db):
    """从（拷出来的）state.vscdb 里取 antigravityAuthStatus 那段明文 JSON。"""
    con = sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True)
    try:
        row = con.execute(
            "select value from ItemTable where key='antigravityAuthStatus'"
        ).fetchone()
    finally:
        con.close()
    if not row or not row[0]:
        return None
    try:
        st = json.loads(row[0])
    except ValueError:
        return None
    return st if isinstance(st, dict) else None


def antigravity_credit(profile=None, live=True):
    root = Path(profile) if profile else Path(os.environ.get("APPDATA", "")) / "Antigravity"
    # 主档案优先；.backup 是产品自己的周期快照，主文件里没这个键时才拿它兜底
    cands = [root / "User" / "globalStorage" / "state.vscdb",
             root / "state.vscdb",
             root / "User" / "globalStorage" / "state.vscdb.backup",
             root / "state.vscdb.backup"]
    db = next((c for c in cands if c.is_file()), None)
    if db is None:
        return {"error": f"没找到 Antigravity 的登录档案（{cands[0]} 不存在）——"
                         "该账号可能没在这台机器上登录过"}
    # state.vscdb 是产品正在写的 SQLite：拷一份再查，绝不以写方式打开原文件
    tmp = Path(tempfile.gettempdir()) / "agenthub-antigravity-state.sqlite"
    try:
        for suf in ("", "-journal", "-wal", "-shm"):
            src0 = Path(str(db) + suf)
            if src0.is_file():
                shutil.copy2(src0, str(tmp) + suf)
        st = _antigravity_auth(tmp)
    except Exception as e:      # noqa: BLE001
        return {"error": f"读 Antigravity 登录档案失败：{e}", "auth_from": str(db)}
    if st is None:
        return {"error": "Antigravity 的 state.vscdb 里没有 antigravityAuthStatus——"
                         "这台机器上没登录过 Google 账号", "auth_from": str(db)}
    name = str(st.get("name") or "").strip()
    email = str(st.get("email") or "").strip()
    if not name and not email:
        return {"error": "antigravityAuthStatus 里没有账号信息", "auth_from": str(db)}
    uid = email or name
    shaped = {
        "userId": uid, "userType": "Google 账号",
        "expiresAt": 0, "isQuotaExceeded": False, "totalUsagePercentage": 0,
        # 没有任何额度可报：空 vendorGroups 让 cmd_quota 输出 null，
        # 界面就会老实写"还没读到额度"，而不是画一个 0
        "vendorGroups": [],
        "raw": {"name": name, "email": email},
    }
    who = {"name": name or email, "userId": uid}
    if name and email:
        who["email"] = email
    return {"data": shaped, "who": who,
            "source": "local-cache", "auth_from": str(db),
            "auth_kind": "state.vscdb 明文 JSON(antigravityAuthStatus)",
            "live_error": "Antigravity 按 Google 账号计费，客户端没有可查的余额接口，"
                          "本地只读得到登录的是谁"}


# ---------------------------------------------------------------------------
# Coze（扣子桌面端）：**只能认出"登的是谁"，读不到额度**。账号身份散在内嵌
#   coze-browser 分区（bytedance.larkoffice.com 登录态）的 localStorage 里：
#   view_mode 那条 JSON 的 "user" 字段就是数字 userId（实测是一串 19 位数字）。
#   没有昵称、没有邮箱、没有余额接口（额度在官网看），
#   所以账号名按 userId 显示，left/total 一律 None。
#   分区目录名带 hash（coze-browser%3Aonline%3A<hash>），所以按 glob 扫，
#   不把 hash 写死进代码。
# ---------------------------------------------------------------------------
def coze_credit(profile=None, live=True):
    root = Path(profile) if profile else Path(os.environ.get("APPDATA", "")) / "Coze"
    if not root.is_dir():
        return {"error": f"没找到 Coze 的档案目录（{root} 不存在）"}
    uid, hit = None, ""
    for ldb in sorted(root.glob("Partitions/*/Local Storage/leveldb/*.ldb")) + \
               sorted(root.glob("Partitions/*/Local Storage/leveldb/*.log")):
        try:
            data = ldb.read_bytes()
        except OSError:
            continue
        m = re.search(rb'"user"\s*:\s*"(\d{6,})"', data)
        if m:
            uid, hit = m.group(1).decode(), str(ldb)
            break
    if not uid:
        return {"error": "Coze 的本地存储里没找到 userId（这台机器上没登录过账号，"
                         "或登录态还没落到本机分区）"}
    shaped = {
        "userId": uid, "userType": "Coze 账号",
        "expiresAt": 0, "isQuotaExceeded": False, "totalUsagePercentage": 0,
        # 同 MiMo/Antigravity：没有额度就不编额度
        "vendorGroups": [],
        "raw": {"userId": uid},
    }
    return {"data": shaped, "who": {"name": f"Coze 账号 {uid}", "userId": uid},
            "source": "local-cache", "auth_from": hit,
            "auth_kind": "分区 localStorage(view_mode.user)",
            "live_error": "Coze 的余额只在官网/小程序里看，桌面端没有可调的余额接口，"
                          "本地只读得到登录的是谁"}


# ---------------------------------------------------------------------------
# 商汤小浣熊（@raccoon/office-desktop，SenseTime 办公小浣熊桌面端，2026-11-05 本机实测）
#
# 档案根：Windows 是 ~/.office-raccoon（产品里 PROFILE_DIRECTORY='office-raccoon' +
#   WINDOWS_PROFILE_DIRECTORY='.office-raccoon'，家目录那层是对的），结构：
#     agent/config/auth.json     登录态（BOX_AGENT_CONFIG_DIR 指的就是这里）
#     user-data/                 Chromium 档案（Network/Cookies、Local Storage、local-chat.sqlite3）
#     workspace/  logs/
#   换号整目录换的就是它；启动时用 `--client-profile <绝对路径>` 能挂另一份档案
#   （产品自带的多档案开关，目录里必须有 .raccoon-lab-client.json 标记）。
#
# 登录态是**明文 JSON**：access_token 是 HS256 JWT（iss=xiaohuanxiong.com/iam，
#   sub=pu-… 就是平台用户 id），另有 refresh_token / office_identity(personal|enterprise)
#   / office_org_name / office_org_role。
# 认名字靠桌面端自己那句 GET https://xiaohuanxiong.com/api/iam/v1/account（Bearer），
#   返回 data.user.name / phone（服务端已打码）/ platform_user_id；连不上就退回 JWT 的
#   sub 尾巴，绝不把账号读成空串（空账号在驾驶舱里会被当成"没存过登录态"）。
# 额度：桌面端没有积分/会员接口（积分只在官网 xiaohuanxiong.com 看），照 Coze/MiMo 的
#   规矩只出账号，left/total 一律 None，绝不拿 0 冒充余额。
# ---------------------------------------------------------------------------
RACCOON_MAIN = "https://xiaohuanxiong.com"
# 桌面端自己带的那两个头：签到（office/v3/setting_info）与桌面端登录奖励只认它们。
# 值照 Magpie 的 raccoon 插件（packages/raccoon/index.mjs）抄，不是猜的。
RACCOON_DESKTOP_HEADERS = {"X-Client-Platform": "desktop-windows", "X-Client-Version": "v1.0.0"}
# 每天送 300；当天登录桌面客户端后这一笔的有效期是 7 天（本机 2026-11-05 实测：
# 16:04 发的那笔 daily_grant，expire_at = 11-11 23:59:59，正好 7 天窗）。
RACCOON_DAILY = 300
# 余额分档 → 显示名。键取自 /points/v1/balance（本机实测那份报文）。
RACCOON_POOLS = [("daily_points", "每日积分", "daily_grant"),
                 ("reward_points", "活动奖励", "reward_grant"),
                 ("topup_points", "充值积分", "topup_grant"),
                 ("monthly_points", "月度积分", "monthly_grant")]


def _raccoon_dirs(profile=None):
    """档案根候选：给了 profile 就只认它（换号后挂的就是它）。"""
    if profile:
        return [Path(profile)]
    return [HOME / ".office-raccoon",
            Path(os.environ.get("APPDATA", "")) / "office-raccoon"]


def _jwt_payload(token):
    """只解 payload，不验签——认人不需要签名，签名是服务端的事。"""
    try:
        seg = token.split(".")[1]
        seg += "=" * (-len(seg) % 4)
        return json.loads(base64.urlsafe_b64decode(seg.encode("ascii")))
    except Exception:
        return {}


def _raccoon_auth(profile=None):
    """读登录态：返回 (access_token, auth.json 路径, 原始 JSON)。读不到抛 RuntimeError。"""
    reasons = []
    for d in _raccoon_dirs(profile):
        f = Path(d) / "agent" / "config" / "auth.json"
        if not f.is_file():
            reasons.append(f"{f} 不存在")
            continue
        try:
            raw = json.loads(f.read_text(encoding="utf-8"))
        except Exception as e:      # noqa: BLE001
            raise RuntimeError(f"小浣熊的登录文件读不了（{f}）：{e}")
        token = str(raw.get("access_token") or raw.get("token")
                    or raw.get("auth_token") or "").strip()
        if not token:
            reasons.append(f"{f} 里没有 access_token")
            continue
        return token, f, raw
    raise RuntimeError("没找到小浣熊的登录文件（找过：" + "；".join(reasons) + "）："
                       "先在客户端里登录一次再读")


def _raccoon_req(path, token, method="GET", body=None, timeout=15, extra_headers=None):
    """统一走 _ci_req（它处理代理绕过 / TLS 1.2 回退 / 4xx 正文）。成功返回 data。"""
    headers = {"Accept": "application/json", "Authorization": "Bearer " + token,
               "User-Agent": CHECKIN_UA, **RACCOON_DESKTOP_HEADERS}
    if extra_headers:
        headers.update(extra_headers)
    if body is not None:
        headers["Content-Type"] = "application/json"
    d, e = _ci_req(RACCOON_MAIN + path, headers,
                   {} if body is None and method == "POST" else body,
                   timeout=timeout, tag="raccoon " + path.split("?")[0])
    if d is None:
        return None, e
    code = d.get("code")
    if isinstance(code, (int, float)) and int(code) != 0:
        return None, f"服务端回 code={code}（{d.get('message') or '无 message'}）"
    data = d.get("data")
    return (data if isinstance(data, (dict, list)) else d), None


def _iso_ms(s):
    try:
        return int(dt.datetime.fromisoformat(str(s)).timestamp() * 1000)
    except (TypeError, ValueError):
        return 0


def _mask_phone(p):
    p = re.sub(r"\D", "", str(p or ""))
    return f"{p[:3]}****{p[-4:]}" if len(p) >= 7 else ""


def _raccoon_label(user, fallback_uid):
    name = str((user or {}).get("name") or "").strip()
    phone = _mask_phone((user or {}).get("phone"))
    if name and phone:
        return f"{name}〈{phone}〉"
    return name or f"小浣熊账号 …{fallback_uid[-6:]}"


def raccoon_identity(profile=None):
    """离线只认人：不联网，账号名退回令牌尾巴。给 --local 与"服务端读不到"兜底用。"""
    token, f, raw = _raccoon_auth(profile)
    claims = _jwt_payload(token)
    uid = str(claims.get("sub") or "").strip()
    if not uid:
        raise RuntimeError(f"小浣熊的令牌里没有用户 id（{f}）：这份登录态不完整，"
                           "在客户端里重新登录一次再读")
    return token, f, raw, claims, uid


def raccoon_credit(profile=None, live=True):
    """积分余额 + 逐笔未过期发放。接口照 Magpie 的 raccoon 插件实测：
      GET  /api/web/points/v1/balance                     available_points 与分档余额
      GET  /api/web/points/v1/bills?paging.limit=N       逐笔流水（发放为正、消耗为负，带 expire_at）
      GET  /api/web/auth/v1/user_info                    昵称 / 手机号 / 数字 id
      GET  /api/web/auth/v1/entitlement_info             office.pro_enable 与套餐类型（Free/Pro）
    bills 的 paging.offset 无效、total 恒为 0（插件 #223 实测），所以只能一次要一大把。
    """
    try:
        token, f, raw, claims, uid = raccoon_identity(profile)
    except RuntimeError as e:
        return {"error": str(e)}
    ident = str(raw.get("office_identity") or "").strip().lower()
    org = str(raw.get("office_org_name") or "").strip()
    # 别把 exp（会话令牌 30 天）当额度到期日摆出去：那是登录有效期，不是积分有效期
    base = {"userId": uid, "userType": "小浣熊账号", "expiresAt": 0,
            "isQuotaExceeded": False, "totalUsagePercentage": 0,
            "vendorGroups": [], "raw": {"userId": uid, "identity": ident, "org": org}}
    who = {"name": _raccoon_label(None, uid), "userId": uid}
    if not live:
        return {"data": base, "who": who, "source": "local-cache",
                "auth_from": str(f), "auth_kind": "明文 auth.json（access_token 为 HS256 JWT）",
                "live_error": "按 --local 读：只解了令牌认人，没去读积分余额"}

    bal, e1 = _raccoon_req("/api/web/points/v1/balance", token)
    if bal is None:
        exp = int(claims.get("exp") or 0)
        why = e1 or "未知原因"
        if exp and exp < time.time():
            why = "登录令牌已过期（打开一次客户端它会自动续期）；" + why
        return {"data": base, "who": who, "source": "local-cache",
                "auth_from": str(f), "auth_kind": "明文 auth.json（access_token 为 HS256 JWT）",
                "live_error": f"小浣熊的积分没读到：{why}"}

    user, _e2 = _raccoon_req("/api/web/auth/v1/user_info", token)
    if isinstance(user, dict):
        who = {"name": _raccoon_label(user, uid), "userId": uid}
        uid = str(user.get("id") or uid)

    ent, _e3 = _raccoon_req("/api/web/auth/v1/entitlement_info", token)
    office = ((ent or {}).get("office") if isinstance(ent, dict) else None) or {}
    plan_type = ((office.get("active_plan") or {}).get("type") or "").strip()
    pro = office.get("pro_enable") is True
    plan = ("Pro" if plan_type in ("", "none") else plan_type) if pro else "Free"
    pro_exp_ms = _iso_ms(office.get("pro_expired_time")) if pro else 0

    avail = int(bal.get("available_points") or 0)
    bills, _e4 = _raccoon_bills(token, 1000)
    now_ms = int(time.time() * 1000)
    groups = []
    if isinstance(bills, list):
        by_biz = {}
        for it in bills:
            try:
                pts = int(it.get("points") or 0)
            except (TypeError, ValueError):
                continue
            if pts <= 0:
                continue
            exp_ms = _iso_ms(it.get("expire_at"))
            if exp_ms and exp_ms <= now_ms:
                continue        # 已过期的发放不是"还剩下的额度"，不往明细里摆
            by_biz.setdefault(str(it.get("biz_type") or "other"), []).append((pts, exp_ms, it))
        for key, label, biz in RACCOON_POOLS:
            pool_left = int(bal.get(key) or 0)
            items_raw = by_biz.pop(biz, [])
            if not pool_left and not items_raw:
                continue
            total = sum(x[0] for x in items_raw) or pool_left
            # 到期时间必须**到分钟**：官网「积分明细」写的是「有效期至 11-12 24:00」，
            # 接口给的 expire_at 就是 23:59:59。只格式化到日会把这一刻抹掉，
            # 界面上就只剩一个日期，"当天几点作废"看不出来（她 11-06 问的就是这个）。
            items = [{"label": str(it.get("event_name") or label),
                      "total": pts, "used": 0, "left": pts,
                      "expires_on": (dt.datetime.fromtimestamp(exp_ms / 1000).strftime("%Y-%m-%d %H:%M")
                                     if exp_ms else EXPIRY_UNKNOWN),
                      "unit": "积分", "detail": ""}
                     for pts, exp_ms, it in items_raw]
            groups.append({"name": label, "category": label, "unit": "积分",
                           "total": max(total, pool_left), "remaining": pool_left,
                           "used": max(0, total - pool_left),
                           "expiresAt": min((x[1] for x in items_raw if x[1]), default=0),
                           **({"items": items} if items else {})})
        # 余额里有、流水里没归到任何一档的（厂商新开的分档）：照样算进去，别让合计对不上
        rest = sum(int(bal.get(k) or 0) for k, _l, _b in RACCOON_POOLS)
        leftover = int(bal.get("available_points") or 0) - rest
        if leftover > 0:
            groups.append({"name": "其他积分", "category": "其他", "unit": "积分",
                           "total": leftover, "remaining": leftover, "used": 0})
    if not groups:
        groups = [{"name": "可用积分", "category": "积分", "unit": "积分",
                   "total": avail, "remaining": avail, "used": 0}]

    soonest = min((g.get("expiresAt") or 0 for g in groups if g.get("expiresAt")), default=0)
    shaped = {"userId": uid, "userType": plan, "expiresAt": pro_exp_ms or soonest,
              "isQuotaExceeded": avail <= 0,
              "totalUsagePercentage": 0, "vendorGroups": groups,
              "raw": {"userId": uid, "identity": ident, "org": org, "plan": plan}}
    live_error = ""
    if not isinstance(bills, list):
        live_error = "积分余额读到了，逐笔发放明细没读成（到期日只能按汇总显示）"
    return {"data": shaped, "who": who, "source": "server",
            "auth_from": str(f), "auth_kind": "明文 auth.json（access_token 为 HS256 JWT）",
            "live_error": live_error}


def _raccoon_bills(token, limit=20):
    d, e = _raccoon_req(f"/api/web/points/v1/bills?paging.limit={limit}&paging.offset=0", token)
    if d is None:
        return None, e
    items = d.get("items") if isinstance(d, dict) else None
    return (items or []), None


def _raccoon_is_daily(it, today):
    try:
        pts = int(it.get("points") or 0)
    except (TypeError, ValueError):
        return False
    if pts <= 0:
        return False
    if str(it.get("created_at") or "")[:10] != today:
        return False
    return str(it.get("biz_type") or "") in ("daily_grant", "reward_grant")


# ─────────────────────────── 豆包（doubao.com）───────────────────────────
# 客户端是 CEF/Chromium 147 壳，登录态就是 doubao.com 的会话 cookie，落在
#   {LOCALAPPDATA}\Doubao\User Data\Default\Network\Cookies（SQLite + os_crypt v10 信封）
# 主密钥在同目录 Local State 的 os_crypt.encrypted_key（DPAPI 包着）；信封明文前 32 字节
# 是 SHA256(host_key)（Chromium 24+ 的 domain 绑定），解出来要剥掉才是 cookie 值。
#
# 三条接口 2026-11-10 本机实测**全部纯 cookie 鉴权、无 a_bogus / msToken 签名**：
#   POST /alice/ug/campaign/v3/detail       {"campaign_group_code": SCENE} → tasks[].daily_checkin
#   POST /alice/ug/campaign/landing/checkin {"scene": SCENE}              → 领取（服务端幂等）
#   POST /samantha/chat/completion                                        → SSE 对话（反代用）
# 公共 query 只要 aid/version_code/pkg_type 这一小撮：device_id 留空、web_id/tea_uuid 全省
# 都能过（四种组合实测状态一致），所以不必去本机翻设备号。
DOUBAO_SCENE = "pc_incentive_task_mvp_v3"
DOUBAO_AID = "497858"
DOUBAO_MAIN = "https://www.doubao.com"
DOUBAO_DAILY = 200          # 常规每天 200 额度；周一翻倍由服务端决定，这里只写常规值
DOUBAO_UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
             "(KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 Doubao/2.32.6")
DOUBAO_COMMON = {
    "version_code": "20800", "language": "zh", "device_platform": "web",
    "doubao_device_platform": "desktop", "aid": DOUBAO_AID, "real_aid": DOUBAO_AID,
    "pkg_type": "release_version", "device_id": "", "region": "CN", "sys_region": "CN",
    "samantha_web": "1", "web_platform": "desktop", "use-olympus-account": "1",
}
# 豆包开着的时候 Cookies 库是独占打开的（复制直接 PermissionError），所以读到一次就留一份
# 缓存：之后签到/反代可以拿缓存跑，不逼用户"为了签个到先关掉豆包"。
DOUBAO_CACHE = (Path(os.environ.get("USERPROFILE") or str(Path.home()))
                / ".agent-cockpit" / "doubao_auth.json")


def doubao_home(profile=None):
    """档案目录。换号时驾驶舱把某个账号的 User Data 目录（或它的上级）指过来。"""
    if profile:
        p = Path(str(profile))
        if (p / "User Data").exists():
            return p / "User Data"
        return p
    return Path(os.environ.get("LOCALAPPDATA", "")) / "Doubao" / "User Data"


def _doubao_secret(blob, aes, host):
    """解一条 cookie 的 encrypted_value：v10 = 3 字节前缀 + nonce(12) + AES-GCM 密文。"""
    if not blob:
        return ""
    if blob[:3] not in (b"v10", b"v11"):
        return blob.decode("utf-8", "replace")
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    pt = AESGCM(aes).decrypt(blob[3:15], blob[15:], None)
    if pt[:32] == hashlib.sha256(host.encode("utf-8")).digest():
        pt = pt[32:]
    return pt.decode("utf-8", "replace")


def doubao_cookie_header(profile=None):
    """返回 (cookie 串, 来源说明)。读不到就抛 RuntimeError，文案里说清是锁着还是没登录。"""
    home = doubao_home(profile)
    db = home / "Default" / "Network" / "Cookies"
    ls_path = home / "Local State"
    if not db.exists():
        raise RuntimeError(f"没找到豆包的 Cookies 库（{db}）——豆包没装，或这台机器从没登录过")
    aes = _aes_key(ls_path)
    tmp = Path(tempfile.gettempdir()) / f"doubao_ck_{os.getpid()}_{int(time.time())}.db"
    try:
        shutil.copy2(db, tmp)
    except (PermissionError, OSError):
        # 豆包正在跑：退回上次成功读到的缓存（会话 cookie 按天算，够签到用）
        try:
            ck = json.loads(DOUBAO_CACHE.read_text(encoding="utf-8"))
            if ck.get("cookie"):
                age = time.time() - float(ck.get("at") or 0)
                if age < 7 * 86400:
                    return ck["cookie"], str(DOUBAO_CACHE) + f"（豆包在运行，{int(age // 3600)} 小时前读到）"
        except Exception:       # noqa: BLE001
            pass
        raise RuntimeError("豆包正在运行，Cookies 库被独占锁住——关掉豆包读一次"
                           "（读过一次就有缓存，之后开着也能签）")
    try:
        con = sqlite3.connect(str(tmp))
        rows = con.execute("select host_key, name, encrypted_value, value from cookies").fetchall()
        con.close()
    finally:
        with contextlib.suppress(OSError):
            tmp.unlink()
    parts = []
    for host, name, enc, val in rows:
        if not str(host or "").lstrip(".").endswith("doubao.com"):
            continue
        v = str(val or "")
        if not v:
            try:
                v = _doubao_secret(bytes(enc or b""), aes, str(host or ""))
            except Exception:   # noqa: BLE001 单条解不开不影响其它条
                continue
        if v:
            parts.append(f"{name}={v}")
    if not parts:
        raise RuntimeError("豆包 Cookies 里没有 doubao.com 的会话——先在豆包里登录一次")
    cookie = "; ".join(parts)
    with contextlib.suppress(OSError):
        DOUBAO_CACHE.parent.mkdir(parents=True, exist_ok=True)
        DOUBAO_CACHE.write_text(json.dumps({"cookie": cookie, "at": time.time()},
                                           ensure_ascii=False), encoding="utf-8")
    return cookie, str(db)


def _doubao_req(path, cookie, payload=None, tag=""):
    """打一条豆包接口。payload 给了就是 POST、不给就是 GET；成功返回 data。"""
    url = DOUBAO_MAIN + path + "?" + urlencode(DOUBAO_COMMON)
    headers = {"Accept": "application/json", "User-Agent": DOUBAO_UA, "Cookie": cookie,
               "Referer": DOUBAO_MAIN + "/chat/"}
    if payload is not None:
        headers["Content-Type"] = "application/json"
    d, e = _ci_req(url, headers, payload, timeout=25,
                   tag=tag or ("doubao " + path.split("?")[0]))
    if d is None:
        return None, e
    code = d.get("code")
    if isinstance(code, (int, float)) and int(code) != 0:
        return None, f"服务端回 code={code}（{d.get('message') or d.get('msg') or '无 message'}）"
    return (d.get("data") if "data" in d else d), None


def _doubao_account(cookie):
    """昵称 / 数字 user_id / 打码手机号。passport 这条不带 code，直接 data 一层。"""
    info, e = _doubao_req("/passport/account/info/v2", cookie, None, tag="doubao account")
    if info is None:
        return None, e
    return info, None


def _doubao_label(info):
    if not isinstance(info, dict):
        return "豆包账号"
    name = str(info.get("name") or "").strip()
    mobile = str(info.get("mobile") or "").strip()
    return name or mobile or "豆包账号"


def doubao_checkin_state(cookie):
    """返回 (detail 整体, 签到任务, daily_checkin 块)。"""
    d, e = _doubao_req("/alice/ug/campaign/v3/detail", cookie,
                       {"campaign_group_code": DOUBAO_SCENE}, tag="doubao detail")
    if d is None:
        raise RuntimeError(f"读不到豆包的签到状态：{e}")
    task = None
    for t in (d.get("tasks") or []):
        if str(t.get("task_kind") or "") == "daily_signin":
            task = t
            break
    return d, task, ((task or {}).get("daily_checkin") or {})


def doubao_credit(profile=None, live=True):
    """豆包额度。**厂商没有"剩余额度"接口**——签到活动只给 progress.total_amount（累计获得），
    对话消耗扣在哪一桶、还剩多少，服务端不对外。所以这里只认账号，额度按规矩报 None，
    绝不把"累计获得 1000"当成"还剩 1000"摆出去（那是撒谎）。
    累计值放在 note 里，驾驶舱签到页会显示。"""
    try:
        cookie, src = doubao_cookie_header(profile)
    except RuntimeError as e:
        return {"error": str(e)}
    info, e1 = _doubao_account(cookie)
    if info is None:
        return {"error": f"读不到豆包账号信息：{e1}"}
    uid = str(info.get("user_id") or info.get("user_id_str") or "").strip()
    who = {"name": _doubao_label(info), "userId": uid}
    base = {"userId": uid, "userType": "豆包账号", "expiresAt": 0,
            "isQuotaExceeded": False, "totalUsagePercentage": 0, "vendorGroups": [],
            "raw": {"userId": uid, "mobile": info.get("mobile") or "",
                    "avatar": info.get("avatar_url") or ""}}
    out = {"data": base, "who": who, "source": "server", "auth_from": src,
           "auth_kind": "doubao.com 会话 cookie（Chromium os_crypt v10）"}
    if not live:
        out["source"] = "local-cache"
        out["live_error"] = "按 --local 读：只解了 cookie 认人，没去读签到额度"
        return out
    try:
        d, _task, chk = doubao_checkin_state(cookie)
    except RuntimeError as e:
        out["source"] = "local-cache"
        out["live_error"] = str(e)
        return out
    total = (d.get("progress") or {}).get("total_amount")
    out["doubao"] = {"total_granted": total,
                     "continuous_days": chk.get("continuous_days"),
                     "checked_in_today": bool(chk.get("checked_in_today")),
                     "last_checkin_date": chk.get("last_checkin_date") or ""}
    out["live_error"] = (f"签到活动累计获得 {total} 额度；厂商没有剩余额度接口，"
                         "读不到还剩多少") if total is not None else ""
    return out


def ci_doubao(claim=False, profile=None, **_kw):
    """豆包每日签到：+200 额度/天（周一翻倍），连签 30 天约 8800。

    领取 = POST /alice/ug/campaign/landing/checkin，body 只有 {"scene": SCENE}。
    纯 cookie 鉴权、没有签名，服务端**自带幂等**（重复请求回 idempotent:true，不重复发放），
    所以重试是安全的。活动限个人账号，飞书企业号会被拒（服务端原话透出来即可）。
    """
    try:
        cookie, src = doubao_cookie_header(profile)
    except RuntimeError as e:
        return {"error": str(e)}
    info, _e0 = _doubao_account(cookie)
    account = _doubao_label(info)
    try:
        d, _task, chk = doubao_checkin_state(cookie)
    except RuntimeError as e:
        return {"error": str(e)}
    signed = bool(chk.get("checked_in_today"))
    days = int(chk.get("continuous_days") or 0)
    total = (d.get("progress") or {}).get("total_amount")
    reward = int((chk.get("reward") or {}).get("reward_amount") or DOUBAO_DAILY)
    note = (f"每日 {DOUBAO_DAILY} 额度，周一翻倍；累计获得 {total if total is not None else '?'}。"
            "厂商只有累计口径，没有剩余额度接口。")
    base = {
        "supported": True, "auto": True, "account": account, "streak": days,
        "activity_on": True, "signed_today": signed, "claimable": not signed,
        "reward": reward, "unit": "额度", "expires_on": "", "note": note,
        "source": DOUBAO_MAIN + "/alice/ug/campaign/v3/detail",
        "auth_from": src,
        "words": {"done": "今天已到账", "go": "今天可领",
                  "act": "立刻签到", "wait": "还没登录"},
    }
    if not claim or signed:
        if claim and signed:
            base["already"] = True
        return base
    got, e1 = _doubao_req("/alice/ug/campaign/landing/checkin", cookie,
                          {"scene": DOUBAO_SCENE}, tag="doubao checkin")
    if got is None:
        return {**base, "error": f"签到请求没发成：{e1}"}
    idem = bool(got.get("idempotent"))
    amount = int(got.get("reward_amount") or reward)
    # 回读一次真实状态：领取接口回 200 不代表额度真到账（服务端也会 200 + code 0 却什么没发）
    try:
        d2, _t2, chk2 = doubao_checkin_state(cookie)
        total2 = (d2.get("progress") or {}).get("total_amount")
        days2 = int(chk2.get("continuous_days") or days)
        signed2 = bool(chk2.get("checked_in_today"))
    except RuntimeError:
        total2, days2, signed2 = total, days, True
    return {**base, "signed_today": signed2, "streak": days2,
            "claimed": not idem, "already": idem, "reward": amount,
            "verified": signed2,
            "note": f"每日 {DOUBAO_DAILY} 额度，周一翻倍；累计获得 {total2 if total2 is not None else '?'}。"
                    "厂商只有累计口径，没有剩余额度接口。"}


def ci_raccoon(claim=False, profile=None, **_kw):
    """商汤小浣熊每日 300 积分。

    领取就是让客户端"开一次"：桌面端启动时会打 GET /api/web/office/v3/setting_info
    （带 X-Client-Platform / X-Client-Version 两个桌面头），服务端据此发当天那笔
    daily_grant。这里照 Magpie 的 raccoon 插件直接打这条，不必把客户端窗口拉起来；
    万一服务端不发（没见过桌面端登录），就回 need_login 让界面的「登录并领取」去拉客户端。
    ⚠ 有效期：当天登录过桌面客户端的那笔是 **7 天**（本机实测 11-05 发的到 11-11），
      只在网页端签的按官方口径当日刷新——所以真机到账后以流水里的 expire_at 为准，
      这里只把读到的事实摆出来，不替它算。
    """
    try:
        token, _f, _raw = _raccoon_auth(profile)
    except RuntimeError as e:
        return {"error": str(e)}
    today = dt.date.today().isoformat()
    items, e = _raccoon_bills(token, 200)
    if items is None:
        return {"error": f"读不到小浣熊积分流水：{e}"}
    user, _eu = _raccoon_req("/api/web/auth/v1/user_info", token)
    account = _raccoon_label(user if isinstance(user, dict) else None,
                             str((_jwt_payload(token) or {}).get("sub") or ""))

    today_bills = [it for it in items if _raccoon_is_daily(it, today)]
    daily = [it for it in today_bills if str(it.get("biz_type") or "") == "daily_grant"]
    exp_txt = str((daily[0].get("expire_at") if daily else "") or "")
    # 连签：从今天往回数，哪一天有 daily_grant 就算一天（断一天就停）
    day_set = {str(it.get("created_at") or "")[:10] for it in items
               if str(it.get("biz_type") or "") == "daily_grant"}
    streak, cur = 0, dt.date.today()
    while cur.isoformat() in day_set:
        streak += 1
        cur -= dt.timedelta(days=1)
    note = (f"每日 {RACCOON_DAILY} 积分；当天登录桌面客户端的那笔有效期 7 天"
            + (f"（今天这笔到 {exp_txt[:10]}）" if exp_txt else "")
            + "，只在网页端签的按当日刷新。")
    # 受邀新用户那 1000 不是签到发的：前 7 日内活跃满 3 天才在活动页领（截图原文
    # 「7 日活跃奖励 1000 · 7 日内累计活跃 3 天即得」）。只在账号还年轻的时候提一句，
    # 老账号天天挂这条等于噪音。
    created = str((user or {}).get("created_at") or "")[:10] if isinstance(user, dict) else ""
    if created:
        try:
            age = (dt.date.today() - dt.date.fromisoformat(created)).days
        except ValueError:
            age = 999
        if 0 <= age <= 7:
            note += ("受邀新用户：前 7 日内活跃满 3 天可再领 1000 积分"
                     "（在小浣熊「受邀专属任务」里领，不是这里签出来的）。")

    if not claim:
        return {"supported": True, "auto": True, "account": account, "streak": streak,
                "activity_on": True, "signed_today": bool(daily),
                "claimable": not daily, "reward": RACCOON_DAILY, "unit": "积分",
                "expires_on": exp_txt, "note": note,
                "words": {"done": "今天已到账", "go": "今天可领", "act": "立刻签到",
                          "wait": "今天还没登录"},
                "source": RACCOON_MAIN + "/api/web/points/v1/balance"}

    before = {str(x.get("bill_id") or "") for x in today_bills}
    # 这条是 GET（插件里 apiGetJson 的默认方法）。写成 POST 会被网关回 404 page not found，
    # 而且"404 也算发过了"会把当天名额白白记成跑过——本机 2026-11-05 踩过一次。
    _ok, e1 = _raccoon_req("/api/web/office/v3/setting_info", token,
                           extra_headers={"X-Raccoon-Language": "zh"})
    if e1:
        return {"error": f"小浣熊签到请求没发成：{e1}"}
    after_items, _e2 = _raccoon_bills(token, 20)
    new_bills = [it for it in (after_items or [])
                 if _raccoon_is_daily(it, today) and str(it.get("bill_id") or "") not in before]
    got = sum(int(it.get("points") or 0) for it in new_bills)
    names = []
    for it in new_bills:
        ev = str(it.get("event_name") or "积分发放")
        if ev not in names:
            names.append(ev)
    if got <= 0 and daily:
        return {"supported": True, "auto": True, "account": account,
                "signed_today": True, "claimable": False, "reward": RACCOON_DAILY,
                "unit": "积分", "expires_on": exp_txt, "already": True,
                "message": "今天这份已经到账了，服务端没再发新的", "note": note,
                "source": RACCOON_MAIN + "/api/web/office/v3/setting_info"}
    if got > 0:
        return {"supported": True, "auto": True, "account": account, "claimed": True,
                "credit": got, "message": " + ".join(names) + f" +{got}",
                "signed_today": True, "claimable": False, "reward": RACCOON_DAILY,
                "unit": "积分",
                "expires_on": str((new_bills[0].get("expire_at") if new_bills else "") or ""),
                "note": note,
                "source": RACCOON_MAIN + "/api/web/office/v3/setting_info"}
    # 没领到：这一步真要客户端开一次（服务端按"桌面端登录"发），交给界面的「登录并领取」
    return {"supported": True, "auto": False, "account": account,
            "signed_today": False, "claimable": False, "need_login": True,
            "reward": RACCOON_DAILY, "unit": "积分",
            "words": {"done": "今天已到账", "go": "今天可领", "act": "打开客户端登录",
                      "wait": "今天还没登录"},
            "note": "服务端这次没发新的每日积分：打开小浣熊桌面客户端登一次，"
                    "到账后回这里确认（客户端自己会发当天那笔）。",
            "source": RACCOON_MAIN + "/api/web/office/v3/setting_info"}



SERVER_CREDIT_STORES = {
    "astudio": astudio_credit,
    "minimax": minimax_credit,
    "trae-cn": trae_credit,
    "workbuddy": workbuddy_full,
    "workbuddy-intl": workbuddy_intl_full,
    "zcode": zcode_credit,
    "qwenwork": qwenwork_credit,
    "catpaw": catpaw_credit,
    "dsh": dsh_credit,
    "marvis": marvis_credit,
    # 只出账号、不出额度，见上面那段注释
    "xiaomi-mimo": xiaomi_mimo_credit,
    # 只出账号、不出额度，见上面那两段注释
    "antigravity": antigravity_credit,
    "coze": coze_credit,
    # 只出账号、不出额度（桌面端没有积分接口），见上面那段注释
    "raccoon": raccoon_credit,
    # 豆包：只出账号。签到活动只给"累计获得"（progress.total_amount），没有剩余额度接口
    "doubao": doubao_credit,
}


def credit_buckets(d):
    """把积分拆成一桶一桶：主额度 / 附加包 / 每个专属资源包，各自 total-used-remaining-到期日。

    到期日逐笔取：专属资源包（dedicatedResourcePackages）各自带 expiresAt，是唯一能拿到
    "分批过期"的地方；主额度/附加包只有顶层 expiresAt 一个汇总日期——Qoder 客户端与
    服务端都只给汇总数，逐日过期的明细在官网 usage 页。

    ⚠ 档名不许由这里定：各家把积分叫得很不一样（Trae 叫「每月登录积分/每日签到」、
    AStudio 叫「通用积分/星火积分」且星火只能喂讯飞模型、MiniMax 叫「签到/购买」、
    ZCode 干脆按 token 计数）。这类产品走 vendorGroups 把**厂商本名**带上来，
    下面那三个 push() 的固定名字只是 Qoder 系那套老口径的回落。
    """
    out, top_exp = [], d.get("expiresAt")

    def push(label, q, exp=None, inherit=True, category="其他", items=None):
        if not isinstance(q, dict):
            return
        if q.get("total") is None and not items:
            return
        eff = exp if exp is not None else (top_exp if inherit else None)
        b = {"pool": label, "total": q.get("total"), "used": q.get("used"),
             "remaining": q.get("remaining"), "unit": q.get("unit") or "credits",
             "expires": exp_text(eff), "category": category}
        if q.get("note"):
            b["note"] = q["note"]
        # scope = 这一档能花在什么上（厂商原话），界面拿它做"仅星火模型"这类标注
        if q.get("scope"):
            b["scope"] = q["scope"]
        if items:
            b["items"] = items
        out.append(b)

    for g in d.get("vendorGroups") or []:
        push(g.get("name") or "积分", g, g.get("expiresAt"), inherit=False,
             category=g.get("category") or g.get("name") or "其他", items=g.get("items"))
    if d.get("vendorGroups"):
        # 已经按厂商口径摊开了，就别再把 userQuota 那个汇总数当一笔塞进来——
        # cmd_quota 是对 buckets 求和的，两份都算会让余额凭空翻倍。
        return out
    push("主额度", d.get("userQuota"), category="套餐基础积分")
    # 附加包绝不继承顶层 expiresAt：那是「计划」的到期日（Free 计划给 9999 哨兵＝无限期），
    # 和赠送积分资源包没关系。实测同一账号 addOnQuota 汇总 1300，官网 account/usage 页
    # 列的是 13 笔各 100、从 10-27 起每天到期一笔的赠送包——桌面接口只给汇总数，
    # 逐笔到期日拿不到。跟着计划说"无限期"会让人以为这 1300 永远不会作废，
    # 驾驶舱的临期提醒也就永远不会响。宁可报"没给到期日"。
    push("附加包", d.get("addOnQuota"), inherit=False, category="平台奖励积分")
    for p in d.get("dedicatedResourcePackages") or []:
        push(f"资源包 {p.get('name') or str(p.get('id'))[:8]}", p, p.get("expiresAt"),
             inherit=False, category=p.get("category") or "平台奖励积分",
             items=p.get("items"))
    return out


# WorkBuddy 官方用量弹窗就是这三档，别的家多出来的（累计已用之类）归到"其他"。
GROUP_ORDER = ["套餐基础积分", "平台奖励积分", "购买积分", "其他"]


def dt_text(ms):
    """毫秒 → 'YYYY-MM-DD 时:分'。到分钟就够人看了，秒只在两个接口对账时有用。

    与 exp_text 同一套三态：0 = 接口没给（≠ 无限期，别混），9999 哨兵 = 厂商说的无限期。
    """
    if not ms:
        return EXPIRY_UNKNOWN
    if ms >= FOREVER_MS:
        return "无限期"
    return dt.datetime.fromtimestamp(ms / 1000).strftime("%Y-%m-%d %H:%M")


def _f(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return 0.0


def credit_groups(buckets, now=None):
    """把一桶桶积分归成大组，每组带逐笔明细——照 WorkBuddy 官网那层「积分明细 + 查看明细」。

    明细缺失的产品（Qoder 的附加包只有汇总数）不空着：把这一桶自己当成一笔列进去，
    界面上仍然能点开看到"这一组里有这么一笔、什么时候到期"，而不是一个数字孤零零。
    """
    now = now if now is not None else dt.datetime.now()
    merged = {}
    for b in buckets:
        name = b.get("category") or "其他"
        g = merged.setdefault(name, {"name": name, "unit": b.get("unit") or "credits",
                                     "left": 0.0, "total": 0.0, "used": 0.0, "items": []})
        if b.get("scope") and not g.get("scope"):
            g["scope"] = b["scope"]
        t, r = _f(b.get("total")), _f(b.get("remaining"))
        u = _f(b.get("used")) or max(0.0, t - r)
        g["total"] += t
        g["left"] += r
        g["used"] += u
        items = b.get("items")
        if not items:
            items = [{"label": b.get("pool") or name, "total": t, "used": u, "left": r,
                      "expires_on": b.get("expires") or EXPIRY_UNKNOWN,
                      "unit": b.get("unit") or "credits", "detail": ""}]
        for it in items:
            it = dict(it)
            it["pool"] = b.get("pool") or name
            g["items"].append(it)

    def expired(item):
        s = str(item.get("expires_on") or "")
        for fmt in ("%Y-%m-%d %H:%M", "%Y-%m-%d"):
            try:
                return dt.datetime.strptime(s, fmt) < now
            except ValueError:
                continue
        return False        # "未给出"/"无限期" 不算过期

    def finalize(g):
        live = [i for i in g["items"] if not expired(i)]
        # 逐笔补一个 expired 标记：界面「查看明细」分「有效期内 / 已过期」两个页签，
        # 靠的就是这个字段。组层的 live_count/expired_count 与它是同一套判定，
        # 不然会出现「组里说 6 笔过期，页签里却全在有效期内」这种自相矛盾。
        for it in g["items"]:
            it["expired"] = expired(it)
        # 「什么时候到期」只该看**还没用完**的那些笔。WorkBuddy 实测踩过：平台奖励积分
        # 62 笔里最早那笔今天作废、但那笔早已用光，卡上却顶着一条红色「今天到期」，
        # 而真正要盯的 3,694 积分其实还能用 20 天——提醒就此变成误导。
        # left 没给（厂商不提供逐笔剩余）时按"还没用完"算，
        # 宁可留着提醒，也不要把没证据的笔当成已经花光。
        usable = [i for i in live if i.get("left") is None or _f(i.get("left")) > 0]
        exp_dates = sorted(str(i.get("expires_on")) for i in usable
                           if i.get("expires_on") not in (EXPIRY_UNKNOWN, "无限期", ""))
        known_left = [_f(i.get("left")) for i in live if i.get("left") is not None]
        g.update({
            "live_count": len(live),
            "expired_count": len(g["items"]) - len(live),
            "usable_count": len(usable),
            "live_left": round(sum(known_left), 4) if known_left else round(g["left"], 4),
            "expires": exp_dates[0] if exp_dates else EXPIRY_UNKNOWN,
            "left": round(g["left"], 4), "total": round(g["total"], 4),
            "used": round(g["used"], 4),
        })
        return g

    groups = [finalize(merged.pop(n)) for n in GROUP_ORDER if merged.get(n)]
    # 厂商自己命名的档（Trae 的「每日签到」、AStudio 的「星火积分」…）照它本来的顺序
    # 跟在四档老口径后面。早先这里只补一个 expires=未给出，等于把逐笔到期直接丢掉，
    # 界面上这些产品就永远只有总量、没有"哪一笔哪天作废"。
    return groups + [finalize(g) for g in merged.values()]


def exp_text(ms):
    """区分三种：有日期→日期+时刻；9999 哨兵→厂商明说的"无限期"；0/缺失→接口根本没给。

    把"没给"和"无限期"合并成一个词是这次误报的根因——无限期是个承诺，
    没给到期日只是信息缺失，不能拿来充当承诺。

    时刻要留着：厂商的到期几乎都落在 23:59:59（官网写「有效期至 11-12 24:00」），
    只格式化到日会把"当天几点作废"这一格信息抹掉。界面那一行是 `日期 时刻`，
    倒计时仍然只按日期算（quota::days_left 只看前 10 位）。"""
    if not ms:
        return EXPIRY_UNKNOWN
    if ms >= FOREVER_MS:
        return "无限期"
    return dt.datetime.fromtimestamp(ms / 1000).strftime("%Y-%m-%d %H:%M")


def usage_scope(profile, data):
    """只读"已用"的产品的账号标签：按档案目录读时要说清楚算的是哪一份。

    括号是有意的：驾驶舱只会把形如纯用户名的 account 自动填成账号名，
    目录名不该冒充账号名。"""
    if not profile:
        return "本机累计（不分账号）"
    return f"该档案累计已用（{Path(data['db']).parent.name}）"


# ---------------------------------------------------------------------------
# 每日签到 / 领积分
#
# 默认只查状态（全是只读语义的接口），要领必须显式带 --claim 指明产品，
# 免得一条误打的命令就把某个号的当日名额用掉。
# 每条返回都带 auto 字段：能不能交给程序自动跑，取决于「有没有独立状态接口」
# 和「领取要不要验证码/要不要在网页里点」——这三条我们替用户判断不了的就一律 False。
# ---------------------------------------------------------------------------

CHECKIN_UA = ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
              "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")

# MiniMax 按这个判"今天"是哪一天，status 与 claim 两条都得带
CI_TZ = "Asia/Shanghai"


CI_RAW = []   # 最近一轮的原始报文，--raw 用。MiniMax 那条"领取成功"其实是空响应被当成成功，
              # 就是因为界面上只看得到解析结果、看不到原文——解析错了永远查不出来。


def _ci_req(url, headers, body=None, timeout=15, tag="", tries=2):
    """发一条签到类请求。body 给了就是 POST、不给就是 GET——这两类接口两家都有，
    别把 GET 写成 POST（Qoder 的 status 与 campaigns 只认 GET）。
    这类网关鉴权失败常常照样回 200、只在 body 里给 code，所以只看状态码会漏。

    tries 只对 GET 生效：WorkBuddy 成长中心那几条有 12~16 秒的长尾（同一条
    `travel/config` 实测一次 3.4 秒、一次 16.4 秒直接顶穿超时），一次读不到不等于
    今天读不到。带 body 的写请求一次都不重发——重发等于替她把当日名额再花一遍。"""
    data = json.dumps(body).encode() if body is not None else None
    left = (tries if data is None else 1) - 1
    raw = ""
    while True:
        req = UrlRequest(url, data=data, headers=headers)
        try:
            with direct_open(req, timeout=timeout) as r:
                raw = r.read().decode("utf-8", "replace")
            break
        except HTTPError as e:
            try:
                full = e.read().decode("utf-8", "replace")
            except Exception:       # noqa: BLE001
                full = ""
            txt = full[:150]
            CI_RAW.append({"tag": tag, "url": url, "err": f"HTTP {e.code} {txt}".strip()})
            # 这类网关会把业务结论放在 4xx 的正文里（WorkBuddy 用 HTTP 400 回
            # {"code":10001,"msg":"今天已签到，请明天再来"}）。把整条响应当错误丢掉，
            # "已签过不算异常"那条判据就永远轮不到执行——所以正文能解析成 JSON 就照样交回去，
            # 只多挂一个 _http_status 让调用方愿意看时能看出来。
            try:
                got = json.loads(full)
            except ValueError:
                return None, f"HTTP {e.code} {txt}".strip()
            if isinstance(got, dict):
                got.setdefault("_http_status", e.code)
                return got, f"HTTP {e.code} {txt}".strip()
            return None, f"HTTP {e.code} {txt}".strip()
        except Exception as e:      # noqa: BLE001
            err = f"{type(e).__name__}: {e}"
            CI_RAW.append({"tag": tag, "url": url, "err": err})
            if left <= 0:
                return None, err
            left -= 1
            time.sleep(1.2)
    CI_RAW.append({"tag": tag, "url": url, "raw": raw[:1500]})
    try:
        return json.loads(raw), None
    except ValueError:
        return None, f"响应不是 JSON：{raw[:120]}"


def _ci_body(d):
    """各家都把业务数据塞在 data 里，Qoder 那套 sash 是平铺的——两种形状都认。"""
    if isinstance(d, dict) and isinstance(d.get("data"), dict):
        return d["data"]
    return d if isinstance(d, dict) else {}


def _ci_pick(dct, *names):
    """snake_case / camelCase 两套写法各家混用，按顺序取第一个存在的。"""
    for n in names:
        if n in dct and dct[n] is not None:
            return dct[n]
    return None


# 官方 CLI（@genie/agent-cli）跟着桌面端一起装，它认 CODEBUDDY_AUTH_TOKEN 这个环境变量，
# 把桌面端已登录的令牌喂进去就能无人值守跑一句——实测（2026-10-13）不需要再走一次浏览器登录。
# bin/codebuddy 是无扩展名的 node 脚本，Windows 不能直接 exec，必须 node 拉起。
WB_CLI_BY_INTL = {
    False: Path("{LOCALAPPDATA}/LenovoSoftstore/Install/WorkBuddy/resources/app.asar.unpacked/cli/bin/codebuddy"),
    True: Path("{LOCALAPPDATA}/Programs/WorkBuddyAI/resources/app.asar.unpacked/cli/bin/codebuddy"),
}


def _wb_cli_path(intl=False):
    return WB_CLI_BY_INTL.get(bool(intl))


def _wb_activity_dir(intl=False):
    """发对话固定用这个目录。用临时目录的话每跑一次就在 projects 下多长一个项目文件夹
    （CLI 按 cwd 分项目），既攒垃圾又让会话记录没处找。两个号分开，别混进同一个项目。"""
    d = Path.home() / ".codebuddy" / ("daily-activity-intl" if intl else "daily-activity-cn")
    try:
        d.mkdir(parents=True, exist_ok=True)
    except OSError:
        pass
    return d


# 国际版桌面端的本地会话库。`sessions` 表逐会话记着 user_id 与最后活动时间——
# 服务端只给"次日到账的 30 积分流水"，没有"今天活跃过没有"这个接口，
# 所以"她今天自己在客户端发过对话"这件事只能从这里读出来。
# 国内的 ~/.workbuddy 排第二只是兜底：uid 对不上就不会误判成国际版那个号。
WB_INTL_SESSION_DBS = (("~/.workbuddy-ai", "workbuddy.db"), ("~/.workbuddy", "workbuddy.db"))


def _wb_local_activity(uid, intl=True, day=None):
    """这一天、这个 user_id 在本地会话库里有没有真发生过对话。

    返回 {"ok", "count", "last"(HH:MM), "db", "uid", "other_today", "why"}。
    只读、不写任何文件。uid 拿不到时不拿别人的会话充数（那就是串号），
    只报 other_today 让上层说明"库里有对话但认不出是哪个号"。
    """
    if not intl:
        return {"ok": False, "count": 0, "why": "只有国际版需要这条证据"}
    # 没有 user_id 就不判"这个号今天活跃过"：整个库今天有对话不代表是它，
    # 拿别的号的活跃给它记完成，等于把两家都判错（跟共用档案那类串号一个道理）
    if not uid:
        return {"ok": False, "count": 0, "why": "登录包里没有 user_id，认不出这个号自己的会话"}
    day = day or dt.date.today()
    start = int(dt.datetime.combine(day, dt.time.min).timestamp() * 1000)
    cands = [Path(rel).expanduser() / name for rel, name in WB_INTL_SESSION_DBS]
    cands = [p for p in cands if p.is_file()]
    if not cands:
        return {"ok": False, "count": 0, "why": "没找到国际版的本地会话库"}
    why = ""
    for p in cands:
        try:
            # as_uri 拼 file: —— 路径里带空格/中文/# 时手写 "file:<path>" 会指到别处
            con = sqlite3.connect(p.absolute().as_uri() + "?mode=ro", uri=True)
            try:
                where = "coalesce(last_activity_at, updated_at) >= ?"
                args = [start]
                if uid:
                    where += " and user_id = ?"
                    args.append(str(uid))
                row = con.execute(
                    "select count(*), max(coalesce(last_activity_at, updated_at))"
                    f" from sessions where {where}", args).fetchone()
                other = 0
                if uid:
                    other = con.execute(
                        "select count(*) from sessions where coalesce(last_activity_at,"
                        " updated_at) >= ? and user_id != ?", [start, str(uid)]).fetchone()[0]
            finally:
                con.close()
        except sqlite3.Error as e:
            why = f"读不了 {p.name}：{e}"
            continue
        n, last = int(row[0] or 0), row[1]
        return {
            "ok": True, "count": n, "db": str(p), "uid": str(uid or ""),
            "last": dt.datetime.fromtimestamp(last / 1000).strftime("%H:%M") if last else "",
            "other_today": int(other or 0),
        }
    return {"ok": False, "count": 0, "why": why or "本地会话库读不到"}


def _wb_activity_note(act):
    """把本地证据说成一句人话：哪一刻用过、有几条会话、认没认出是哪个号。"""
    if not act.get("ok"):
        return act.get("why") or ""
    if act.get("count"):
        return (f"本地会话记录显示这个号今天 {act.get('last') or ''} 还发过对话"
                f"（{act['count']} 条会话）")
    if act.get("other_today"):
        return ("本地会话库今天有对话，但认不出属于这个号（缺 user_id），"
                "所以不算它今天的活跃")
    return "本地会话记录显示这个号今天还没发过对话"


def _wb_bonus_today(intl=False, auth_file=None):
    """读取 Bonus Pack 并按次日结算惯例推测所属活跃日。

    CycleStartTime 是到账时间，不是活跃日期。厂商未返回活跃日期，不能把
    推算写成确定事实；按最近观察到的次日到账惯例标注为推测。
    """
    login, err = _wb_read_login(intl, auth_file)
    if login is None:
        return None, None, err or "读不到 WorkBuddy 的登录令牌"
    rows = _wb_pack_accounts(_wb_base(login["domain"]), login["token"], str(login.get("uid") or ""))
    bonus = [r for r in rows if str(r.get("PackageName") or "").strip().lower() == "bonus pack"
             and _wb_num(r.get("CycleCapacitySize")) == 30]
    entries = []
    for row in bonus:
        start = str(row.get("CycleStartTime") or "")
        try:
            arrived = dt.date.fromisoformat(start[:10])
        except ValueError:
            continue
        entries.append({"arrived_at": start, "inferred_activity_date":
                        (arrived - dt.timedelta(days=1)).isoformat()})
    entries.sort(key=lambda x: x["arrived_at"], reverse=True)
    return entries, len(bonus), ""


# 活跃地图「发一句对话」用哪个模型。
#
# 2026-10-17 她的账本：`只回复两个字：收到` 在 hy4-preview 上**扣了 0.91 积分**——
# 这个任务是纯打卡，不该花她一分钱。WorkBuddy 的 CLI 认 `--model`，
# 里面有两个 0.00x 的限免档：
#   step-5-preview  独家限免（她截图里勾着的那个）
#   hy3             限时免费（2026-07-06 ~ 10-31，每日赠送免费额度）
# 默认按顺序试，前一个挂了换下一个；都挂了才退回"不指定模型"。
# 想改：设环境变量 AGENTHUB_ACTIVITY_MODEL（填 none 表示不指定、走客户端默认）。
WB_ACTIVITY_MODELS = ("step-5-preview", "hy3")


def _wb_intl_free_activity_models(login):
    """Only use zero-priced chat models from this account's live config."""
    headers = {
        "Authorization": "Bearer " + login["token"], "Accept": "application/json",
        "X-User-Id": str(login.get("uid") or ""), "X-Domain": login.get("domain") or "",
        "X-Product": "SaaS", "X-IDE-Type": "WorkBuddy", "X-IDE-Name": "WorkBuddy",
        "X-IDE-Version": "5.8.4", "User-Agent": "WorkBuddy/5.8.4 WorkBuddy/5.8.4",
    }
    try:
        req = UrlRequest("https://www.workbuddy.ai/v3/config", headers=headers)
        with direct_open(req, timeout=20) as resp:
            data = json.loads(resp.read().decode("utf-8"))
        body = data.get("data") if isinstance(data.get("data"), dict) else data
        models = body.get("models")
        if not isinstance(models, list):
            raise ValueError("模型目录缺少 models")
    except Exception as e:  # noqa: BLE001
        return [], f"无法读取国际版实时模型价格（{type(e).__name__}），未发送对话"
    free = []
    excluded = ("image", "video", "t2v", "i2v", "completion", "rewrite", "-nes",
                "jump", "codewise", "embed", "rerank", "kling", "tts")
    for item in models:
        if not isinstance(item, dict):
            continue
        mid, price = item.get("id"), item.get("credits")
        if not isinstance(mid, str) or not mid.strip() or not isinstance(price, str):
            continue
        mid = mid.strip()
        if any(x in mid.lower() for x in excluded):
            continue
        if re.fullmatch(r"(?:[xX]\s*)?0(?:\.0+)?(?:\s*credits)?", price.strip(), re.I):
            if mid not in free:
                free.append(mid)
    preferred = ("hy3", "deepseek-v4.1-flash")
    free.sort(key=lambda x: (preferred.index(x.lower()) if x.lower() in preferred else len(preferred), x.lower()))
    return free, ""


def _wb_model_unavailable(raw, returncode):
    """Retry only a clear model rejection with no reported token use."""
    try:
        payload = json.loads(raw)
    except (TypeError, ValueError):
        payload = None
    records = payload if isinstance(payload, list) else [payload] if isinstance(payload, dict) else []
    for record in records:
        if not isinstance(record, dict):
            continue
        usage = record.get("usage") or {}
        if isinstance(usage, dict) and any(_wb_num(usage.get(k)) > 0 for k in ("input_tokens", "output_tokens")):
            return False
        if record.get("type") == "result" and record.get("subtype") == "success" and not record.get("is_error"):
            return False
    text = raw.lower()
    rejected = "model" in text and any(x in text for x in ("not found", "invalid", "unsupported", "unavailable", "not available"))
    if not rejected:
        return False
    return returncode != 0 or any(isinstance(r, dict) and r.get("is_error") is True for r in records)



# 发给 CLI 子进程的环境白名单：只留系统基础变量 + 它自己的登录令牌。
# 两个原因，都是 2026-10-15 实测出来的：
# ① 从别的程序（驾驶舱、终端、Agent 宿主）里被拉起来时，环境里会带着 *_proxy 与
#    CODEBUDDY_MCP_CONFIG / CODEBUDDY_SERVICE_PROXY_URL。CLI 的 setProxy 会在
#    「Right-hand side of 'instanceof' is not callable」上崩掉，而且**它不退出、就挂在那儿**，
#    把 180 秒超时整个吃掉（同一次实测里带代理的两次全超时，干净环境 40 秒内就有结论）。
# ② MCP 配置灌进去，会让这次「只回两个字」的会话去连一堆它根本用不上的服务。
_WB_ENV_KEEP = frozenset({
    "PATH", "PATHEXT", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC",
    "TEMP", "TMP", "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "APPDATA", "LOCALAPPDATA",
    "PROGRAMDATA", "PROGRAMFILES", "PROGRAMFILES(X86)", "PROGRAMW6432",
    "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "PROCESSOR_IDENTIFIER",
    "OS", "USERNAME", "USERDOMAIN", "COMPUTERNAME", "LANG",
})


def _wb_cli_env(token):
    env = {k: v for k, v in os.environ.items() if k.upper() in _WB_ENV_KEEP}
    env["CODEBUDDY_AUTH_TOKEN"] = token
    return env


def _wb_bundle_env(cli):
    """按产物存在与否显式点名走哪个 bundle，返回 (env, 名字)。

    2026-10-15 实测：国内版 `cli/dist/` 里只剩 `codebuddy-lite-wb.mjs`——旧的全量
    `codebuddy.js` 与 `codebuddy-headless.js` 在这次客户端自更新里没了，而 `bin/codebuddy`
    的默认回退链最后仍然 `require('../dist/codebuddy')`，于是直接
    「Cannot find module '../dist/codebuddy'」退出 1，活跃地图那句对话永远发不出去。
    （同一个目录下 `--version` 走的是不加载 bundle 的快速路径，所以看着一切正常。）

    顺序 lite-wb → headless → 不指定（full），取第一个真有产物的：CLI 自己也是这个优先级，
    我们只是把它**明说**出来，免得回退链踩到一个不存在的文件上。
    """
    dist = cli.parent.parent / "dist"
    if (dist / "codebuddy-lite-wb.mjs").is_file():
        return {"CODEBUDDY_FORCE_LITE_WB_BUNDLE": "1"}, "lite-wb"
    if (dist / "codebuddy-headless.js").is_file():
        return {"CODEBUDDY_FORCE_HEADLESS_BUNDLE": "1"}, "headless"
    return {}, "full"



def _wb_send_activity(intl=False, auth_file=None):
    """替用户发一句对话，让服务端认定这个号今天活跃过。
    跑在临时空目录里，别在她真工作区跑——那会把项目的 AGENTS.md/记忆塞进提示词。
    （实测省不下多少：空目录一句"收到"也要 6,458 token，其中 6,354 是缓存的系统提示，
      但绝不该拿她的仓库当这个定时任务的 cwd。）"""
    cli = _wb_cli_path(intl)
    if cli is None or not cli.is_file():
        return {"error": f"没找到 {'国际版' if intl else '国内版'}自带的命令行（{cli}）"}
    login, err = _wb_read_login(intl, auth_file)
    if login is None:
        return {"error": err or "读不到 WorkBuddy 的登录令牌"}
    bundle_env, bundle = _wb_bundle_env(cli)
    env = _wb_cli_env(login["token"])
    env.update(bundle_env)
    want = os.environ.get("AGENTHUB_ACTIVITY_MODEL", "").strip()
    if intl:
        models, model_err = _wb_intl_free_activity_models(login)
        if model_err:
            return {"error": model_err}
        if want:
            models = [m for m in models if m.lower() == want.lower()]
            if not models:
                return {"error": "指定模型不在国际版实时零倍率目录中，未发送对话"}
        if not models:
            return {"error": "国际版实时目录没有明确零倍率的聊天模型，未发送对话"}
    else:
        models = [None] if want.lower() == "none" else ([want] if want else list(WB_ACTIVITY_MODELS) + [None])
    r = None
    used = None
    for m in models:
        args = ["node", str(cli), "-p", "只回复两个字：收到",
                "--output-format", "json", "--max-turns", "1", "--tools", ""]
        if m:
            args += ["--model", m]
        try:
            r = subprocess.run(args, capture_output=True, timeout=180, env=env,
                               cwd=str(_wb_activity_dir(intl)),
                               text=True, encoding="utf-8", errors="replace")
        except FileNotFoundError:
            return {"error": "本机 PATH 里没有 node，跑不了 WorkBuddy 的命令行"}
        except subprocess.TimeoutExpired:
            return {"error": "发对话超过 180 秒没回，先当成没发成"}
        used = m
        # 这个模型不可用（下架 / 没权限 / 不支持）就换下一个；其它错误交给下面统一解释
        raw0 = f"{r.stderr or ''}{r.stdout or ''}"
        if m and ((intl and _wb_model_unavailable(r.stdout or r.stderr or "", r.returncode)) or
                  (not intl and r.returncode != 0 and ("model" in raw0.lower() and
                   ("not found" in raw0.lower() or "invalid" in raw0.lower()
                    or "unsupported" in raw0.lower())))):
            continue
        break
    if r.returncode != 0:
        # 把"为什么没跑起来"说成人话。裸堆栈（Cannot find module …）她看不懂，
        # 而这几种失败各有一句能直接照做的处置
        raw = f"{r.stderr or ''}{r.stdout or ''}"
        if "Cannot find module" in raw and "dist/" in raw:
            miss = raw.split("Cannot find module", 1)[1].split("\n", 1)[0].strip().strip("'\"")
            return {"error": f"客户端自带的命令行找不到它的程序包（{miss}），"
                             f"多半是刚自更新过、产物换了名字。这次按 {bundle} 通道重试仍失败："
                             "把 WorkBuddy 打开一次让它自己修完，或干脆在客户端里手动发一句话"}
        if "instanceof" in raw and "setProxy" in raw:
            return {"error": "命令行被环境里的代理变量绊住了（setProxy 崩了）。"
                             "从带 *_proxy 的终端里启动驾驶舱就会这样，换从开始菜单启动即可"}
        if "401" in raw or "Authentication required" in raw:
            return {"error": f"命令行说没登录（401）。{login.get('nickname') or ''} 这份令牌"
                             "可能过期了，把客户端打开一次让它续期"}
        return {"error": f"命令行返回 {r.returncode}（{bundle} 通道）：{raw[:200]}"}
    # ⚠ 令牌拿错那份 CLI 时它 **rc=0** 却把 "401 Authentication required" 当正文吐出来
    # （实测：国内版令牌 + 国际版那份 CLI）。只看 returncode 会把没登录当成发成功。
    tokens, ok = 0, False
    try:
        for m in json.loads(r.stdout or "[]"):
            if m.get("type") == "result":
                u = m.get("usage") or {}
                tokens = int(u.get("input_tokens") or 0) + int(u.get("output_tokens") or 0)
                ok = (m.get("subtype") == "success" and not m.get("is_error"))
    except ValueError:
        return {"error": f"命令行没回 JSON：{(r.stdout or r.stderr or '')[:160]}"}
    if not ok:
        return {"error": f"这一句没跑成：{(r.stdout or r.stderr or '')[:160]}"}
    # 会话文件在 CLI 自己的项目目录里（按 cwd 分项目），桌面客户端的会话列表读不到它——
    # 把路径一并报出去，不然用户只会问"你发的那句对话跑哪去了"
    sess_dir = ""
    try:
        cand = sorted((Path.home() / ".codebuddy" / "projects").glob(
            "*daily-activity-" + ("intl" if intl else "cn")),
            key=lambda p: p.stat().st_mtime, reverse=True)
        sess_dir = str(cand[0]) if cand else ""
    except OSError:
        pass
    return {"claimed": True, "message": "已发一句对话", "credit": None,
            "tokens": tokens, "model": used or "（客户端默认）",
            "model_price_source": "国际版实时模型配置 x0.00" if intl else "",
            "account": login.get("nickname") or "",
            "session_dir": sess_dir}


WB_CN_BASE = "https://www.workbuddy.cn"


def _wb_cn_api(path, body=None, write=False, auth_file=None):
    """国内版「成长中心」的接口（活跃地图 / 旅行猫），鉴权就是桌面端那份 bearer。
    实测（2026-10-14）：status / config / heatmap / records 四条只读全通，
    路径与请求体形状是从 assets/GrowthCenterPage 与 growthSpace 那两个前端 chunk 里读到的：
      POST /activity/growth/buddy/travel/depart  body {"location_id": <int>}
      POST /activity/growth/buddy/travel/claim   body {}
    返回统一是 {code, msg, requestId, data}，code!=0 就是失败——只看 HTTP 状态会漏。"""
    login, err = _wb_read_login(intl=False, auth_file=auth_file)
    if login is None:
        return None, err or "读不到 WorkBuddy 国内版的登录令牌"
    h = {"Accept": "application/json", "Content-Type": "application/json",
         "Authorization": f"Bearer {login['token']}", "User-Agent": CHECKIN_UA,
         "Origin": WB_CN_BASE, "Referer": WB_CN_BASE + "/profile/growth-center"}
    d, e = _ci_req(WB_CN_BASE + path, h, body if write else None,
                   tag="wb-cn " + path.rsplit("/", 1)[-1], timeout=30)
    if d is None:
        return None, e
    if d.get("code") != 0:
        return None, f"服务端 code={d.get('code')} {str(d.get('msg') or '')[:80]}"
    return d.get("data") or {}, ""


def _wb_date_of_ts(ts):
    try:
        return dt.datetime.fromtimestamp(int(ts)).date().isoformat()
    except (TypeError, ValueError, OSError):
        return ""


def ci_workbuddy_activity(claim=False, auth_file=None):
    """WorkBuddy 国内版「10月活跃地图」：页面原话「完成一次对话才能被记录」，
    连登 14 天兑换奖励。判据只认 /activity/growth/heatmap 里今天那格的 score>0——
    实测发一句国内版 CLI 对话之前那格是 score:0，发完变 score:2，因果是坐实的。
    国内版另有真签到（ci_workbuddy），两件事分两条，别合成一条。"""
    data, err = _wb_cn_api("/activity/growth/heatmap", auth_file=auth_file)
    if data is None:
        return {"error": err}
    today = dt.date.today().isoformat()
    cell = next((c for c in (data.get("cells") or []) if c.get("date") == today), None)
    scored = bool(cell and _wb_num(cell.get("score")) > 0)
    streak = sum(1 for c in (data.get("cells") or [])
                 if _wb_num(c.get("score")) > 0 and str(c.get("date") or "") <= today)
    if not claim:
        return {
            "supported": True, "auto": True, "mode": "activity",
            "activity_on": True, "signed_today": scored, "claimable": not scored,
            "streak": streak, "reward": None, "unit": "活跃",
            "words": {"done": "今天已打卡", "go": "今天还没打卡", "act": "发一句对话"},
            "note": "活跃地图要「完成一次对话」才记录当天（连登 14 天可兑换奖励）；"
                    "这一条只管打卡，签到是上面 WorkBuddy 那条",
            "source": WB_CN_BASE + "/activity/growth/heatmap",
        }
    if scored:
        return {"claimed": False, "already": True, "message": "今天这一格已经打卡了"}
    return _wb_send_activity(intl=False)


def ci_workbuddy_travel(claim=False, auth_file=None):
    """WorkBuddy 国内版「Buddy 旅行猫」：派他去旅行 → 1~4 小时后到达 → 领取 5~10 积分。
    状态机 idle / traveling / arrived，每天只能**派**一次（daily_limit_reached 管派出，
    不管领取）；领取按趟计，一趟一趟各领各的，跨天领（昨晚到、今天领）完全正常。
    「今天领过没有」不猜状态字，看 records 里有没有一笔 claimed_at 落在今天——
    但它只回答"今天有没有领取动作"，回答不了"眼前这趟领了没有"：state=arrived
    本身就说明当前这趟没领（领完即回 idle）。"""
    st, err = _wb_cn_api("/activity/growth/buddy/travel/status", auth_file=auth_file)
    if st is None:
        return {"error": err}
    rec, _e2 = _wb_cn_api("/activity/growth/buddy/travel/records?page=1&limit=8", auth_file=auth_file)
    today = dt.date.today().isoformat()
    rows = (rec or {}).get("records") or []
    claimed_today = any(_wb_date_of_ts(r.get("claimed_at")) == today for r in rows if r.get("claimed_at"))
    state = str(st.get("state") or "")
    rw = _wb_num(st.get("reward_credit"))
    reward = int(rw) if rw and float(rw).is_integer() else (rw or None)
    # 派出的名额归服务端 daily_limit_reached 管（今天派过没有），跟"今天领过没有"是两回事——
    # 昨晚回来那趟今天早上才领，领完照样得把今天的猫派出去（10-14 就这么白白浪费过一天：
    # 上午领了昨晚那趟 → 整天不再派 → records 里 10-14 直接缺一趟）
    can_depart = state == "idle" and not st.get("daily_limit_reached")
    can_claim = state == "arrived"
    arrive_text = ""
    if st.get("arrive_at"):
        try:
            arrive_text = dt.datetime.fromtimestamp(int(st["arrive_at"])).strftime("%m-%d %H:%M")
        except (TypeError, ValueError, OSError):
            arrive_text = ""
    # 这一条是**状态机**，不是"签到"：派出去 → 在路上 → 到点领。
    # 所以给界面一个 next_action，让操作列写"派出 / 等待中 / 领取"这三个真动作，
    # 而不是一句含糊的"推进这一步"（2026-10-15 她原话：难听死了）。
    # 到达未领（state=arrived）必须排最前：claimed_today 只说明"今天领过一笔"，
    # 领的可能是昨晚那趟——把它排在前面会把眼前这趟没领的盖成"今天已领完"，
    # 状态列/操作列双双失真、奖励漏领（2026-10-25 20:23 到的这趟就是这么被盖住的）。
    if can_claim:
        next_action = "claim"
    elif can_depart:
        next_action = "depart"
    elif state == "traveling":
        next_action = "wait"
    else:
        next_action = "done" if claimed_today else "wait"
    if not claim:
        return {
            "supported": True, "auto": True, "mode": "travel",
            "activity_on": True, "signed_today": claimed_today,
            "claimable": bool(can_claim or can_depart),
            "reward": reward, "unit": "积分", "travel_state": state,
            "next_action": next_action, "arrive_at": arrive_text,
            "words": {
                "done": "今天已领取", "go": "今天可派可领",
                "depart": "派出", "wait": "等待中", "claim": "领取",
                "wait_badge": "猫猫在路上", "done_act": "今天已领完",
                "arrive_badge": "猫猫到了，待领取", "home_badge": "猫猫在家，可派出",
            },
            "note": "1~4 小时一趟、每天一趟；先派、到点再领"
                    + (f"（这一趟约 {arrive_text} 到）" if arrive_text and next_action == "wait" else "")
                    + ("。今天已领过一趟（领的可能是昨晚那趟），眼前这趟照领不误"
                       if next_action == "claim" and claimed_today else ""),
            "source": WB_CN_BASE + "/activity/growth/buddy/travel/status",
        }
    if can_claim:
        d, e = _wb_cn_api("/activity/growth/buddy/travel/claim", body={}, write=True, auth_file=auth_file)
        if d is None:
            return {"error": e}
        # 领完这一趟，今天还有没有下一步：派出名额没用完就还能再派（跨天领的常见后续）。
        # 驾驶舱凭这个决定要不要把这家记成"今天跑完"——记早了，当天自动领就被钉死。
        return {"claimed": True, "message": f"已领取这一趟的旅行奖励（{reward} 积分）",
                "credit": reward,
                "next_action": "depart" if not st.get("daily_limit_reached") else "done"}
    if can_depart:
        cfg, e = _wb_cn_api("/activity/growth/buddy/travel/config", auth_file=auth_file)
        if cfg is None:
            return {"error": e}
        locs = cfg.get("locations") or []
        # 挑奖励上限最高的那一档，并列时挑用时最短的——同样一趟，尽量多拿早回
        pick = sorted(locs, key=lambda x: (-_wb_num(x.get("reward_credit_max")),
                                           _wb_num(x.get("duration_hours_min"))))[:1]
        if not pick:
            return {"error": "旅行配置里一个地点都没有"}
        d, e = _wb_cn_api("/activity/growth/buddy/travel/depart",
                          body={"location_id": pick[0].get("id")}, write=True,
                          auth_file=auth_file)
        if d is None:
            return {"error": e}
        return {"claimed": False, "settling": True, "credit": None, "next_action": "wait",
                # 这一步是**做成了**，只是奖励要等猫回来——界面按"推进了一步"画，
                # 不许染成红色"没领成"（她 2026-10-23 原话：写"已派去咖啡馆"就行了）。
                # 括号里那句是给界面看的说明，别塞内部机制（"今天还会被推第二次"）。
                "message": f"已派去{pick[0].get('name')}，约 "
                           f"{pick[0].get('duration_hours_min')}~{pick[0].get('duration_hours_max')} 小时到；"
                           "到了这一行会变成「领取」"}
    # 在路上要排在"今天领过"前面：跨天领之后又派出去时，claimed_at 落在今天、
    # 但路上这趟明明还没领，说"今天这一趟已经领过了"是假话
    if claimed_today and state != "traveling":
        return {"claimed": False, "already": True, "next_action": "done",
                "message": "今天这一趟已经领过了"}
    return {"claimed": False, "settling": True, "next_action": "wait",
            "message": f"猫猫还在路上（状态 {state}），到点再领"}


def ci_workbuddy(intl=False, claim=False, auth_file=None):
    """WorkBuddy / CodeBuddy 系签到。实测（2026-10-12）：
    只有 `checkin-activity-status` 这条给真状态，同族老接口 `checkin-status`
    对这个号返回 active=false——照老的判就会以为活动没开，别用那条。
    国内版走 www.codebuddy.cn（登录态在 codebuddy.cn 侧），国际版走 www.workbuddy.ai
    且活动未开（active=false），所以国际版我们如实报"没开"，不去点领取。"""
    login, err = _wb_read_login(intl, auth_file)
    if login is None:
        return {"error": err}
    base = _wb_base(login["domain"])
    headers = {
        "Accept": "application/json", "Content-Type": "application/json",
        "Authorization": f"Bearer {login['token']}", "User-Agent": CHECKIN_UA,
        "X-User-Id": str(login.get("uid") or ""),
    }
    if not claim:
        d, e = _ci_req(base + "/v2/billing/meter/checkin-activity-status", headers, {},
                       tag="wb status")
        if d is None:
            return {"error": e, "account": login.get("nickname") or ""}
        b = _ci_body(d)
        active = _ci_pick(b, "active")
        signed = _ci_pick(b, "today_checked_in", "todayCheckedIn")
        if intl and active is not True:
            # 国际版没有"签到"这个活动，30 积分/天写在 Free 套餐页的基础权益里
            # （「每日活跃奖励：30 积分/天」）。到账只能读额度里的逐笔账，
            # 而"今天活跃过没有"服务端不给——那条只能从桌面端自己的会话库读（按 user_id）。
            # 她 2026-11-03："我这个账号今天已经自己发起过对话了…总因为各种原因识别不到，
            # 还让我重新发"——以前只认驾驶舱自己点过的那次，她自己在客户端聊过就白瞎了。
            entries, n, berr = _wb_bonus_today(intl=True, auth_file=auth_file)
            if entries is None:
                return {"error": berr, "account": login.get("nickname") or ""}
            today = dt.date.today().isoformat()
            act = _wb_local_activity(login.get("uid"), intl=True)
            latest = entries[0] if entries else {}
            inferred_today = next((x for x in entries if x["inferred_activity_date"] == today), None)
            detail = (f"最近到账 {latest['arrived_at']}，按次日结算惯例推测属于 "
                      f"{latest['inferred_activity_date']} 的活跃（厂商未提供活跃日期）"
                      if latest else "未发现可解析的 30 积分到账记录")
            done_today = bool(act.get("count"))
            words = {"go": "今天可签到", "act": "发一句对话"}
            if done_today:
                # 今天这一档已经完成，别再把「发一句对话」摆在她面前
                words.update({"done": "今天已活跃", "done_act": "今天已用过"})
            return {
                "supported": True, "auto": True, "mode": "activity",
                "account": login.get("nickname") or "",
                "user_id": str(login.get("uid") or ""),
                "activity_on": True,
                "signed_today": done_today, "claimable": not done_today,
                "reward": 30, "bonus_packs": n, "unit": "积分",
                "bonus_entries": entries, "inferred_today_arrival": bool(inferred_today),
                "activity_at": act.get("last") or "",
                "activity_count": int(act.get("count") or 0),
                "words": words,
                "note": _wb_activity_note(act) + "；" + detail
                        + "。国际版这 30 积分按次日到账，不用点领取。",
                "source": f"{base}/billing/meter/get-user-resource-free-packages",
            }
        return {
            "supported": True, "auto": True,
            "account": login.get("nickname") or "", "user_id": str(login.get("uid") or ""),
            "activity_on": active, "signed_today": signed,
            # 领取要同时满足"活动开着"和"今天还没签"；active 缺省不许当 true，
            # 厂商改字段名时把缺省判成可用会真发起领取（cockpit-tools 就踩过这个形状）
            "claimable": (active is True) and (signed is False),
            "streak": _ci_pick(b, "streak_days", "streakDays"),
            "reward": _ci_pick(b, "daily_credit", "dailyCredit"),
            "today_credit": _ci_pick(b, "today_credit", "todayCredit"),
            "total": _ci_pick(b, "total_credits", "totalCredits"),
            "days_signed": len(b.get("checkin_dates") or b.get("checkinDates") or []),
            "ends_on": str(_ci_pick(b, "end_time", "endTime") or ""),
            "unit": "积分", "source": f"{base}/v2/billing/meter/checkin-activity-status",
        }
    if intl:
        # 发之前先看本地证据：今天这个号已经聊过（她自己开的客户端、或早先驾驶舱发过的那句），
        # 就不再起一次命令行会话——重复发既烧 token，也是她抱怨的"还让我重新发"。
        # 真要强制再发一次：AGENTHUB_ACTIVITY_FORCE=1（给自己排查用，界面不暴露）。
        act = _wb_local_activity(login.get("uid"), intl=True)
        forced = os.environ.get("AGENTHUB_ACTIVITY_FORCE", "").strip().lower() in ("1", "true", "yes")
        if act.get("count") and not forced:
            return {
                "claimed": False, "already": True, "settling": True, "settling_final": True,
                "activity_at": act.get("last") or "", "activity_count": int(act.get("count") or 0),
                "account": login.get("nickname") or "",
                "message": (f"本地会话记录显示这个号今天 {act.get('last') or ''} 已经发过对话"
                            f"（{act['count']} 条会话），这次没再重复发；今天的 30 积分按次日到账"),
            }
        entries, n, berr = _wb_bonus_today(intl=True, auth_file=auth_file)
        if entries is None:
            return {"error": berr}
        r = _wb_send_activity(intl=True, auth_file=auth_file)
        if r.get("error"):
            return r
        r["claimed"] = False
        r["settling"] = True
        r["settling_final"] = True
        latest = entries[0] if entries else None
        detail = (f"；最近一笔于 {latest['arrived_at']} 到账，按次日结算惯例推测属于 "
                  f"{latest['inferred_activity_date']} 的活跃" if latest else "")
        r["message"] = (f"今天的对话已完成（{r.get('tokens')} token，{r.get('model')}；"
                        f"{r.get('model_price_source')}），今天的积分尚待到账"
                        + detail + "。归属日期为推测，厂商未提供活跃日期。")
        return r
    d, e = _ci_req(base + "/v2/billing/meter/daily-checkin", headers, {}, tag="wb claim")
    if d is None:
        return {"error": e}
    b = _ci_body(d)
    code = d.get("code")
    # 这个网关的话术键是 msg 不是 message（"今天已签到，请明天再来"就靠它判 already），
    # 只认 message 的话这条会被当成一次普通失败，界面上一片红
    msg = str(_ci_pick(b, "message", "msg") or d.get("message") or d.get("msg") or "")
    # 已经签过不是异常：服务端 code!=0 + "已签到"，重试会一直撞在这句上
    already = ("已签" in msg) or ("已领" in msg) or ("already" in msg.lower())
    ok = (code in (0, None, 200)) and (_ci_pick(b, "success") is not False)
    return {"claimed": bool(ok), "already": already, "message": msg or ("领取成功" if ok else "没领成"),
            "credit": _ci_pick(b, "credit", "today_credit", "todayCredit"),
            "streak": _ci_pick(b, "streak_days", "streakDays"),
            "rewards": b.get("rewards") or _ci_pick(b, "reward")}


def ci_trae(claim=False):
    """Trae CN 签到。令牌在 storage.json 的 iCubeAuthInfo://icube.cloudide（byteCrypto，
    我们这边已能解）；status 与 claim 都要带 x-device-id，缺了服务端会退化身份。"""
    appdata = Path(os.environ.get("APPDATA", ""))
    login = None
    for name in ("Trae CN", "TRAE SOLO CN"):
        info, _err = _trae_read_login(appdata / name)
        if info:
            login = info
            break
    if login is None:
        return {"error": "没找到 Trae CN 的登录态（%APPDATA%\\Trae CN 或 TRAE SOLO CN）"}
    host = (login.get("host") or "https://api.trae.cn").rstrip("/")
    uid = str(login.get("user_id") or "")
    headers = {
        "Accept": "application/json", "Content-Type": "application/json",
        "Authorization": f"Cloud-IDE-JWT {login['token']}",
        "x-device-id": uid, "x-app-type": "trae",
        "Origin": "https://www.trae.cn", "Referer": "https://www.trae.cn/",
        "User-Agent": CHECKIN_UA,
    }
    path = "/trae/api/v2/ug/checkin_credits/claim" if claim \
        else "/trae/api/v2/ug/checkin_credits/status"
    d, e = _ci_req(host + path, headers, {})
    if d is None:
        return {"error": e}
    b = _ci_body(d)
    code = d.get("code")
    if code not in (0, None, 200):
        # Trae 侧非 0 基本都是令牌过期
        return {"error": f"Trae 返回 code={code} {str(d.get('message') or '')[:100]}"}
    if claim:
        return {"claimed": True, "message": str(d.get("message") or "已提交领取"),
                "credit": _ci_pick(d, "credits"), "extra": _ci_pick(d, "extra_credits")}
    signed = _ci_pick(d, "checked_in", "checkedIn")
    did = _ci_pick(d, "did_checked_in", "didCheckedIn")
    return {
        "supported": True, "auto": True,
        "account": (login.get("who") or {}).get("name") or uid[:8],
        "user_id": uid,
        "activity_on": _ci_pick(d, "enable"),
        # 它给两个标记：账号维度与设备维度，任一为真就说明今天这份名额已经动过
        "signed_today": bool(signed) or bool(did),
        "claimable": (_ci_pick(d, "enable") is True) and not (signed or did),
        "streak": _ci_pick(d, "consecutive_days", "consecutiveDays"),
        "reward": _ci_pick(d, "credits"), "extra": _ci_pick(d, "extra_credits"),
        "unit": "积分", "source": host + path,
    }


def ci_minimax(claim=False):
    """MiniMax Code 七日签到。status 必须带 timezone_id，缺了服务端直接
    1406010011 invalid timezone_id。days[] 的 status：1 未到期 / 2 可领 / 3 已领 / 4 停用。"""
    try:
        token, _auth_from, _refreshed = _minimax_access()
    except Exception as e:      # noqa: BLE001
        return {"error": str(e)}
    origin = "https://agent.minimax.cn"
    headers = {
        "Accept": "application/json", "Content-Type": "application/json",
        "Authorization": f"Bearer {token}", "Origin": origin, "Referer": origin + "/",
        "User-Agent": CHECKIN_UA,
    }
    if not claim:
        d, e = _ci_req(origin + "/minimax-cloud/api/v1/signin/status?timezone_id=" + CI_TZ,
                       headers, tag="minimax status")
        if d is None:
            return {"error": e}
        base_resp = d.get("base_resp") or {}
        if base_resp.get("status_code") not in (0, None):
            return {"error": "MiniMax 签到状态查询失败：" + str(base_resp.get("status_msg") or base_resp.get("status_code"))}
        b = _ci_body(d)
        days = b.get("days") or []
        today = next((x for x in days if _ci_pick(x, "is_today", "isToday")), None)
        return {
            "supported": True, "auto": True, "account": "",
            "activity_on": bool(days),
            "signed_today": str(_ci_pick(today or {}, "status")) == "3" if today else None,
            "claimable": any(str(_ci_pick(x, "status")) == "2" for x in days),
            "streak": sum(1 for x in days if str(_ci_pick(x, "status")) == "3"),
            "reward": _ci_pick(today or {}, "points", "bonus_points"),
            "cycle": [{"day": _ci_pick(x, "day_no", "dayNo"), "points": x.get("points"),
                       "status": _ci_pick(x, "status")} for x in days],
            "unit": "积分", "source": origin + "/minimax-cloud/api/v1/signin/status",
        }
    d, e = _ci_req(origin + "/minimax-cloud/api/v1/signin/claim?timezone_id=" + CI_TZ,
                   headers, {}, tag="minimax claim")
    if d is None:
        return {"error": e}
    # 成败在 base_resp 里，data 是空的——只盯着 data 找字段的话，一条
    # `{"base_resp":{"status_code":1406010011,"status_msg":"invalid timezone_id"}}`
    # 会被读成"没报错就是领到了"（2026-10-13 就是这么连着假成功了四次）
    br = d.get("base_resp") or {}
    code = br.get("status_code")
    msg = str(br.get("status_msg") or "")
    b = _ci_body(d)
    res = str(_ci_pick(b, "claim_result", "claimResult") or "")
    already = ("already" in msg.lower()) or ("已领" in msg) or ("已签" in msg) or res in ("2", "AlreadyClaimed")
    ok = (code == 0) and res not in ("2", "AlreadyClaimed")
    return {"claimed": bool(ok), "already": already,
            "message": str(_ci_pick(b, "message") or msg or ("已领取" if ok else "没领成")),
            "code": code,
            "day": _ci_pick(b, "day_no", "dayNo"), "credit": _ci_pick(b, "points")}


# 客户端自带的那套设备身份组件（设备风控）。服务端只对"带了设备令牌"的调用方下发
# 每日领取那条活动，其余一律藏起来——2026-10-18 实测，同一个 uid、同一个令牌：
#   带 ClientType/MachineId/Version      → campaigns=[BOGO(VIEW_DETAILS)]，看着像"今天没活动"
#   再加 MachineToken/MachineCode/Type   → 多一条 CLAIM_BENEFIT 100 积分（CLAIMABLE，领得动）
# 客户端日志里那条请求的头部逐字对得上（Cosy-MachineToken 被日志打码，值由这个 exe 现算）。
QODER_UMID_LAUNCHER = {"qoder": ("Qoder", "Qoder Launcher"), "Qoder": ("Qoder", "Qoder Launcher"),
                       "qoder-cn": ("Qoder CN", "Qoder CN Launcher"),
                       "QoderCN": ("Qoder CN", "Qoder CN Launcher")}
QODER_IDENT_CACHE = Path.home() / ".agenthub" / "qoder-ident.json"
QODER_IDENT_TTL = 1800          # 客户端一个进程算一次就复用；我们也别每轮都去 spawn 一个 exe


def _qoder_umid_exe(store):
    """找这个产品的 resources/umid/runtime-info.exe。
    0.4.x 换壳后安装位置由 launcher 管，只能从 launcher 的 state.ini/install.ini 里读
    （installDir + appExecutable），硬编码盘符换个版本就失效。"""
    local = Path(os.environ.get("LOCALAPPDATA") or os.environ.get("APPDATA", ""))
    prod = QODER_UMID_LAUNCHER.get(store)
    cands, installs = [], []
    if prod:
        # 两种布局都试：%LOCALAPPDATA%\<产品>\<产品> Launcher\state.ini（当前）
        # 和 %LOCALAPPDATA%\<产品> Launcher\state.ini（早期安装器）
        dirs = [local / prod[0] / prod[1], local / prod[1]]
        for d in dirs:
            for ini_name in ("state.ini", "install.ini"):
                try:
                    txt = (d / ini_name).read_text(encoding="utf-8", errors="ignore")
                except OSError:
                    continue
                kv = dict(re.findall(r"^([A-Za-z]+)=(.*)$", txt, re.M))
                inst = (kv.get("installDir") or "").strip()
                exe = (kv.get("appExecutable") or "").strip()
                if not inst:
                    continue
                installs.append(Path(inst))
                if exe:
                    ver_dir = Path(exe.replace("\\", "/")).parent
                    cands.append(Path(inst) / ver_dir / "resources" / "umid" / "runtime-info.exe")
    for inst in installs:
        cands.append(inst / "resources" / "umid" / "runtime-info.exe")   # 旧壳布局
        try:                                                            # 新壳：版本目录里挑最新
            for p in sorted(inst.glob(".qoder-versions/*/resources/umid/runtime-info.exe"),
                            reverse=True):
                cands.append(p)
        except OSError:
            pass
    for p in cands:
        if p.is_file():
            return p
    return None


def _qoder_machine_identity(store, fresh=False):
    """跑一次客户端的 runtime-info.exe 拿设备令牌。失败就返回 None（不抛）。
    结果按 exe 路径缓存在 ~/.agenthub/qoder-ident.json：这个 exe 要 4 秒才出结果，
    而一次签到刷新会起好几个 agenthub 进程，不缓存等于每行都等 4 秒。"""
    exe = _qoder_umid_exe(store)
    if not exe:
        return None
    now = time.time()
    if not fresh:
        try:
            j = json.loads(QODER_IDENT_CACHE.read_text(encoding="utf-8"))
            if (j.get("exe") == str(exe) and j.get("machineToken")
                    and now - float(j.get("at") or 0) < QODER_IDENT_TTL):
                return j
        except Exception:       # noqa: BLE001 缓存坏了就当没有
            pass
    try:
        p = subprocess.run(
            [str(exe), "production", "--account-stdin"],
            input=json.dumps({"account": ""}), capture_output=True, text=True,
            timeout=30, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        j = json.loads(p.stdout or "{}")
    except Exception:           # noqa: BLE001 没装/被杀/输出不是 JSON：退回不带令牌
        return None
    if not j.get("machineToken"):
        return None
    j["exe"], j["at"] = str(exe), now
    try:
        QODER_IDENT_CACHE.parent.mkdir(parents=True, exist_ok=True)
        QODER_IDENT_CACHE.write_text(json.dumps(j), encoding="utf-8")
    except OSError:
        pass
    return j


def _qoder_cosy_headers(store, token, ident=None):
    """Qoder 服务端按 Cosy-* 机器身份决定给不给活动。实测（2026-10-13 / 2026-10-18）：
    同一个 uid、同一个令牌，裸 Bearer 打 /sash/api/v1/me/campaigns 拿到的是
    `{"showCampaign":false,"campaigns":[]}`；带上 ClientType/MachineId/Version 变成
    `showCampaign:true` 但只列 BOGO 推广；**再带上设备令牌**才会列出每日领取那条。
    所以"Qoder 今天没活动"多半是我们看不见，不是事实。
    machineId 用桌面端自己的 auth.machine-id，版本号读 .qoder-app-status.json。"""
    root = Path(os.environ.get("APPDATA", "")) / QODER_NEW_SHELL.get(store, store)
    ver = ""
    for p in (Path.home() / (".qoder-cn" if store == "qoder-cn" else ".qoder") / ".qoder-app-status.json",):
        try:
            ver = str(json.loads(p.read_text(encoding="utf-8")).get("version") or "")
        except Exception:       # noqa: BLE001 读不到就不带版本号，让服务端自己判
            pass
    mid = ""
    try:
        mid = (root / "auth.machine-id").read_text(encoding="utf-8").strip()
    except Exception:           # noqa: BLE001
        pass
    h = {"Accept": "application/json", "Content-Type": "application/json",
         "Authorization": f"Bearer {token}", "User-Agent": "Qoder",
         "Cosy-ClientType": "10", "Cosy-MachineOS": f"win32_{platform.system()}",
         "Cosy-MachineHostname": platform.node()}
    if ver:
        h["Cosy-Version"] = ver
    if mid:
        h["Cosy-MachineId"] = mid
    if ident:
        # 客户端日志里这三个头的顺序：Token / Code / Type；缺一个服务端就当下发对象不合格
        for hk, jk in (("Cosy-MachineToken", "machineToken"),
                       ("Cosy-MachineCode", "machineCode"),
                       ("Cosy-MachineType", "machineType")):
            if ident.get(jk):
                h[hk] = str(ident[jk])
    return h


def _qoder_campaigns(base, headers, claim=False, campaign_id=""):
    """读 /sash/api/v1/me/campaigns；claim 时打 /sash/api/v1/me/campaigns/<id>/claim。
    领取那条实测回 503 SAME_PERSON_DEPENDENCY_UNAVAILABLE——服务端要设备风控令牌
    （Cosy-MachineToken，由安装目录 resources/umid/runtime-info.exe + sgsdk.dll 现算），
    我们这套头里没有它。"""
    if claim:
        d, e = _ci_req(f"{base}/sash/api/v1/me/campaigns/{campaign_id}/claim", headers, {},
                       tag="qoder claim")
        if d is None:
            return None, e
        st = str(_ci_pick(_ci_body(d), "claimStatus", "status") or "")
        got = st in ("CLAIMED",)
        return {"claimed": got, "already": False if got else st in ("CLAIMED",),
                "message": st or "服务端没回领取结果", "credit": _ci_pick(_ci_body(d), "amount")}, None
    d, e = _ci_req(base + "/sash/api/v1/me/campaigns", headers, tag="qoder campaigns")
    if d is None:
        return None, e
    cs = [c for c in (d.get("campaigns") or []) if isinstance(c, dict)]
    benefit = next((c for c in cs
                    if c.get("actionType") == "CLAIM_BENEFIT"
                    and str(c.get("claimStatus")) == "CLAIMABLE"), None)
    claimed = next((c for c in cs
                    if c.get("actionType") == "CLAIM_BENEFIT"
                    and str(c.get("claimStatus")) == "CLAIMED"), None)
    # 四种状态，**空列表绝不能报成"没活动"**：
    #   今天这份还没领 / 领过了 / 服务端确实没挂（showCampaign=false 且一条都没有）/ 这次没读到。
    # 2026-10-17 她截图：客户端弹窗明明白白写着「每天领 100 Credits」，驾驶舱却说
    # "服务端今天没挂可领的活动"——那条 Cosy-* 机器身份头是**会失配**的，
    # 失配时服务端不报错、只回一个空列表（早先"国际版没活动"就是这么误判出来的）。
    # 所以：列表空但 showCampaign 还是 true → 只能是"这次没读到"，不许下结论。
    if benefit:
        state = "claimable"
    elif claimed:
        state = "claimed"
    elif not cs and bool(d.get("showCampaign")):
        state = "unreadable"
    else:
        state = "none"
    src = benefit or claimed or {}
    b = src.get("benefit") or {}
    return {
        "show": bool(d.get("showCampaign")), "claimable": bool(d.get("claimable")),
        "state": state,
        "campaign": str(src.get("campaignKey") or ""),
        "campaign_id": str(src.get("campaignId") or ""),
        "reward": b.get("amount"), "kind": b.get("kind"),
        "valid_days": (b.get("validity") or {}).get("days"),
        "ends_at": src.get("endAt"), "all": len(cs),
    }, None


def ci_qoder(store, claim=False, profile=None):
    """Qoder 系的每日领积分，三条路都实测过：
      新：GET /sash/api/v1/me/campaigns —— 桌面端那个「专属活动权益 · 每天领 100 Credits」
          弹窗走的就是这条。要看到它得同时满足两件事（2026-10-13 第一次发现，2026-10-18 补齐）：
            ① 带 Cosy-* 那套客户端身份头（裸 Bearer 只拿到空列表）；
            ② **带设备令牌** Cosy-MachineToken/Code/Type —— 只带身份头时服务端只回
               BOGO 那条推广（VIEW_DETAILS），把每日领取那条（CLAIM_BENEFIT）藏起来。
               这正是"登录着却说今天没活动"的真身：不是没有，是不给没令牌的调用方看。
          令牌由客户端自带的 resources/umid/runtime-info.exe + sgsdk.dll 现算（设备风控），
          我们照着客户端的调法跑它（2026-10-18 实测：拿它算出的头打同一条接口，
          CLAIM_BENEFIT 100 积分立刻出现，POST .../claim 回 status=CLAIMED、grantId、30 天有效）。
      旧：CN 另有一条 /sash/api/v1/me/daily-check-in/*，本号 status=DISABLED
          （活动键 cn_daily_check_in_legacy），留着当兜底读数。"""
    name = QODER_STORES.get(store, store)
    login = None
    for d in _qoder_candidate_dirs(name, profile):
        info, _err = _qoder_read_login(d)
        if info:
            login = info
            break
    if login is None:
        return {"error": f"没找到 {name} 的登录令牌"}
    token = login.get("token") or ""
    base = "https://openapi.qoder.sh" if store == "qoder" else "https://openapi.qoder.com.cn"
    ident = _qoder_machine_identity(store)
    headers = _qoder_cosy_headers(store, token, ident)
    camp, cerr = _qoder_campaigns(base, headers)
    # 有令牌却整条读失败：可能是令牌过期（它带时效），换一份重来一次；再不行退回不带令牌，
    # 至少把"能读到的部分"给出来，而不是直接报读不到。
    if camp is None and ident:
        ident2 = _qoder_machine_identity(store, fresh=True)
        if ident2:
            camp, cerr = _qoder_campaigns(base, _qoder_cosy_headers(store, token, ident2))
            if camp is not None:
                ident = ident2
        if camp is None:
            camp, cerr = _qoder_campaigns(base, _qoder_cosy_headers(store, token))
            ident = None
    # 空列表重试一次：Cosy-* 那套机器身份头偶尔会失配（同一进程里第一次就空、第二次正常），
    # 一次没读到就报"没有活动"是假话（2026-10-17 她截图抓到的正是这个）。
    if camp is not None and camp.get("state") == "unreadable":
        time.sleep(0.6)
        camp2, _e2 = _qoder_campaigns(base, headers)
        if camp2 is not None and camp2.get("state") != "unreadable":
            camp = camp2
    if claim and camp.get("state") == "claimable":
        res, e2 = _qoder_campaigns(base, headers, claim=True,
                                   campaign_id=camp.get("campaign_id") or "")
        if res is None:
            return {"error": e2}
        res.setdefault("credit", camp.get("reward"))
        return res
    if camp is None:
        return {"error": cerr or "读不到活动"}
    if claim and camp.get("state") != "claimable":
        if camp.get("state") == "claimed":
            msg = "今天这份已经领过了"
        elif store == "qoder" and not ident:
            msg = "没拿到客户端设备令牌，服务端不会把每日领取那条列出来（点 ↻ 重试）"
        else:
            msg = "服务端今天没挂可领的活动"
        return {"claimed": False, "already": camp.get("state") == "claimed", "message": msg}
    if camp.get("state") != "none":
        ea = camp.get("ends_at")
        # 两家现在都领得动：CN 只要 Cosy-* 那套身份头；国际版再加设备令牌（见上面那段）。
        # 没拿到令牌时国际版的 POST 会回 503 SAME_PERSON_DEPENDENCY_UNAVAILABLE，
        # 那种情况下 auto 只能是 false，界面写清"要在客户端里点"。
        autoable = store != "qoder" or bool(ident)
        return {
            "supported": True, "auto": autoable,
            "account": (login.get("who") or {}).get("name") or "",
            "activity_on": True,
            "signed_today": camp["state"] == "claimed",
            "claimable": camp["state"] == "claimable" and camp["claimable"],
            "reward": camp.get("reward"), "streak": None,
            "campaign": camp.get("campaign") or "", "campaign_id": camp.get("campaign_id") or "",
            "ends_on": (dt.datetime.fromtimestamp(ea).strftime("%Y-%m-%d %H:%M")
                        if isinstance(ea, (int, float)) and ea > 10 ** 8 else ""),
            "valid_days": camp.get("valid_days"),
            "unit": "积分",
            "device_token": bool(ident),
            "note": ("没拿到设备令牌（runtime-info.exe 没跑起来），领取口会被挡；"
                     "到点在客户端里点一下「领取」" if not autoable
                     else "带客户端设备令牌，领取口能打通" if store == "qoder"
                     else "领取口能打通"),
            "source": base + "/sash/api/v1/me/campaigns",
        }
    if store != "qoder-cn":
        # 国际版：campaigns 里没有 CLAIM_BENEFIT 条目。先分清是"真没活动"还是"这次没读到"，
        # 别把"没读到"写成"没有"——她明明登录着、客户端弹窗也弹着（2026-10-17）。
        # 2026-10-18 补：**没拿到设备令牌时也属于"没读到"**，因为服务端会把每日领取那条
        # 对没有令牌的调用方藏起来，这时候回一句"今天确实没挂活动"是在替服务端撒谎。
        unreadable = camp.get("state") == "unreadable" or (store == "qoder" and not ident)
        why = ("这次没读到活动列表（服务端回了空列表，机器身份头可能失配）"
               if camp.get("state") == "unreadable"
               else "没拿到客户端设备令牌（runtime-info.exe 没跑起来），服务端不会把每日领取那条列给我们")
        return {"supported": True, "auto": False, "activity_on": bool(camp["show"]),
                "signed_today": None, "claimable": False,
                "unreadable": bool(unreadable), "need_login": False,
                "device_token": bool(ident),
                "account": (login.get("who") or {}).get("name") or "",
                "unit": "积分",
                "note": (why + "——点这一行的 ↻ 重试，或者到客户端里点「领取」")
                        if unreadable else "服务端今天确实没挂可领的活动",
                "source": base + "/sash/api/v1/me/campaigns"}
    path = "/sash/api/v1/me/daily-check-in/" + ("claim" if claim else "status")
    d, e = _ci_req(base + path, headers, {} if claim else None, tag="qoder legacy")
    if d is None:
        return {"error": e}
    b = _ci_body(d)
    st = str(_ci_pick(b, "status") or "")
    if claim:
        return {"claimed": st in ("CLAIMED",), "already": st in ("ALREADY_CLAIMED", "DISABLED"),
                "message": st or "没领成", "credit": _ci_pick(b, "rewardCredits", "reward_credits")}
    return {
        "supported": True, "auto": st != "DISABLED",
        "account": (b.get("user") or {}).get("name") or "",
        "activity_on": st != "DISABLED",
        "signed_today": st == "CLAIMED_TODAY",
        "claimable": st == "CLAIMABLE",
        "streak": _ci_pick(b, "currentStreakDays", "current_streak_days"),
        "total": _ci_pick(b, "totalClaimDays", "total_claim_days"),
        "reward": _ci_pick(b, "rewardCredits", "reward_credits"),
        "campaign": str(_ci_pick(b, "campaignKey", "campaign_key") or ""),
        "unit": "积分", "source": base + path,
        "note": "这条是旧版签到接口，活动键 " + str(_ci_pick(b, "campaignKey", "campaign_key") or "")
                + "；新活动在 campaigns 那条",
    }


def ci_astudio(claim=False):
    """讯飞 AStudio 的「限时每日福利 · 今日可领 100 积分」。实测（2026-10-13）：
      GET  {base}/client-popups/pending   -> data[] 里 componentType=DAILY_REWARD_DIALOG 那条就是今天这份，
                                             instanceKey 直接是日期串（20261013），服务端认当日账
      POST {base}/client-popups/complete  -> body {popupId, instanceKey}，界面上那个「领取」就是发这条
    鉴权是 cookie（明文落在 AStudio Data/userdata/astron-session.json），不需要 Bearer 也不需要签名。
    ⚠ 别拿渲染层看到的 /api/astron-client-popups/* 直连：那是打到产品自带本地内核的相对路径，
      本地内核再转成上面这条 xingchen-studio 绝对地址；直连 agent.xfyun.cn 只会拿回前端 HTML，
      直连 astronstudio-api-volces-prod 会被网关要 HMAC。之前"服务端不给今天领没领、只能手动"
      这个结论就是这么来的，是错的。
    到账与否不看文案（pending 里"今日可领"和"已到账"是同一条运营位），只看 points/details
    今天有没有一笔 reason 含「每日登录」的 CREDIT——那才是钱到手了。"""
    sess = None
    for d in _astudio_candidates():
        info, _e = _astudio_read_session(d)
        if info:
            sess = info
            break
    if sess is None:
        return {"error": "没找到 AStudio 的登录档案（AStudio Data/userdata/astron-session.json）"}
    headers = {"Accept": "application/json", "Content-Type": "application/json",
               "Cookie": _astudio_cookie(sess), "clientType": "21", "studioVersion": "3.5.0",
               "Origin": "https://agent.xfyun.cn", "Referer": "https://agent.xfyun.cn/",
               "User-Agent": CHECKIN_UA}
    d, e = _ci_req(ASTUDIO_POPUP_URL + "/pending", headers, tag="astudio pending")
    if d is None:
        return {"error": e}
    if d.get("flag") is not True or d.get("code") != 0:
        return {"error": f"服务端返回异常：{str(d)[:140]}"}
    pop = next((x for x in (d.get("data") or [])
                if x.get("componentType") == "DAILY_REWARD_DIALOG"
                or "DAILY_LOGIN" in str(x.get("popupCode") or "")), None)
    got, gerr = _astudio_daily_granted(headers)
    if pop is None:
        if got is not None:
            return {"supported": True, "auto": True, "signed_today": True, "claimable": False,
                    "activity_on": True, "account": sess.get("nickname") or "",
                    "reward": got.get("amount"), "granted_at": str(got.get("createTime") or ""),
                    "unit": "积分", "words": {"done": "今天已到账"},
                    "note": "今天这份已到账，服务端没再挂这条运营位",
                    "source": ASTUDIO_POPUP_URL + "/pending"}
        # No pending popup and no today's credit: opening the client remains
        # necessary until a headless login trigger has been verified.
        if gerr:
            return {"error": "AStudio 积分流水读取失败：" + str(gerr)}
        return {"supported": True, "auto": False, "signed_today": False, "claimable": False,
                "activity_on": True, "need_login": True,
                "account": sess.get("nickname") or "", "reward": 100, "unit": "积分",
                "words": {"done": "今天已到账", "go": "今天待确认", "act": "确认领取",
                          "wait": "今天还没登录"},
                "note": "今日积分未到账；请打开 AStudio 登录，到账后可在这里确认。",
                "source": ASTUDIO_POPUP_URL + "/pending"}
    payload = pop.get("payload") or {}
    if claim:
        return _astudio_claim(headers, pop)
    # 两个事实分开判，合成一个必然说谎：
    #   got      —— 今天这 100 到没到账（登录就自动发，实测 12:11:09 那笔）
    #   pop 在不在 —— 界面上「限时每日福利 · 今日可领」还挂着没有
    # 只按 got 报"已签"，就会出现"客户端明明写着今日可领、我们说已签"（2026-10-13 被她撞穿）。
    # 实测点 complete 不会多出一笔流水——它只是把弹窗确认掉，所以弹窗还挂着就是还有事可做。
    # 弹窗还挂着 = 还有事可做，这一律算"可领"，哪怕积分其实已经到账——
    # 到账与确认是两件事，只按到账报"已签"就会出现"客户端写着今日可领、我们说已签"
    # （2026-10-13 被她撞穿的就是这个）。实测点 complete 不多出一笔流水，它只是确认掉弹窗。
    return {
        "supported": True, "auto": True,
        "account": sess.get("nickname") or "",
        "activity_on": True,
        "signed_today": False,
        "claimable": True,
        "reward": (got.get("amount") if got else payload.get("points")) or 100,
        "granted_at": str((got or {}).get("createTime") or ""),
        "title": str(payload.get("title") or ""),
        "popup_id": pop.get("popupId"), "instance_key": str(pop.get("instanceKey") or ""),
        "unit": "积分",
        "words": {"done": "今天已到账", "go": "今天待确认", "act": "确认领取"},
        "note": ("每日 100 积分登录就自动到账了，这个「领取」只是把弹窗确认掉"
                 if got else "今天这份还没到账，点领取"),
        "source": ASTUDIO_POPUP_URL + "/pending",
    }


def _astudio_claim(headers, pop):
    d, e = _ci_req(ASTUDIO_POPUP_URL + "/complete", headers,
                   {"popupId": pop.get("popupId"), "instanceKey": pop.get("instanceKey")},
                   tag="astudio complete")
    if d is None:
        return {"error": e}
    code = d.get("code")
    msg = str(d.get("desc") or "")
    # 11003002 是"这条已经完成了"——重复点领取撞的就是它，不能当错误重试
    already = code == 11003002 or "已" in msg
    ok = d.get("flag") is True and code == 0
    return {"claimed": bool(ok), "already": bool(already), "message": msg or ("已提交领取" if ok else "没领成"),
            "code": code, "credit": (pop.get("payload") or {}).get("points")}


def _astudio_daily_granted(headers):
    """今天有没有一笔「每日登录赠送」的 CREDIT 流水。返回 (那一笔或 None, err)。"""
    url = (ASTUDIO_DETAILS_URL + "?" +
           urlencode({"direction": "CREDIT", "pageNum": 1, "pageSize": 20}))
    d, e = _ci_req(url, headers, tag="astudio details")
    if d is None:
        return None, e
    if d.get("flag") is not True or d.get("code") != 0:
        return None, f"积分流水读不到：{str(d)[:120]}"
    today = dt.date.today().isoformat()
    for it in ((d.get("data") or {}).get("items") or []):
        if str(it.get("createTime") or "")[:10] == today and \
                "每日登录" in str(it.get("reason") or ""):
            return it, None
    return None, None


# 驾驶舱 store 名 → 签到实现。没列进来的（ZCode 领取强绑阿里云验证码、
# 千问办公网关未路由、DSH/Marvis/MiMo 全量 grep 无任何签到端点）一律不做自动，
# 界面上也不会出现那几家。
ASTUDIO_POPUP_URL = "https://agent.xfyun.cn/xingchen-studio/client-popups"

CHECKIN_STORES = {
    "workbuddy": lambda claim, **kw: ci_workbuddy(intl=False, claim=claim, **kw),
    "workbuddy-activity": ci_workbuddy_activity,
    "workbuddy-travel": ci_workbuddy_travel,
    "workbuddy-intl": lambda claim, **kw: ci_workbuddy(intl=True, claim=claim, **kw),
    "trae-cn": lambda claim, **kw: ci_trae(claim=claim),
    "minimax": lambda claim, **kw: ci_minimax(claim=claim),
    "astudio": lambda claim, **kw: ci_astudio(claim=claim),
    "qoder-cn": lambda claim, **kw: ci_qoder("qoder-cn", claim=claim, **kw),
    "qoder": lambda claim, **kw: ci_qoder("qoder", claim=claim, **kw),
    # 每天 300；领取＝带桌面头打一条 office/v3/setting_info（客户端启动时打的就是它）
    "raccoon": lambda claim, **kw: ci_raccoon(claim=claim, **kw),
    # 每天 200（周一翻倍）；领取＝POST /alice/ug/campaign/landing/checkin，服务端幂等
    "doubao": lambda claim, **kw: ci_doubao(claim=claim, **kw),
}


# 产品显示名放在这一处：Rust 侧的提醒文案和界面的行标题都读它，别再各存一份
CHECKIN_LABELS = {
    "workbuddy": "WorkBuddy", "workbuddy-intl": "WorkBuddy 国际版",
    "workbuddy-activity": "WorkBuddy 活跃地图", "workbuddy-travel": "WorkBuddy 旅行猫",
    "trae-cn": "Trae CN", "minimax": "MiniMax Code", "astudio": "AStudio",
    "qoder-cn": "Qoder CN", "qoder": "Qoder", "raccoon": "商汤小浣熊",
    "doubao": "豆包",
}


def _ci_confirm(fn, r):
    """领完回读一次状态再交卷。厂商的领取响应形状各家不同、失败也常常照样回 200，
    「今天这份到手了没有」只有状态接口说了算。宁可报没领成也不报假成功——
    假成功会让驾驶舱把这家记成今天已跑过，当天再也不重试
    （2026-10-13 MiniMax 就是缺 timezone_id 被服务端拒了，却连着记了四次"领到"）。"""
    if r.get("error") or r.get("settling") or r.get("verified"):
        # settling = 这家的到账是服务端按天批量结算的，刚发完读不到不等于失败，
        # 拿状态回读去否定它反而会报出"没领成"的假话
        return r
    for i in range(2):
        if i:
            time.sleep(1.2)          # 服务端可能慢一拍，隔一下再确认一次
        st = fn(False) or {}
        if st.get("error"):
            r["verify_error"] = st["error"]
            return r
        if st.get("signed_today") is True:
            r["verified"] = True
            if not r.get("claimed"):
                r["already"] = True
            return r
    r["verified"] = False
    if r.get("claimed"):
        r["claimed"] = False
        r["error"] = "服务端回了「{}」，但状态里今天这份还没到手".format(str(r.get("message") or "无"))
    return r


def cmd_checkin(con, args):
    out = []
    if args.claim and not args.product:
        # 故意不让一条命令把所有号都签一遍：领错号没法撤销，必须一家一家点名
        print(f"领取要指名产品（可领的：{'、'.join(CHECKIN_STORES)}）；不带 --claim 时只查状态")
        return
    # 按账号查：驾驶舱把那个号存过的登录态铺到一个临时目录/文件，再把路径指过来。
    # 两个都省略＝照旧查"这台机器现在登着的那个号"。
    kw = {"profile": getattr(args, "profile", None), "auth_file": getattr(args, "auth_file", None)}
    kw = {k: v for k, v in kw.items() if v}
    want = [args.product] if args.product else list(CHECKIN_STORES)
    for p in want:
        fn = CHECKIN_STORES.get(p)
        if not fn:
            out.append({"product": p, "error": f"这家没有签到实现（已知：{'、'.join(CHECKIN_STORES)}）"})
            continue
        del CI_RAW[:]
        try:
            r = fn(bool(args.claim), **kw)
        except Exception as e:      # noqa: BLE001
            r = {"error": f"{type(e).__name__}: {e}"}
        if args.claim:
            try:
                r = _ci_confirm(lambda c, _f=fn, _k=kw: _f(c, **_k), r)
            except Exception as e:  # noqa: BLE001
                r["verify_error"] = f"{type(e).__name__}: {e}"
        if args.raw:
            r["_raw"] = list(CI_RAW)
        r["product"] = p
        r["name"] = CHECKIN_LABELS.get(p, p)
        # 本机日期与小时：自动签到要按"当天跑过没有"和"在不在时间窗里"判断，
        # 这两件事 Rust 侧算要自己取时区，交给已经知道本地时区的 Python 报出来
        r["today"] = dt.date.today().isoformat()
        r["hour"] = dt.datetime.now().hour
        # 提醒要按"到没到设定的时刻"判，光有小时不够（10:05 和 10:55 得能分开）
        r["minute"] = dt.datetime.now().minute
        out.append(r)
    if args.json:
        print(json.dumps(out, ensure_ascii=False, indent=2))
        return
    for r in out:
        if r.get("error"):
            print(f"  {r['product']:<16}读不到：{r['error']}")
            continue
        if args.claim:
            print(f"  {r['product']:<16}{'已领到' if r.get('claimed') else '没领成'}"
                  f" · {r.get('message')}"
                  + (f" · +{r.get('credit')}" if r.get("credit") else ""))
            continue
        flag = ("今天已签" if r.get("signed_today") else
                "今天可领" if r.get("claimable") else
                "活动未开" if r.get("activity_on") is False else "查不到状态")
        print(f"  {r['product']:<16}{flag} · 连签 {r.get('streak') if r.get('streak') is not None else '?'} 天"
              f" · 每日 {r.get('reward') if r.get('reward') is not None else '?'} {r.get('unit') or '积分'}"
              f" · {'可自动' if r.get('auto') else '只能手动'}")
        if r.get("note"):
            print(f"        {r['note']}")


def cmd_quota(con, args):
    known = list(QODER_STORES) + list(LOCAL_USAGE_STORES) + list(SERVER_CREDIT_STORES)
    want = [args.product] if args.product else known
    rows, dump = [], []
    for p in want:
        # 只统计"已用"、读不到余额的店：输出 schema 与 Qoder 同形，但 left/total 为 null
        if p in LOCAL_USAGE_STORES:
            prof = getattr(args, "profile", None)
            r = workbuddy_credit(prof)
            if "error" in r:
                rows.append((p, None, r["error"]))
                dump.append({"product": p, "error": r["error"]})
                continue
            d = r["data"]
            scope = usage_scope(prof, d)
            rows.append((p, ("usage", d), None))
            dump.append({"product": p, "kind": "usage_only", "account": scope,
                         "userId": "", "userType": "usage_only", "left": None, "total": None,
                         "expires": "", "exceeded": False, "db": d["db"],
                         "used_credits": d["used_credits"], "sessions": d["sessions"],
                         "last_active": d["last_active"],
                         "buckets": [{"pool": "累计已用", "total": None,
                                      "used": d["used_credits"], "remaining": None,
                                      "unit": "credits"}]})
            continue
        fn = SERVER_CREDIT_STORES.get(p)
        store = QODER_STORES.get(p)
        if not fn and not store:
            print(f"不认识的产品 {p}（支持：{'、'.join(known)}）")
            continue
        if fn:
            r = fn(profile=getattr(args, "profile", None),
                   live=not getattr(args, "local", False))
        else:
            r = qoder_credit(store, profile=getattr(args, "profile", None),
                             live=not getattr(args, "local", False))
        if "error" in r:
            rows.append((p, None, r["error"]))
            dump.append({"product": p, "error": r["error"]})
            continue
        d = r["data"]
        who = r.get("who") or {}
        acct = who.get("name") or (who.get("email") or "")[:24] or str(d.get("userId"))[:8] + "…"
        if who.get("email") and who.get("name"):
            acct += f"〈{who['email']}〉"
        buckets = credit_buckets(d)
        # 一笔额度都没有时输出 null 而不是 0：把"这产品读不到额度"显示成"剩 0 积分"是撒谎
        # （Xiaomi MiMo 只有登录态可读，见 xiaomi_mimo_credit 那段）
        left = round(sum(b["remaining"] or 0 for b in buckets), 8) if buckets else None
        total = round(sum(b["total"] or 0 for b in buckets), 8) if buckets else None
        live_err = r.get("live_error", "")
        # 登录态实际是从哪个档案读到的：换了档案位置（0.4.x 新壳）时，这是唯一的线索
        auth_from = r.get("auth_from", "")
        rows.append((p, (d, buckets, left, total, acct, live_err, auth_from), None))
        entry = {"product": p, "account": acct, "userId": d.get("userId"),
                 "userType": d.get("userType"), "left": left, "total": total,
                 "expires": exp_text(d.get("expiresAt")), "exceeded": d.get("isQuotaExceeded"),
                 "source": r.get("source", "local-cache"), "liveError": live_err,
                 "authFrom": auth_from, "authKind": r.get("auth_kind", ""),
                 "plan": d.get("subscriptionPackageName") or "",
                 "buckets": buckets, "groups": credit_groups(buckets)}
        # 按天消耗序列（驾驶舱的波形图/热力图用）。读不出来只记一句原因，
        # 不许把余额本身打挂——她那张卡还得显示剩多少积分。
        t0 = daily_trend(p, profile=getattr(args, "profile", None))
        if t0:
            if "data" in t0:
                entry["trend"] = t0["data"]
            else:
                entry["trendError"] = t0.get("error", "")
        dump.append(entry)
    if args.json:
        print(json.dumps(dump, ensure_ascii=False, indent=2))
        return
    for p, ok, err in rows:
        if err:
            print(f"  {p:<12}读不到：{err}")
            continue
        if ok[0] == "usage":
            d = ok[1]
            print(f"  {p:<12}累计已用 {d['used_credits']} credits · {d['sessions']} 个会话 · "
                  f"最近 {d['last_active'] or '?'} · 库 {d['db']} · 余额需官网接口，本地读不到")
            continue
        d, buckets, left, total, acct, live_err, auth_from = ok
        # 只出账号的产品（小浣熊 / Coze / MiMo / Antigravity）：没有余额就直说，
        # 别印成「剩 None/None 积分」再补一句"下面是旧缓存"——那是拿空数字糊人。
        if left is None and total is None and not buckets:
            print(f"  {p:<12}只读到账号 {acct}（{d.get('userType') or '?'}）· "
                  "厂商没有余额接口，额度去官网看")
            if auth_from:
                print(f"      登录档案 {auth_from}")
            continue
        flag = "已超额" if d.get("isQuotaExceeded") else f"用了 {d.get('totalUsagePercentage', 0):.0%}"
        src = "服务端实时"
        if live_err:
            src = "本地缓存(旧)"
            if "TOKEN_EXPIRE" in live_err or "401" in live_err:
                src = "本地缓存(旧·登录令牌已过期)"
        # 单位跟着额度桶走：Qoder/WorkBuddy 是 credits、AStudio 是积分、ZCode 是 token——
        # 一律印"积分"会把 ZCode 的 3 亿 token 说成 3 亿积分，量级完全不是一回事。
        unit = (buckets[0]["unit"] if buckets else "") or "积分"
        print(f"  {p:<12}剩 {left}/{total} {unit} · 账号 {acct} · {d.get('userType') or '?'} · "
              f"到期 {exp_text(d.get('expiresAt'))} · {flag} · 来源 {src}")
        if live_err:
            print(f"      ⚠ 服务端没读成（{live_err}）——上面的数字是客户端上次联网时的缓存，可能已经过期。")
            if "TOKEN_EXPIRE" in live_err:
                print("        打开一次该产品让它自动续期，或重新登录后再读。")
        if auth_from:
            print(f"      登录档案 {auth_from}")
        for b in buckets:
            print(f"      {b['pool']:<26}{b['used']}/{b['total']} → 剩 {b['remaining']} {b['unit']}"
                  f" · 到期 {b.get('expires') or exp_text(d.get('expiresAt'))}")
            if b.get("note"):
                print(f"        {b['note']}")
        tr = next((e.get("trend") for e in dump if e.get("product") == p), None)
        if tr:
            # 来源各家不同：WorkBuddy 是本地库（报文件路径），另外几家是接口（报接口名）
            where = tr.get("db") or tr.get("source") or ""
            # "有数"按真不为 0 的那些天算：MiniMax 那份是官方补零的连续数组，
            # 拿 len(days) 说成"168 天有数"就是谎报
            used = sum(1 for x in tr["days"] if (x.get("credits") or 0) > 0)
            print(f"      按天消耗：{used} 天有消耗 · 合计 {tr['total']} {tr['unit']} · "
                  f"峰值 {tr['peak_date']} {tr['peak']}" + (f" · {where}" if where else ""))
            print("        归日口径见界面那行小字（WorkBuddy 按会话最后活动时间，别家按接口给的日桶）")
    if args.write:
        for p, ok, _ in rows:
            if not ok:
                continue
            if ok[0] == "usage":
                d = ok[1]
                line = (f"{p} 累计已用 {d['used_credits']} credits（{d['sessions']} 个会话，"
                        f"最近 {d['last_active'] or '?'}），档案 {d['db']}")
                insert_note(con, "agenthub", "fact", f"{p} 用量快照", "quota,agenthub", line)
                print(f"  已写入共享记忆：{line[:60]}…")
                continue
            d, buckets, left, total, acct, live_err, _auth_from = ok
            line = (f"{p} 余额（账号 {acct}）：剩 {left}/{total} 积分，到期 {exp_text(d.get('expiresAt'))}；"
                    + "；".join(f"{b['pool']} 剩 {b['remaining']}/{b['total']}" for b in buckets)
                    + (f"［来源：本地缓存，服务端读不到：{live_err}］" if live_err else ""))
            insert_note(con, "agenthub", "fact", f"{p} 余额快照", "quota,agenthub", line)
            print(f"  已写入共享记忆：{line[:60]}…")


def cmd_role(con, args):
    roles = role_rows()
    act = args.role_act
    if act == "list":
        if not roles:
            print("（没有定义任何角色）")
            return
        for name, r in roles.items():
            marks = "  ".join(f"{a}{'✓' if agent_available(a) else '✗'}"
                              for a in r.get("agents", [])) or "（空）"
            print(f"  {name:<12}{r.get('note') or ''}")
            print(f"              候选: {marks}")
        print(f"\n配置文件：{ROLES_FILE}（改完即生效）")
        print("指派：agenthub role assign <角色> <产品>；移除：agenthub role remove <角色> <产品>")
        return
    role, agent = args.role, args.agent
    if agent not in AGENTS and agent not in BRIDGE_PRODUCTS:
        raise SystemExit(f"不认识的 Agent：{agent}"
                         f"（CLI：{'、'.join(AGENTS)}；桥接：{'、'.join(BRIDGE_PRODUCTS)}）")
    if act == "assign":
        r = roles.setdefault(role, {"agents": [], "note": ""})
        if agent in r["agents"]:
            print(f"{role} 里已经有 {agent} 了")
            return
        r["agents"].append(agent)
        save_role_rows(roles)
        print(f"已把 {agent} 指派给角色 {role} → 现候选：{'、'.join(r['agents'])}")
    elif act == "remove":
        r = roles.get(role)
        if not r or agent not in r.get("agents", []):
            print(f"角色 {role} 里没有 {agent}")
            return
        r["agents"].remove(agent)
        save_role_rows(roles)
        print(f"已把 {agent} 移出角色 {role} → 现候选：{'、'.join(r['agents']) or '（空）'}")


def _bridge_task_expired(t):
    lu = str(t.get("lease_until") or "")
    if not lu:
        return False
    try:
        return dt.datetime.fromisoformat(lu) < dt.datetime.now()
    except ValueError:
        return False


def cmd_bridge(con, args):
    act = args.bridge_act
    if act == "setup":
        p = args.product
        if p not in BRIDGE_PRODUCTS:
            raise SystemExit(f"不认识的桥接产品：{p}（支持：{'、'.join(BRIDGE_PRODUCTS)}）")
        bridge_write_readme(p)
        bdir = BRIDGE_DIR / p
        (bdir / "inbox").mkdir(parents=True, exist_ok=True)
        (bdir / "outbox").mkdir(parents=True, exist_ok=True)
        print(f"已生成 {bdir / 'README.md'}")
        print(f"  收件箱 {bdir / 'inbox'}")
        print(f"  回执箱 {bdir / 'outbox'}")
        print("下一步：在产品里建自动化/技能扫 inbox（说明见 README.md）")
        return
    if act == "heartbeat":
        p = str(args.product or "")
        if p not in BRIDGE_PRODUCTS:
            raise SystemExit(f"不认识的桥接产品：{p}")
        jpath = BRIDGE_DIR / p / "inbox" / f"task_{args.id}.json"
        if not jpath.exists():
            raise SystemExit(f"找不到任务：{jpath}")
        t = json.loads(jpath.read_text(encoding="utf-8"))
        lease = (dt.datetime.now() + dt.timedelta(
            seconds=int(getattr(args, "seconds", 300) or 300))).isoformat(timespec="seconds")
        t["heartbeat_at"], t["lease_until"] = now_iso(), lease
        jpath.write_text(json.dumps(t, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(f"已续租 {p}/{args.id} → {lease}")
        return
    if act == "reap":
        products = [args.product] if getattr(args, "product", None) else list(BRIDGE_PRODUCTS)
        n = 0
        for p in products:
            if p not in BRIDGE_PRODUCTS:
                continue
            bdir = BRIDGE_DIR / p
            inbox, outbox = bdir / "inbox", bdir / "outbox"
            if not inbox.exists():
                continue
            outbox.mkdir(parents=True, exist_ok=True)
            for j in sorted(inbox.glob("task_*.json")):
                try:
                    t = json.loads(j.read_text(encoding="utf-8"))
                except (OSError, ValueError):
                    continue
                if str(t.get("status") or "pending") != "pending" or not _bridge_task_expired(t):
                    continue
                t["status"] = "expired"
                t["expired_at"] = now_iso()
                j.write_text(json.dumps(t, ensure_ascii=False, indent=2) + "\n",
                             encoding="utf-8")
                (outbox / f"result_{t.get('id', j.stem)}.json").write_text(
                    json.dumps({"task_id": t.get("id", ""), "status": "failed",
                                "summary": "租约过期，任务被回收",
                                "blockers": ["product did not heartbeat before lease_until"]},
                               ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
                n += 1
        print(f"回收 {n} 个过期桥接任务" if n else "没有过期任务")
        return
    shown = False
    for p in BRIDGE_PRODUCTS:
        bdir = BRIDGE_DIR / p
        inbox = sorted((bdir / "inbox").glob("task_*.json")) if (bdir / "inbox").exists() else []
        outbox = sorted((bdir / "outbox").glob("result_*.json")) if (bdir / "outbox").exists() else []
        if not inbox and not outbox:
            continue
        shown = True
        print(f"  {p}（{BRIDGE_PRODUCTS[p]['name']}）")
        for j in inbox[-5:]:
            try:
                t = json.loads(j.read_text(encoding="utf-8"))
                st, created = str(t.get("status", "?")), str(t.get("created_at", ""))
                lease = str(t.get("lease_until") or "")
                expired = st == "pending" and _bridge_task_expired(t)
            except (OSError, ValueError):
                st, created, lease, expired = "?", "", "", False
            mark = {"pending": "!", "done": "✓", "picked": "→",
                    "expired": "×"}.get(st, "?")
            tail = f"  租约 {lease}" if lease else ""
            if expired:
                tail += "  [已过期]"
            print(f"    {mark} 收件 {j.stem}  {created}  [{st}]{tail}")
        for rj in outbox[-5:]:
            tm = dt.datetime.fromtimestamp(rj.stat().st_mtime).strftime("%m-%d %H:%M")
            print(f"    ✓ 回执 {rj.stem}  {tm}")
    if not shown:
        print("（还没有桥接任务。agenthub call workbuddy \"任务\" 或 agenthub bridge setup workbuddy 开始）")


def cmd_approval(con, args):
    if args.approval_act == "list":
        rows = approval_list(con, run_id=getattr(args, "run", None),
                             limit=int(getattr(args, "n", 20) or 20))
        if not rows:
            print("（没有审批记录）")
            return
        for r in rows:
            dec = r.get("decision") or "pending"
            print(f"  {r['id']:<14} [{dec:<9}] {r['kind']:<16} "
                  f"run={r['run_id'] or '-'} step={r['step_id'] or '-'} "
                  f"{r['created_at']}")
            if r.get("request"):
                print(f"      {str(r['request']).splitlines()[0][:100]}")
        return
    if args.approval_act == "decide":
        ok = approval_decide(args.id, args.decision, by=getattr(args, "by", "user") or "user",
                             con=con)
        if not ok:
            raise SystemExit(f"审批 {args.id} 不存在或已经决策过")
        print(f"审批 {args.id} → {args.decision}")
        return
    raise SystemExit(f"不认识的 approval 子命令：{args.approval_act}")


def cmd_ticket(con, args):
    act = args.ticket_act
    if act == "new":
        t, created = ticket_create(
            "cli", args.title, requester=args.requester or "user",
            source_event_id=args.event_id or "", scope=args.scope or "",
            project=args.project or "", run_id=args.run or "",
            workspace=args.workspace or "", strength="explicit",
            matched_rules=args.rule or [], body=args.body or "",
            force=True, con=con)
        print(("已建工单 " if created else "工单已存在（幂等） ") + t["id"])
        ticket_print(t, con=con)
        return
    if act == "intake":
        text = args.text or ""
        if args.text == "-" or (not text and not sys.stdin.isatty()):
            text = sys.stdin.read()
        title = args.title or (text.strip().splitlines()[0][:80] if text.strip() else "")
        assessment = None
        if getattr(args, "assess_json", None):
            try:
                assessment = json.loads(args.assess_json)
            except ValueError as e:
                raise SystemExit(f"--assess-json 不是合法 JSON：{e}")
        r = ticket_intake(
            args.source, args.actor or "user", title, body=text,
            event_id=args.event_id or "", scope=args.scope or "",
            project=args.project or "", run_id=args.run or "",
            workspace=args.workspace or "", addressed=not args.unaddressed,
            assessment=assessment, con=con)
        if not r.get("ok"):
            print(f"未准入：{r.get('reason')}（{r.get('strength')}）")
            raise SystemExit(1)
        if r.get("would_accept"):
            print(f"[observe] 会接单但未创建：{title}（policy {r.get('policy_version')}）")
            return
        t = r["ticket"]
        print(("已建工单 " if r.get("created") else "工单已存在（幂等） ") + t["id"])
        ticket_print(t, con=con)
        return
    if act == "list":
        rows = ticket_list(args.stage, args.open, args.n, con=con)
        if args.json:
            print(json.dumps(rows, ensure_ascii=False, indent=2))
            return
        if not rows:
            print("（没有工单）")
            return
        for t in rows:
            ticket_print(t, con=con, with_details=False)
        return
    if act == "inbox":
        rows = ticket_inbox(args.n, con=con)
        if args.json:
            print(json.dumps(rows, ensure_ascii=False, indent=2))
            return
        if not rows:
            print("没有需要人的工单")
            return
        for t in rows:
            ticket_print(t, con=con, with_details=False)
        return
    if act == "show":
        ticket_print(ticket_get(args.id, con=con), con=con)
        return
    if act == "report":
        t = ticket_report(args.id, args.state, detail=args.detail or "",
                          question=args.question or "", con=con)
        ticket_print(t, con=con, with_details=False)
        return
    if act in ("answer", "note"):
        text = " ".join(args.text).strip()
        t = ticket_answer(args.id, text, kind=act, con=con)
        ticket_print(t, con=con, with_details=False)
        return
    if act == "retry":
        ticket_print(ticket_retry(args.id, con=con), con=con, with_details=False)
        return
    if act == "stop":
        ticket_print(ticket_stop(args.id, args.reason or "", con=con),
                     con=con, with_details=False)
        return
    raise SystemExit(f"不认识的 ticket 子命令：{act}")


def cmd_policy(con, args):
    p = policy_load()
    act = args.policy_act
    if act == "show":
        print(json.dumps({"version": policy_version(p), **p},
                         ensure_ascii=False, indent=2))
        return
    if act == "set-mode":
        if args.mode not in ("off", "observe", "auto"):
            raise SystemExit("mode 只能是 off / observe / auto")
        p["mode"] = args.mode
        policy_save(p)
        print(f"policy mode = {args.mode} · version {policy_version(p)}")
        return
    if act in ("allow", "disallow"):
        source = args.source
        part = p.setdefault("sources", {}).setdefault(source, {"allowed": [], "admission": "inferred"})
        allowed = part.setdefault("allowed", [])
        if act == "allow":
            if args.actor not in allowed:
                allowed.append(args.actor)
        else:
            allowed[:] = [a for a in allowed if a != args.actor]
        policy_save(p)
        print(f"{source} allowed = {allowed or '（空）'}")
        return
    if act == "rule-add":
        rules = p.setdefault("rules", [])
        found = next((r for r in rules if r.get("id") == args.id), None)
        if found:
            found["instruction"] = args.instruction
            found["enabled"] = not args.disable
        else:
            rules.append({"id": args.id, "instruction": args.instruction,
                          "enabled": not args.disable})
        policy_save(p)
        print(f"rule {args.id} · enabled={not args.disable} · version {policy_version(p)}")
        return
    if act == "rule-list":
        for r in p.get("rules") or []:
            print(f"  {r.get('id'):<24} {'on ' if r.get('enabled', True) else 'off'}  "
                  f"{r.get('instruction') or ''}")
        if not p.get("rules"):
            print("（没有规则）")
        return
    raise SystemExit(f"不认识的 policy 子命令：{act}")


def cmd_effects(con, args):
    act = args.effect_act
    if act == "list":
        rows = effect_rows(getattr(args, "kind", None), con=con)
        if not rows:
            print("（没有待执行的副作用）")
            return
        for r in rows:
            print(f"  {r['kind']:<10} {r['key'][:40]:<40} 尝试 {r['attempts']}  "
                  f"due={dt.datetime.fromtimestamp(r['due']).strftime('%H:%M:%S')}"
                  + (f"  {r['error'][:80]}" if r.get("error") else ""))
        return
    if act == "run":
        n = effect_run(getattr(args, "kind", None), int(getattr(args, "n", 20) or 20), con=con)
        print(f"已处理 {n} 条副作用")
        return
    raise SystemExit(f"不认识的 effects 子命令：{act}")


def cmd_health(con, args):
    rows = [dict(r) for r in con.execute("select * from health order by kind")]
    if not rows:
        print("（还没有健康记录；跑一次 effects 或 workflow 后就有）")
        return
    for r in rows:
        print(f"  {r['kind']:<16} {r['state']:<8} {r['detail'] or ''}")


def cmd_source(con, args):
    p = policy_load()
    if args.source_act == "list":
        for name, part in (p.get("sources") or {}).items():
            total = con.execute("select count(*) from tickets where source=?",
                                (name,)).fetchone()[0]
            open_n = con.execute("select count(*) from tickets where source=? and ended_at=''",
                                 (name,)).fetchone()[0]
            events = con.execute("select count(*) from source_events where source=?",
                                 (name,)).fetchone()[0]
            allowed = part.get("allowed") or []
            print(f"  {name:<10} allowed={('、'.join(allowed) or '（空）'):<12}"
                  f" admission={part.get('admission') or '-':<10} 工单 {open_n}/{total}"
                  f" 事件 {events}")
        print("  GitHub/Feishu 目前是策略占位；可用 source ingest 从 JSON 喂事件。")
        return
    if args.source_act == "ingest":
        raw = sys.stdin.read() if args.file == "-" else Path(args.file).read_text(
            encoding="utf-8", errors="replace")
        try:
            data = json.loads(raw)
        except ValueError as e:
            raise SystemExit(f"来源事件不是合法 JSON：{e}")
        events = data.get("events") if isinstance(data, dict) else data
        if not isinstance(events, list):
            raise SystemExit("事件文件要是数组，或 {\"events\": [...]}")
        counts = source_ingest(args.source, events, actor=args.actor or "user",
                               addressed_default=not args.unaddressed, con=con)
        print(json.dumps(counts, ensure_ascii=False, indent=2))
        return
    if args.source_act == "show":
        part = (p.get("sources") or {}).get(args.name)
        if not part:
            raise SystemExit(f"来源不存在：{args.name}")
        print(json.dumps(part, ensure_ascii=False, indent=2))
        return
    raise SystemExit(f"不认识的 source 子命令：{args.source_act}")


# ================================================== MCP 双向接入（P1）======
#
# 两个方向：
#   1. agenthub 作为 MCP Server：把记忆、派工、工作流、桥接暴露成工具，
#      任何 MCP 客户端（TRAE / Claude Desktop / Cursor / …）都能接。
#   2. agenthub 作为 MCP Client：注册并调用外部 MCP server；工作流里
#      `tool: server/工具名` 直接当 DAG 节点。
#
# 2026-07-28 规范已经改成无状态：没有 initialize 握手和 Mcp-Session-Id。
# 这里同时兼容旧客户端：initialize 仍然应答，工具请求不依赖握手。

MCP_HOME = APP_DIR / "mcp"
MCP_SERVERS_FILE = MCP_HOME / "servers.json"
MCP_SUPPORTED = ["2026-07-28", "2025-06-18", "2025-03-26", "2024-11-05"]
MCP_PROTO = MCP_SUPPORTED[0]
MCP_CALL_TIMEOUT = 900
MCP_SERVER_VERSION = "1.4.0"


def _sch(props, req=None):
    s = {"type": "object",
         "properties": {k: {"type": v[0], "description": v[1]} for k, v in props.items()},
         "additionalProperties": True}
    if req:
        s["required"] = list(req)
    return s


def _tool_agents_list(a):
    cmd_agents(None, SimpleNamespace())


def _tool_role_list(a):
    print("角色 → Agent 候选（roles.json，改了立即生效；call role:角色 按序挑第一个可用的）：")
    for name, r in role_rows().items():
        print(f"  {name:<10} {'、'.join(r.get('agents') or [])}  {r.get('note') or ''}")


def _tool_agent_call(a):
    task = str(a.get("task") or "")
    if not task.strip() or task == "-":
        raise SystemExit("task 要给任务描述（MCP 调用里不走 stdin）")
    con = connect()
    try:
        cmd_call(con, SimpleNamespace(
            product=str(a.get("product") or ""), task=task,
            role=a.get("role"), cwd=a.get("cwd"), mem=a.get("mem"),
            mem_n=int(a.get("mem_n") or 5), write=bool(a.get("write")),
            yolo=bool(a.get("yolo")), model=a.get("model"),
            timeout=int(a.get("timeout") or DEFAULT_TIMEOUT),
            no_proxy=bool(a.get("no_proxy")), no_wait=bool(a.get("no_wait"))))
    finally:
        con.close()


def _tool_dispatch_log(a):
    con = connect()
    try:
        cmd_log(con, SimpleNamespace(n=int(a.get("n") or 10), show=None))
    finally:
        con.close()


def _tool_mem_write(a):
    content = str(a.get("content") or "")
    if not content.strip():
        raise SystemExit("content 要给记忆正文")
    agent = str(a.get("agent") or "user")
    kind = str(a.get("kind") or "note")
    con = connect()
    try:
        nid = insert_note(con, agent, kind, str(a.get("topic") or ""),
                          str(a.get("tags") or ""), content)
    finally:
        con.close()
    print(f"已写入共享记忆 #{nid}（agent={agent} kind={kind}）")


def _tool_mem_search(a):
    con = connect()
    try:
        cmd_mem_search(con, SimpleNamespace(
            query=str(a.get("query") or ""), agent=a.get("agent"), kind=a.get("kind"),
            since=a.get("since"), n=int(a.get("n") or 15),
            sort=a.get("sort") or "relevance", tag=a.get("tag"),
            all=False, scope=None, project=None))
    finally:
        con.close()


def _tool_mem_context(a):
    con = connect()
    try:
        cmd_mem_context(con, SimpleNamespace(
            query=str(a.get("query") or ""), n=int(a.get("n") or 5),
            agent=a.get("agent"), project=None))
    finally:
        con.close()


def _tool_mem_list(a):
    con = connect()
    try:
        cmd_mem_list(con, SimpleNamespace(
            n=int(a.get("n") or 15), agent=a.get("agent"), kind=a.get("kind"),
            tag=a.get("tag")))
    finally:
        con.close()


def _tool_mem_read(a):
    try:
        nid = int(a.get("id"))
    except (TypeError, ValueError):
        raise SystemExit("id 要是数字（mem_list / mem_search 的输出里有编号）")
    con = connect()
    try:
        cmd_mem_read(con, SimpleNamespace(id=nid))
    finally:
        con.close()


def _tool_workflow_run(a):
    """走 CLI 子进程跑，避免在 MCP 进程里复制一套 run 目录初始化。"""
    file = str(a.get("file") or "").strip()
    if not file:
        raise SystemExit("file 要给工作流 YAML 的路径")
    cmd = [sys.executable, str(Path(__file__).resolve()), "workflow", "run", file]
    if a.get("dry_run"):
        cmd.append("--dry-run")
    if a.get("project"):
        cmd += ["--project", str(a["project"])]
    if a.get("cwd"):
        cmd += ["--cwd", str(a["cwd"])]
    if a.get("strict_contract"):
        cmd.append("--strict-contract")
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8",
                           timeout=int(a.get("timeout") or 3600))
    except subprocess.TimeoutExpired:
        raise RuntimeError("工作流执行超时")
    text = (r.stdout or "") + (("\n" + r.stderr) if r.stderr else "")
    print(text.strip())
    if r.returncode:
        raise SystemExit(r.returncode)


def _tool_workflow_status(a):
    wf_cmd_status(SimpleNamespace(project=str(a.get("project") or ""), run=a.get("run")))


def _tool_workflow_stop(a):
    wf_cmd_stop(SimpleNamespace(project=str(a.get("project") or "")))


def _tool_workflow_list(a):
    wf_cmd_list(SimpleNamespace(n=int(a.get("n") or 10)))


def _tool_workflow_trace(a):
    project = str(a.get("project") or "")
    runs = wf_runs(project)
    rd = wf_project_dir(project) / "runs" / a["run"] if a.get("run") \
        else (runs[-1] if runs else None)
    if not rd or not rd.exists():
        raise SystemExit(f"项目 {project} 没有对应的运行")
    print(wf_trace_text(rd.name))


def _tool_project_new(a):
    name = str(a.get("name") or "").strip()
    if not name:
        raise SystemExit("name 要给项目名")
    con = connect()
    try:
        cmd_project(con, SimpleNamespace(pj_act="new", name=name))
    finally:
        con.close()


def _tool_catalog_show(a):
    cmd_catalog(None, SimpleNamespace(catalog_act="show", detail=bool(a.get("detail"))))


def _tool_bridge_ls(a):
    cmd_bridge(None, SimpleNamespace(bridge_act="ls", product=None))


MCP_TOOLS = [
    {"name": "agents_list", "description": "列出本机可派工的 Agent（CLI 无头 + 桌面桥接）",
     "schema": _sch({})},
    {"name": "role_list", "description": "列出角色 → Agent 候选",
     "schema": _sch({})},
    {"name": "agent_call", "description": "把任务派给另一个 Agent（CLI 无头或桌面桥接）",
     "schema": _sch({
         "product": ("string", "codex / claude / role:角色名 / 桥接产品名"),
         "task": ("string", "任务描述"),
         "cwd": ("string", "工作目录"),
         "mem": ("string", "派工前检索共享记忆的关键词"),
         "timeout": ("integer", "超时秒数（默认 900）"),
         "no_wait": ("boolean", "桥接派工投完就走"),
     }, ["product", "task"])},
    {"name": "dispatch_log", "description": "查看派工记录",
     "schema": _sch({"n": ("integer", "最多几条（默认 10）")})},
    {"name": "mem_write", "description": "写一条共享记忆",
     "schema": _sch({
         "content": ("string", "记忆正文"),
         "agent": ("string", "写入方（默认 user）"),
         "kind": ("string", "note / decision / fact / handoff"),
         "topic": ("string", "主题"),
         "tags": ("string", "逗号分隔标签"),
     }, ["content"])},
    {"name": "mem_search", "description": "全文检索共享记忆",
     "schema": _sch({
         "query": ("string", "关键词"),
         "agent": ("string", "只看某个 Agent"),
         "kind": ("string", "按类型过滤"),
         "tag": ("string", "按标签过滤"),
         "since": ("string", "7d / 30d / 2026-08-01"),
         "n": ("integer", "最多几条（默认 15）"),
     }, ["query"])},
    {"name": "mem_context", "description": "生成可直接粘进提示词的记忆上下文块",
     "schema": _sch({
         "query": ("string", "关键词"),
         "n": ("integer", "注入几条（默认 5）"),
         "agent": ("string", "只看某个 Agent"),
     }, ["query"])},
    {"name": "mem_list", "description": "最近写入的共享记忆",
     "schema": _sch({
         "n": ("integer", "最近几条（默认 15）"),
         "agent": ("string", "按 Agent 过滤"),
         "kind": ("string", "按类型过滤"),
         "tag": ("string", "按标签过滤"),
     })},
    {"name": "mem_read", "description": "看某条共享记忆全文",
     "schema": _sch({"id": ("integer", "记忆编号")}, ["id"])},
    {"name": "workflow_run", "description": "执行工作流 YAML（建议先 dry_run=true）",
     "schema": _sch({
         "file": ("string", "工作流 YAML 路径"),
         "dry_run": ("boolean", "只看计划不真跑"),
         "strict_contract": ("boolean", "缺契约字段直接拒派"),
         "project": ("string", "项目名"),
         "cwd": ("string", "工作目录"),
     }, ["file"])},
    {"name": "workflow_status", "description": "看某项目最近一次运行的状态",
     "schema": _sch({
         "project": ("string", "项目名"),
         "run": ("string", "run_id（默认最近一次）"),
     }, ["project"])},
    {"name": "workflow_stop", "description": "请求停止运行中的工作流",
     "schema": _sch({"project": ("string", "项目名")}, ["project"])},
    {"name": "workflow_list", "description": "列出项目与运行记录",
     "schema": _sch({"n": ("integer", "每个项目最多几条（默认 10）")})},
    {"name": "workflow_trace", "description": "看某次运行的事件流水",
     "schema": _sch({
         "project": ("string", "项目名"),
         "run": ("string", "run_id（默认最近一次）"),
     }, ["project"])},
    {"name": "project_new", "description": "建项目目录并放工作流模板",
     "schema": _sch({"name": ("string", "项目名")}, ["name"])},
    {"name": "catalog_show", "description": "本机 Agent 索引目录",
     "schema": _sch({"detail": ("boolean", "显示接入细节")})},
    {"name": "bridge_ls", "description": "桌面桥接收件箱/回执箱现状",
     "schema": _sch({})},
]

MCP_TOOL_FNS = {t["name"]: fn for t, fn in zip(MCP_TOOLS, [
    _tool_agents_list, _tool_role_list, _tool_agent_call, _tool_dispatch_log,
    _tool_mem_write, _tool_mem_search, _tool_mem_context, _tool_mem_list,
    _tool_mem_read, _tool_workflow_run, _tool_workflow_status, _tool_workflow_stop,
    _tool_workflow_list, _tool_workflow_trace, _tool_project_new,
    _tool_catalog_show, _tool_bridge_ls])}


def mcp_tool_exec(name, args):
    if name not in MCP_TOOL_FNS:
        raise KeyError(f"未知工具：{name}（tools/list 看清单）")
    buf = io.StringIO()
    code = 0
    try:
        with contextlib.redirect_stdout(buf):
            MCP_TOOL_FNS[name](dict(args or {}))
    except SystemExit as e:
        if isinstance(e.code, int) and e.code:
            code = e.code
        elif isinstance(e.code, str):
            code = 1
            buf.write(e.code.rstrip() + "\n")
        elif e.code not in (None, 0):
            code = 1
    except (YamlishError, RuntimeError, ValueError, OSError) as e:
        code = 1
        buf.write(f"{type(e).__name__}: {e}\n")
    return buf.getvalue().strip("\n"), code


class McpError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def _mcp_handle(method, params):
    if method == "initialize":
        want = str((params or {}).get("protocolVersion") or "")
        return {"protocolVersion": want if want in MCP_SUPPORTED else MCP_PROTO,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "agenthub", "version": MCP_SERVER_VERSION}}
    if method == "ping":
        return {}
    if method == "tools/list":
        return {"tools": [{"name": t["name"], "description": t["description"],
                           "inputSchema": t["schema"]} for t in MCP_TOOLS]}
    if method == "tools/call":
        name = str((params or {}).get("name") or "")
        if name not in MCP_TOOL_FNS:
            raise McpError(-32602, f"未知工具：{name}")
        text, code = mcp_tool_exec(name, (params or {}).get("arguments") or {})
        return {"content": [{"type": "text", "text": text}], "isError": bool(code)}
    raise McpError(-32601, f"未知方法：{method}")


def mcp_serve():
    """stdio JSON-RPC：一行一个请求，一行一个响应；通知不回、坏行回 -32700。"""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except ValueError:
            print(json.dumps({"jsonrpc": "2.0", "id": None,
                              "error": {"code": -32700, "message": "Parse error"}}),
                  flush=True)
            continue
        if not isinstance(req, dict) or "method" not in req:
            print(json.dumps({"jsonrpc": "2.0",
                              "id": req.get("id") if isinstance(req, dict) else None,
                              "error": {"code": -32600, "message": "Invalid Request"}}),
                  flush=True)
            continue
        rid = req.get("id")
        if rid is None:
            continue
        try:
            result = _mcp_handle(str(req.get("method") or ""), req.get("params") or {})
            print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result},
                             ensure_ascii=False), flush=True)
        except McpError as e:
            print(json.dumps({"jsonrpc": "2.0", "id": rid,
                              "error": {"code": e.code, "message": str(e)}},
                             ensure_ascii=False), flush=True)
        except Exception as e:
            print(json.dumps({"jsonrpc": "2.0", "id": rid,
                              "error": {"code": -32603,
                                        "message": f"{type(e).__name__}: {e}"}},
                             ensure_ascii=False), flush=True)


class McpClient:
    """MCP stdio 客户端：spawn server 子进程，行式 JSON-RPC 往返。"""

    def __init__(self, cfg):
        env = dict(os.environ)
        env.update(cfg.get("env") or {})
        self.proc = subprocess.Popen(
            [cfg["command"]] + list(cfg.get("args") or []),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, encoding="utf-8", env=env, bufsize=1)
        self._rq = queue.Queue()
        self._id = 0
        self._lock = threading.Lock()
        threading.Thread(target=self._read_loop, daemon=True).start()

    def _read_loop(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            self._rq.put(msg)
        self._rq.put(None)

    def _send(self, obj):
        self.proc.stdin.write(json.dumps(obj, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params=None, timeout=60):
        with self._lock:
            self._id += 1
            rid = self._id
        self._send({"jsonrpc": "2.0", "id": rid, "method": method,
                    "params": params or {}})
        deadline = time.time() + max(1, int(timeout or 60))
        while True:
            left = deadline - time.time()
            if left <= 0:
                raise RuntimeError(f"MCP 请求超时（{timeout}s）：{method}")
            try:
                msg = self._rq.get(timeout=left)
            except queue.Empty:
                continue
            if msg is None:
                raise RuntimeError(f"MCP server 进程退出了（调用 {method} 时）")
            if not isinstance(msg, dict) or msg.get("id") != rid \
                    or ("result" not in msg and "error" not in msg):
                continue
            if "error" in msg:
                err = msg.get("error") or {}
                raise RuntimeError(f"MCP 错误 {err.get('code')}: {err.get('message')}")
            return msg.get("result")

    def initialize(self, timeout=30):
        try:
            res = self.request("initialize", {
                "protocolVersion": MCP_PROTO, "capabilities": {},
                "clientInfo": {"name": "agenthub", "version": MCP_SERVER_VERSION}},
                timeout)
        except RuntimeError as e:
            # 2026-07-28 的无状态 server 可能不实现 initialize；直接继续。
            if "-32601" in str(e) or "未知方法" in str(e):
                return {}
            raise
        try:
            self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        except OSError:
            pass
        return res

    def close(self):
        try:
            if self.proc.stdin and not self.proc.stdin.closed:
                self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.terminate()
            self.proc.wait(timeout=5)
        except Exception:
            try:
                self.proc.kill()
            except Exception:
                pass


def mcp_registry():
    try:
        data = json.loads(MCP_SERVERS_FILE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return data if isinstance(data, dict) else {}


def mcp_registry_save(reg):
    MCP_HOME.mkdir(parents=True, exist_ok=True)
    MCP_SERVERS_FILE.write_text(
        json.dumps(reg, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def mcp_list_tools(server, timeout=60):
    if server == "self":
        return [{"name": t["name"], "description": t["description"],
                 "inputSchema": t["schema"]} for t in MCP_TOOLS]
    cfg = mcp_registry().get(server)
    if not cfg:
        raise RuntimeError(f"MCP server「{server}」没注册（agenthub mcp add {server} …）")
    cli = McpClient(cfg)
    try:
        cli.initialize(timeout=timeout)
        return cli.request("tools/list", timeout=timeout).get("tools") or []
    finally:
        cli.close()


def mcp_call_tool(server, tool, arguments=None, timeout=None):
    if server == "self":
        return mcp_tool_exec(tool, arguments or {})
    cfg = mcp_registry().get(server)
    if not cfg:
        raise RuntimeError(f"MCP server「{server}」没注册（agenthub mcp add {server} …）")
    cli = McpClient(cfg)
    try:
        cli.initialize(timeout=30)
        res = cli.request("tools/call",
                          {"name": tool, "arguments": dict(arguments or {})},
                          timeout or MCP_CALL_TIMEOUT)
    finally:
        cli.close()
    parts = [c.get("text", "") for c in (res.get("content") or [])
             if isinstance(c, dict) and c.get("type") == "text"]
    text = "\n".join(p for p in parts if p)
    return text, 1 if res.get("isError") else 0


def cmd_mcp(con, args):
    """mcp 命令组全程不碰数据库（con 形参只用于签名对齐）。"""
    act = args.mcp_act
    if act == "serve":
        mcp_serve()
        return
    if act == "add":
        name = str(args.name or "").strip()
        if not name:
            raise SystemExit("要给 server 起个名字：agenthub mcp add 名字 启动命令…")
        launch = list(args.launch or [])
        env = {}
        while launch and (launch[0] == "--env" or launch[0].startswith("--env=")):
            tok = launch.pop(0)
            if tok == "--env":
                if not launch:
                    raise SystemExit("--env 后面要跟 K=V")
                kv = launch.pop(0)
            else:
                kv = tok[len("--env="):]
            if "=" not in kv:
                raise SystemExit(f"--env 要 K=V 格式，收到的是：{kv}")
            k, _, v = kv.partition("=")
            env[k.strip()] = v
        if not launch:
            raise SystemExit("要给启动命令：agenthub mcp add 名字 命令 参数…")
        reg = mcp_registry()
        if name in reg:
            raise SystemExit(f"server「{name}」已注册（先 agenthub mcp rm {name}）")
        reg[name] = {"command": launch[0], "args": launch[1:], "env": env}
        mcp_registry_save(reg)
        print(f"已注册 MCP server「{name}」→ {MCP_SERVERS_FILE}")
        print(f"试一下：agenthub mcp tools {name}")
        return
    if act == "list":
        reg = mcp_registry()
        if not reg:
            print("（还没有注册外部 MCP server。agenthub mcp add 名字 命令…）")
            return
        for name, c in list(reg.items())[:max(0, getattr(args, "n", 10) or 10)]:
            envs = f" · env {', '.join(sorted(c.get('env') or {}))}" if c.get("env") else ""
            print(f"  {name:<16} {c.get('command')} "
                  f"{' '.join(c.get('args') or [])}{envs}")
        return
    if act == "rm":
        name = str(args.name or "").strip()
        reg = mcp_registry()
        if name not in reg:
            raise SystemExit(f"server「{name}」没注册（agenthub mcp list 看名单）")
        reg.pop(name)
        mcp_registry_save(reg)
        print(f"已删除「{name}」")
        return
    if act == "tools":
        name = str(args.name or "").strip()
        for t in mcp_list_tools(name):
            print(f"  {t.get('name', '?'):<24} {t.get('description') or ''}")
        return
    if act == "call":
        raw = getattr(args, "args_json", None)
        call_args = {}
        if raw:
            try:
                call_args = json.loads(raw)
            except ValueError as e:
                raise SystemExit(f"--args-json 不是合法 JSON：{e}")
            if not isinstance(call_args, dict):
                raise SystemExit("--args-json 要是一个 JSON 对象")
        out, code = mcp_call_tool(str(args.name or ""), str(args.tool or ""),
                                  call_args,
                                  getattr(args, "timeout", None) or MCP_CALL_TIMEOUT)
        print(out)
        if code:
            sys.exit(code)
        return
    if act == "config":
        frag = {"command": sys.executable.replace("\\", "/"),
                "args": [str(Path(__file__).resolve()).replace("\\", "/"),
                         "mcp", "serve"],
                "env": {"PYTHONIOENCODING": "utf-8"}}
        if os.environ.get("AGENTHUB_HOME"):
            frag["env"]["AGENTHUB_HOME"] = os.environ["AGENTHUB_HOME"]
        print("把下面这段加进 MCP 客户端的 mcpServers 配置里，重启客户端生效：")
        print(json.dumps({"mcpServers": {"agenthub": frag}},
                         ensure_ascii=False, indent=2))
        return
    raise SystemExit(f"不认识的 mcp 子命令：{act}")


def wf_step_tool(s, prompt, cwd, opts, run_dir, label=""):
    """tool 步骤：引擎直接调 MCP 工具，产物和派工记录沿用同一套。"""
    server, tool = s["tool"].split("/", 1)
    timeout = (s.get("_effective_timeout") or s.get("timeout")
               or getattr(opts, "timeout", DEFAULT_TIMEOUT) or MCP_CALL_TIMEOUT)
    t0 = time.time()
    out, code = mcp_call_tool(server, tool, dict(s.get("arguments") or {}), timeout)
    dur = time.time() - t0
    con = connect()
    try:
        wf_name = label or flow_name_of(run_dir)
        record_dispatch(con, f"mcp:{server}", str(cwd), prompt, "", code, dur,
                        "", out, chain_from_env() + [f"wf:{wf_name}"], "")
        insert_note(con, f"mcp:{server}", "dispatch",
                    f"工作流[{wf_name}] 步骤 {s['id']}"[:60],
                    "agenthub,workflow,mcp",
                    f"工作流步骤 {s['id']} 调用 MCP 工具 {s['tool']}（cwd={cwd}）。\n"
                    f"参数: {json.dumps(s.get('arguments') or {}, ensure_ascii=False)}\n"
                    f"结果摘要: {(out or '').strip()[:1500] or '(无输出)'}")
    finally:
        con.close()
    return out, "", code, dur, ""


# ================================================== 本地调用控制台（P3）====

CONSOLE_HTML_PATH = Path(__file__).resolve().with_name("console.html")
CONSOLE_JOBS = {}
CONSOLE_LOCK = threading.Lock()

_CONSOLE_FALLBACK = """<!doctype html><meta charset="utf-8">
<title>AgentHub Console</title>
<body style="background:#0C0E14;color:#E8E4DB;font:15px system-ui;padding:40px">
<h1>AgentHub Console</h1>
<p>缺少 <code>console.html</code>。请把它和 <code>agenthub.py</code> 放在同一目录。</p>
</body>"""


def _console_spawn(kind, cmd, cwd=None):
    jid = "job-" + dt.datetime.now().strftime("%H%M%S") + "-" + os.urandom(2).hex()
    job = {"id": jid, "kind": kind, "status": "running", "output": "",
           "code": None, "started": now_iso(), "ended": ""}
    with CONSOLE_LOCK:
        if len(CONSOLE_JOBS) >= 50:
            for k in list(CONSOLE_JOBS)[:10]:
                CONSOLE_JOBS.pop(k, None)
        CONSOLE_JOBS[jid] = job

    def run():
        try:
            p = subprocess.Popen(cmd, cwd=cwd or None, stdout=subprocess.PIPE,
                                 stderr=subprocess.STDOUT, text=True,
                                 encoding="utf-8", errors="replace", bufsize=1)
            for line in p.stdout:
                with CONSOLE_LOCK:
                    if len(job["output"]) < 200_000:
                        job["output"] += line
            code = p.wait()
            with CONSOLE_LOCK:
                job["code"] = code
                job["status"] = "done" if code == 0 else "failed"
        except Exception as e:
            with CONSOLE_LOCK:
                job["status"] = "failed"
                job["output"] += f"\n[console] {type(e).__name__}: {e}\n"
        finally:
            with CONSOLE_LOCK:
                job["ended"] = now_iso()

    threading.Thread(target=run, daemon=True).start()
    return job


def _console_runs(limit=20):
    out = []
    if not PROJECTS_DIR.exists():
        return out
    projects = sorted((p for p in PROJECTS_DIR.iterdir() if p.is_dir()),
                      key=lambda p: p.stat().st_mtime, reverse=True)[:10]
    for pd in projects:
        for rd in reversed(wf_runs(pd.name)[-5:]):
            st = wf_load_state(rd)
            if st:
                out.append({"project": pd.name, "run_id": st["run_id"],
                            "workflow": st["workflow"], "status": st["status"],
                            "updated_at": st["updated_at"]})
    return out[:limit]


def _console_overview(con):
    agents = []
    for pid, a in AGENTS.items():
        agents.append({"id": pid, "name": pid, "kind": "cli",
                       "available": bool(shutil.which(a["bin"])),
                       "desc": a.get("desc", ""),
                       "tags": (CAPS.get(pid) or {}).get("strength_tags", [])})
    for pid, b in BRIDGE_PRODUCTS.items():
        agents.append({"id": pid, "name": b.get("name", pid), "kind": "bridge",
                       "available": agent_available(pid),
                       "desc": "桌面桥接（共享目录）",
                       "tags": (CAPS.get(pid) or {}).get("strength_tags", [])})
    stages = {r["stage"]: r["c"] for r in con.execute(
        "select stage,count(*) c from tickets group by stage")}
    open_n = con.execute("select count(*) from tickets where ended_at=''").fetchone()[0]
    inbox_n = con.execute(
        "select count(*) from tickets where ended_at='' and stage like 'person_%'"
    ).fetchone()[0]
    effects_n = con.execute("select count(*) from effects").fetchone()[0]
    health = [dict(r) for r in con.execute("select * from health order by kind")]
    policy = policy_load()
    return {"now": now_iso(), "agents": agents, "tickets": {
        "total": sum(stages.values()), "open": open_n, "inbox": inbox_n,
        "by_stage": stages}, "effects": effects_n, "health": health,
        "policy": {"mode": policy.get("mode"), "version": policy_version(policy),
                   "rules": len(policy.get("rules") or []),
                   "sources": len(policy.get("sources") or {})},
        "runs": _console_runs(12), "inbox_tickets": ticket_inbox(6, con=con)}


def _console_ticket_detail(con, tid):
    t = ticket_get(tid, con=con)
    if not t:
        return None
    return {"ticket": t, "mandate": mandate_get(tid, con=con),
            "assignments": ticket_assignments(tid, con=con),
            "inputs": ticket_input_rows(tid, con=con)}


class ConsoleHandler(BaseHTTPRequestHandler):
    server_version = "AgentHubConsole/1.0"

    def log_message(self, fmt, *args):
        pass

    def _local(self):
        return self.client_address[0] in ("127.0.0.1", "::1")

    def _json(self, code, obj):
        raw = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _html(self, text, code=200):
        raw = text.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n > 2_000_000:
            raise SystemExit("请求体过大")
        raw = self.rfile.read(n) if n else b"{}"
        try:
            return json.loads(raw.decode("utf-8"))
        except ValueError as e:
            raise SystemExit(f"JSON 解析失败：{e}")

    def do_GET(self):
        if not self._local():
            return self._json(403, {"error": "local only"})
        path = urlsplit(self.path).path
        if path in ("/", "/index.html"):
            try:
                return self._html(CONSOLE_HTML_PATH.read_text(encoding="utf-8"))
            except OSError:
                return self._html(_CONSOLE_FALLBACK)
        if path == "/favicon.ico":
            self.send_response(204)
            self.end_headers()
            return
        con = connect()
        try:
            if path == "/api/overview":
                return self._json(200, _console_overview(con))
            if path == "/api/tickets":
                qs = parse_qs(urlsplit(self.path).query)
                stage = (qs.get("stage") or [None])[0]
                open_only = bool(qs.get("open") and qs["open"][0] == "1")
                limit = int((qs.get("limit") or ["50"])[0])
                return self._json(200, ticket_list(stage, open_only, limit, con=con))
            if path == "/api/ticket":
                qs = parse_qs(urlsplit(self.path).query)
                tid = (qs.get("id") or [""])[0]
                d = _console_ticket_detail(con, tid)
                return self._json(200, d) if d else self._json(404, {"error": "not found"})
            if path == "/api/runs":
                return self._json(200, _console_runs(30))
            if path == "/api/trace":
                qs = parse_qs(urlsplit(self.path).query)
                project = (qs.get("project") or [""])[0]
                rid = (qs.get("run") or [""])[0]
                runs = wf_runs(project)
                rd = wf_project_dir(project) / "runs" / rid if rid else (runs[-1] if runs else None)
                if not rd or not rd.exists():
                    return self._json(404, {"error": "run not found"})
                return self._json(200, wf_event_list(rd.name))
            if path == "/api/policy":
                p = policy_load()
                return self._json(200, {"version": policy_version(p), **p})
            if path == "/api/health":
                return self._json(200, {
                    "health": [dict(r) for r in con.execute(
                        "select * from health order by kind")],
                    "effects": effect_rows(con=con)})
            if path == "/api/job":
                qs = parse_qs(urlsplit(self.path).query)
                jid = (qs.get("id") or [""])[0]
                job = CONSOLE_JOBS.get(jid)
                return self._json(200, job) if job else self._json(404, {"error": "job not found"})
            return self._json(404, {"error": "not found"})
        finally:
            con.close()

    def do_POST(self):
        if not self._local():
            return self._json(403, {"error": "local only"})
        path = urlsplit(self.path).path
        try:
            body = self._body()
        except SystemExit as e:
            return self._json(400, {"error": str(e)})
        con = connect()
        try:
            if path == "/api/ticket":
                tid = str(body.get("id") or "")
                action = str(body.get("action") or "")
                if action == "answer":
                    t = ticket_answer(tid, str(body.get("text") or ""), con=con)
                elif action == "note":
                    t = ticket_answer(tid, str(body.get("text") or ""), kind="note", con=con)
                elif action == "retry":
                    t = ticket_retry(tid, con=con)
                elif action == "stop":
                    t = ticket_stop(tid, str(body.get("reason") or ""), con=con)
                elif action == "report":
                    t = ticket_report(tid, str(body.get("state") or "progress"),
                                      detail=str(body.get("detail") or ""),
                                      question=str(body.get("question") or ""), con=con)
                else:
                    return self._json(400, {"error": f"unknown action: {action}"})
                return self._json(200, _console_ticket_detail(con, t["id"]))
            if path == "/api/policy":
                p = policy_load()
                act = str(body.get("action") or "")
                if act == "mode":
                    mode = str(body.get("mode") or "")
                    if mode not in ("off", "observe", "auto"):
                        return self._json(400, {"error": "mode must be off/observe/auto"})
                    p["mode"] = mode
                elif act in ("allow", "disallow"):
                    source = str(body.get("source") or "")
                    actor = str(body.get("actor") or "")
                    part = p.setdefault("sources", {}).setdefault(
                        source, {"allowed": [], "admission": "inferred"})
                    allowed = part.setdefault("allowed", [])
                    if act == "allow" and actor not in allowed:
                        allowed.append(actor)
                    if act == "disallow":
                        allowed[:] = [a for a in allowed if a != actor]
                elif act == "rule":
                    rid = str(body.get("id") or "")
                    if not rid:
                        return self._json(400, {"error": "rule id required"})
                    rules = p.setdefault("rules", [])
                    found = next((r for r in rules if r.get("id") == rid), None)
                    if found:
                        found["instruction"] = str(body.get("instruction") or "")
                        found["enabled"] = bool(body.get("enabled", True))
                    else:
                        rules.append({"id": rid,
                                      "instruction": str(body.get("instruction") or ""),
                                      "enabled": bool(body.get("enabled", True))})
                else:
                    return self._json(400, {"error": f"unknown action: {act}"})
                policy_save(p)
                return self._json(200, {"version": policy_version(p), **p})
            if path == "/api/call":
                product = str(body.get("product") or "")
                task = str(body.get("task") or "")
                if not product or not task:
                    return self._json(400, {"error": "product/task 必填"})
                cmd = [sys.executable, str(Path(__file__).resolve()),
                       "call", product, task]
                if body.get("ticket"):
                    cmd += ["--ticket", str(body["ticket"])]
                if body.get("cwd"):
                    cmd += ["--cwd", str(body["cwd"])]
                for flag in ("write", "yolo", "no_wait", "approval"):
                    if body.get(flag):
                        cmd.append("--" + flag)
                if body.get("timeout"):
                    cmd += ["--timeout", str(int(body["timeout"]))]
                return self._json(200, {"job": _console_spawn(
                    "call", cmd, body.get("cwd") or None)})
            if path == "/api/workflow":
                file = str(body.get("file") or "")
                if not file:
                    return self._json(400, {"error": "file 必填"})
                cmd = [sys.executable, str(Path(__file__).resolve()),
                       "workflow", "run", file]
                if body.get("ticket"):
                    cmd += ["--ticket", str(body["ticket"])]
                if body.get("dry_run"):
                    cmd.append("--dry-run")
                if body.get("cwd"):
                    cmd += ["--cwd", str(body["cwd"])]
                return self._json(200, {"job": _console_spawn(
                    "workflow", cmd, body.get("cwd") or None)})
            if path == "/api/effects/run":
                n = effect_run(body.get("kind") or None, int(body.get("limit") or 20), con=con)
                return self._json(200, {"processed": n})
            return self._json(404, {"error": "not found"})
        except SystemExit as e:
            return self._json(400, {"error": str(e)})
        except Exception as e:
            return self._json(500, {"error": f"{type(e).__name__}: {e}"})
        finally:
            con.close()


def cmd_console(con, args):
    port = int(getattr(args, "port", 8766) or 8766)
    try:
        server = ThreadingHTTPServer(("127.0.0.1", port), ConsoleHandler)
    except OSError as e:
        raise SystemExit(f"控制台端口 {port} 起不来：{e}")
    url = f"http://127.0.0.1:{port}/"
    print(f"AgentHub 控制台 → {url}（只监听本机，Ctrl+C 停止）")
    if not getattr(args, "no_browser", False):
        try:
            webbrowser.open(url)
        except Exception:
            pass
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n控制台已停止")
    finally:
        server.server_close()


def main():
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass

    ap = argparse.ArgumentParser(prog="agenthub",
                                 description="多 Agent 共享记忆 + 子 Agent 派工")
    sub = ap.add_subparsers(dest="cmd", required=True)

    mem = sub.add_parser("mem", help="共享记忆：写入/查询/同步")
    msub = mem.add_subparsers(dest="sub", required=True)

    w = msub.add_parser("write", help="写一条共享记忆（内容为 - 时读 stdin）")
    w.add_argument("content", nargs="?", default="")
    w.add_argument("--agent", default="user", help="写入方（哪个 Agent 或 user）")
    w.add_argument("--kind", default="note",
                   choices=["note", "decision", "fact", "handoff", "style", "digest"],
                   help="记忆类型（style=输出偏好层，自动全局置顶）")
    w.add_argument("--topic", default="", help="主题")
    w.add_argument("--tags", default="", help="逗号分隔标签")
    w.add_argument("--scope", choices=list(SCOPES), default="",
                   help="作用域：global / project（会按目录自动推断项目名）/ session")
    w.add_argument("--project", default="", help="项目名；给了就进项目层，默认按当前目录推断")
    w.add_argument("--key", default="", help="逻辑键：同 key 的旧条目自动标为作废（新覆盖旧）")
    w.add_argument("--pin", action="store_true", help="置顶：每次 context 必注入（只给硬规则用）")
    w.add_argument("--supersede", type=int, default=None, help="指定要作废的旧条目 id")
    w.set_defaults(fn=cmd_mem_write)

    l = msub.add_parser("list", help="最近记忆")
    l.add_argument("-n", type=int, default=15)
    l.add_argument("--agent"); l.add_argument("--kind")
    l.add_argument("--tag", help="按标签过滤（逗号分隔，任一命中）")
    l.set_defaults(fn=cmd_mem_list)

    r = msub.add_parser("read", help="看某条记忆全文")
    r.add_argument("id", type=int)
    r.set_defaults(fn=cmd_mem_read)

    f = msub.add_parser("forget", help="删某条记忆")
    f.add_argument("id", type=int)
    f.set_defaults(fn=cmd_mem_forget)

    s = msub.add_parser("search", help="全文检索（关键词、产物名、项目名都行）")
    s.add_argument("query")
    s.add_argument("--agent", help="只看某个产品")
    s.add_argument("--kind", help="note/decision/fact/handoff/dispatch/native")
    s.add_argument("--since", help="7d / 30d / 2026-08-01")
    s.add_argument("--tag", help="按标签过滤（逗号分隔，任一命中）")
    s.add_argument("--scope", choices=list(SCOPES), help="只看某一层")
    s.add_argument("--project", default="", help="只看该项目（含全局层）")
    s.add_argument("--all", action="store_true",
                   help="连已作废、已入摘要的条目一起查（排查旧结论时用）")
    s.add_argument("-n", type=int, default=15)
    s.add_argument("--sort", default="relevance", choices=["relevance", "time"])
    s.set_defaults(fn=cmd_mem_search)

    c = msub.add_parser("context", help="分层装配可粘进提示词的记忆上下文块")
    c.add_argument("query")
    c.add_argument("-n", type=int, default=5)
    c.add_argument("--agent")
    c.add_argument("--project", default="",
                   help="项目名（默认按当前目录/AGENTHUB_PROJECT 自动推断）")
    c.add_argument("--total", type=int, default=0, help=f"总字数预算，默认 {CONTEXT_TOTAL}")
    c.add_argument("--all", action="store_true", help="不过滤已作废条目")
    c.set_defaults(fn=cmd_mem_context)

    pn = msub.add_parser("pin", help="置顶某条记忆（每次 context 必注入）")
    pn.add_argument("id", type=int, nargs="+")
    pn.add_argument("--unpin", action="store_true", help="取消置顶")
    pn.set_defaults(fn=cmd_mem_pin)

    ly = msub.add_parser("layers", help="看分层现状：每层多少条、谁置顶、多少已作废/已蒸馏")
    ly.set_defaults(fn=cmd_mem_layers)

    dg = msub.add_parser("digest", help="把近期碎结论压成摘要（--write 才落库）")
    dg.add_argument("--days", type=int, default=7)
    dg.add_argument("--project", default="", help="只蒸馏某个项目")
    dg.add_argument("--write", action="store_true",
                    help="落库成 kind=digest，并把原条目标为已入摘要")
    dg.set_defaults(fn=cmd_mem_digest)

    stl = msub.add_parser("style", help="输出偏好层：查看 / 写入 ~/.agents/style.md 并常驻注入")
    stl.add_argument("content", nargs="?", default="")
    stl.add_argument("--write", action="store_true",
                     help="写入（内容为 - 时读 stdin；不带内容则读事实源文件）")
    stl.set_defaults(fn=cmd_mem_style)

    sy = msub.add_parser("sync", help="同步各产品原生记忆到共享索引（增量）")
    sy.set_defaults(fn=cmd_mem_sync)

    sc = msub.add_parser("source", help="原生记忆源：list / add / rm / scan（配置存 sources.json）")
    scs = sc.add_subparsers(dest="source_act", required=True)
    scs.add_parser("list", help="列出现有源及各自命中文件数")
    sa = scs.add_parser("add", help="接入一个新产品的记忆文件")
    sa.add_argument("glob", help="相对 home 的 glob，如 .foo/memory/**/*.md")
    sa.add_argument("--product", help="产品名，默认取 glob 的第一段")
    sr = scs.add_parser("rm", help="删掉一条源（该产品的原生记忆同时清出共享库）")
    sr.add_argument("target", help="glob 原文或产品名")
    ss = scs.add_parser("scan", help="扫本机还没接入的记忆目录")
    ss.add_argument("--fix", action="store_true", help="直接把扫到的全部写进 sources.json 并同步")
    sc.set_defaults(fn=cmd_mem_source)

    st = msub.add_parser("stats", help="共享记忆构成")
    st.set_defaults(fn=cmd_mem_stats)

    ag = sub.add_parser("agents", help="列出可派工的子 Agent")
    ag.set_defaults(fn=cmd_agents)

    ct = sub.add_parser("catalog", help="本机 Agent 索引目录：build / show / card")
    cts = ct.add_subparsers(dest="catalog_act", required=True)
    cts.add_parser("build", help="生成/刷新 ~/.agents/catalog.json（含在装判断）")
    sh = cts.add_parser("show", help="列出目录里每家的记忆/余额接入状态")
    sh.add_argument("--detail", action="store_true", help="连余额端点与鉴权方式一起打印")
    cc = cts.add_parser("card", help="打印某个 Agent 的 Agent Card（能力/接入/上限）")
    cc.add_argument("product")
    ct.set_defaults(fn=cmd_catalog)

    qt = sub.add_parser("quota", help="从本机直读各 Agent 的积分（DPAPI + v10/AES-GCM / 本地库聚合，只读、不联网）")
    qt.add_argument("product", nargs="?", help="qoder-cn / qoder（余额全量）· workbuddy（仅累计已用）；省略则全部")
    qt.add_argument("--profile", help="指定该账号自己的档案目录（按账号分别读时用）："
                                      "Qoder 默认 %%APPDATA%%/<产品>，WorkBuddy 默认 ~/.workbuddy")
    qt.add_argument("--local", action="store_true", help="只读客户端本地缓存，不调服务端实时接口")
    qt.add_argument("--json", action="store_true", help="输出机器可读的原始结构")
    qt.add_argument("--write", action="store_true", help="把这次快照写进共享记忆，别的 Agent 也能看到")
    qt.set_defaults(fn=cmd_quota)

    ta = sub.add_parser("trae", help="Trae 反代专用：只读吐出 access token 或模型目录（不碰 refreshToken）")
    ta.add_argument("--token", action="store_true", help="输出 access token、到期时间与账号")
    ta.add_argument("--models", action="store_true",
                    help="输出 state.vscdb 里服务端下发的模型目录（线上接口对 solo_* 恒空）")
    ta.add_argument("--profile", default="",
                    help="指定档案目录；默认 %%APPDATA%%\\Trae CN，回落 TRAE SOLO CN")
    ta.set_defaults(fn=cmd_trae)

    ck = sub.add_parser("checkin",
                        help="查各 Agent 的每日签到状态（只读）；--claim 且指明产品才真的去领")
    ck.add_argument("product", nargs="?",
                    help="workbuddy / workbuddy-intl / trae-cn / minimax / qoder-cn / qoder / "
                         "raccoon / doubao；省略=只查全部")
    ck.add_argument("--claim", action="store_true",
                    help="发写请求去领取。必须同时指名产品，不允许一条命令把所有号都签一遍")
    ck.add_argument("--json", action="store_true", help="输出机器可读结构（驾驶舱读这个）")
    ck.add_argument("--profile",
                    help="按账号查：这个号自己的档案目录（驾驶舱把该号存过的登录包铺到临时目录后指过来）。"
                         "只有 qoder / qoder-cn 走这个")
    ck.add_argument("--auth-file",
                    help="按账号查：这个号自己的登录态文件（workbuddy 系用，铺出来的是那份 .info）")
    ck.add_argument("--raw", action="store_true",
                    help="把每家这次打的原始请求与响应一起带出来，排查解析对不对用")
    ck.set_defaults(fn=cmd_checkin)

    ca = sub.add_parser("call",
                        help="把任务派给子 Agent：CLI 产品无头执行，桌面产品桥接投递")
    ca.add_argument("product", nargs="?",
                    help="codex / claude / role:角色名 / 桥接产品（workbuddy 等）；配 --role 时可省")
    ca.add_argument("task", nargs="?", help="任务描述（为 - 时读 stdin）")
    ca.add_argument("--role", help="按角色派工（候选里挑第一个可用的）")
    ca.add_argument("--cwd", help="子 Agent 的工作目录（默认当前目录）")
    ca.add_argument("--mem", help="派工前按此查询检索共享记忆并注入提示词")
    ca.add_argument("--mem-n", type=int, default=5)
    ca.add_argument("--write", action="store_true", help="codex: 允许写工作区（workspace-write）")
    ca.add_argument("--yolo", action="store_true", help="claude: 跳过权限确认")
    ca.add_argument("--model", help="指定模型")
    ca.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    ca.add_argument("--no-proxy", action="store_true",
                    help="不自动探测并注入本机代理给子 Agent")
    ca.add_argument("--no-wait", action="store_true",
                    help="桥接派工：投递完不等回执就返回")
    ca.add_argument("--approval", dest="requires_approval", action="store_true",
                    help="桥接派工前等待人工审批（agenthub approval decide）")
    ca.add_argument("--ticket", help="关联工单；派工完成时写 assignment/report")
    ca.set_defaults(fn=cmd_call)

    ro = sub.add_parser("role", help="角色 → Agent 候选（call role:角色 用）：list / assign / remove")
    ros = ro.add_subparsers(dest="role_act", required=True)
    ros.add_parser("list", help="列出角色与候选")
    ra = ros.add_parser("assign", help="把 Agent 指派给角色")
    ra.add_argument("role"); ra.add_argument("agent")
    rr = ros.add_parser("remove", help="把 Agent 移出角色")
    rr.add_argument("role"); rr.add_argument("agent")
    ro.set_defaults(fn=cmd_role)

    br = sub.add_parser("bridge", help="桌面端桥接：ls 看收件/回执，setup <产品> 初始化并生成说明")
    brs = br.add_subparsers(dest="bridge_act", required=True)
    brs.add_parser("ls", help="各产品收件箱/回执箱现状（含租约）")
    bs = brs.add_parser("setup", help="初始化某产品的桥接目录并生成 README")
    bs.add_argument("product")
    brp = brs.add_parser("reap", help="把租约过期的 pending 任务回收成失败回执")
    brp.add_argument("--product", help="只处理某个产品")
    brh = brs.add_parser("heartbeat", help="给某任务续租")
    brh.add_argument("product")
    brh.add_argument("id")
    brh.add_argument("--seconds", type=int, default=300)
    br.set_defaults(fn=cmd_bridge)

    apv = sub.add_parser("approval", help="人工审批：list / decide")
    apvs = apv.add_subparsers(dest="approval_act", required=True)
    apl = apvs.add_parser("list", help="看审批记录")
    apl.add_argument("--run", help="只看某个 run")
    apl.add_argument("-n", type=int, default=20)
    apd = apvs.add_parser("decide", help="批准或拒绝一个审批")
    apd.add_argument("id")
    apd.add_argument("decision", choices=["yes", "no"])
    apd.add_argument("--by", default="user")
    apv.set_defaults(fn=cmd_approval)

    tkt = sub.add_parser("ticket", help="工单：new / intake / list / inbox / show / report / answer / note / retry / stop")
    tks = tkt.add_subparsers(dest="ticket_act", required=True)
    tn = tks.add_parser("new", help="人工建一张工单")
    tn.add_argument("--title", required=True)
    tn.add_argument("--requester", default="user")
    tn.add_argument("--event-id", dest="event_id", help="来源事件 id（幂等）")
    tn.add_argument("--scope", default="")
    tn.add_argument("--project", default="")
    tn.add_argument("--workspace", default="", help="该工单的独立工作目录")
    tn.add_argument("--run", default="", help="关联已有 run_id")
    tn.add_argument("--rule", action="append", help="匹配的规则 id（可多次）")
    tn.add_argument("--body", default="")
    tn.set_defaults(fn=cmd_ticket)
    ti = tks.add_parser("intake", help="按 Policy 准入一条来源事件")
    ti.add_argument("--source", default="cli")
    ti.add_argument("--actor", default="user")
    ti.add_argument("--event-id", dest="event_id", default="")
    ti.add_argument("--title", default="")
    ti.add_argument("--scope", default="")
    ti.add_argument("--project", default="")
    ti.add_argument("--workspace", default="")
    ti.add_argument("--run", default="")
    ti.add_argument("--text", default="")
    ti.add_argument("--assess-json", dest="assess_json",
                    help='结构化研判 JSON，如 {"direction":"implementation_requested","evidence":"...","rules":[{"id":"r1","matched":true,"reason":"..."}]}')
    ti.add_argument("--unaddressed", action="store_true", help="没有直接 @Agent")
    ti.set_defaults(fn=cmd_ticket)
    tl = tks.add_parser("list", help="工单列表")
    tl.add_argument("--stage", help="阶段，如 person%% / working / ended")
    tl.add_argument("--open", action="store_true", help="只看未结束")
    tl.add_argument("-n", type=int, default=50)
    tl.add_argument("--json", action="store_true")
    tl.set_defaults(fn=cmd_ticket)
    tb = tks.add_parser("inbox", help="只看需要人的工单")
    tb.add_argument("-n", type=int, default=50)
    tb.add_argument("--json", action="store_true")
    tb.set_defaults(fn=cmd_ticket)
    ts = tks.add_parser("show", help="看工单详情")
    ts.add_argument("id")
    ts.set_defaults(fn=cmd_ticket)
    tr = tks.add_parser("report", help="写工单报告并更新阶段")
    tr.add_argument("id")
    tr.add_argument("state", choices=["progress", "waiting", "ready", "blocked", "failed", "dropped"])
    tr.add_argument("--detail", default="")
    tr.add_argument("--question", default="")
    tr.set_defaults(fn=cmd_ticket)
    ta = tks.add_parser("answer", help="人回一句话，恢复/继续工单")
    ta.add_argument("id")
    ta.add_argument("text", nargs="+")
    ta.set_defaults(fn=cmd_ticket)
    tn2 = tks.add_parser("note", help="给工单加一条上下文（不授权）")
    tn2.add_argument("id")
    tn2.add_argument("text", nargs="+")
    tn2.set_defaults(fn=cmd_ticket)
    tr2 = tks.add_parser("retry", help="重试卡住的工单")
    tr2.add_argument("id")
    tr2.set_defaults(fn=cmd_ticket)
    tsp = tks.add_parser("stop", help="人工停止工单")
    tsp.add_argument("id")
    tsp.add_argument("--reason", default="")
    tsp.set_defaults(fn=cmd_ticket)

    pol = sub.add_parser("policy", help="工单准入策略：show / set-mode / allow / disallow / rule-add / rule-list")
    pols = pol.add_subparsers(dest="policy_act", required=True)
    pols.add_parser("show", help="看当前策略和版本")
    pm = pols.add_parser("set-mode", help="off / observe / auto")
    pm.add_argument("mode", choices=["off", "observe", "auto"])
    pa = pols.add_parser("allow", help="允许某个来源的某个发起人")
    pa.add_argument("source"); pa.add_argument("actor")
    pd = pols.add_parser("disallow", help="移除某个来源的发起人")
    pd.add_argument("source"); pd.add_argument("actor")
    pr = pols.add_parser("rule-add", help="添加/更新一条主动接单规则")
    pr.add_argument("id"); pr.add_argument("instruction")
    pr.add_argument("--disable", action="store_true")
    pols.add_parser("rule-list", help="列出规则")
    pol.set_defaults(fn=cmd_policy)

    ef = sub.add_parser("effects", help="持久化副作用队列：list / run")
    efs = ef.add_subparsers(dest="effect_act", required=True)
    el = efs.add_parser("list", help="看待执行的副作用")
    el.add_argument("--kind", default=None)
    er = efs.add_parser("run", help="执行到期的副作用")
    er.add_argument("--kind", default=None)
    er.add_argument("-n", type=int, default=20)
    ef.set_defaults(fn=cmd_effects)

    hl = sub.add_parser("health", help="各子系统健康状态")
    hl.set_defaults(fn=cmd_health)

    src = sub.add_parser("source", help="来源视图：list / show（GitHub/飞书适配器待接）")
    srcs = src.add_subparsers(dest="source_act", required=True)
    srcs.add_parser("list", help="列出策略里的来源、工单数和事件数")
    sh = srcs.add_parser("show", help="看某个来源的策略")
    sh.add_argument("name")
    si = srcs.add_parser("ingest", help="从 JSON 文件喂一批来源事件（通用 Source 适配器入口）")
    si.add_argument("--source", required=True)
    si.add_argument("--file", required=True, help="事件 JSON；- 表示 stdin")
    si.add_argument("--actor", default="user")
    si.add_argument("--unaddressed", action="store_true", help="事件没有直接 @Agent")
    src.set_defaults(fn=cmd_source)

    csl = sub.add_parser("console", help="本机可视化控制台：总览/收件箱/调用/工作流/策略/追踪")
    csl.add_argument("--port", type=int, default=8766, help="监听端口（默认 8766，仅 127.0.0.1）")
    csl.add_argument("--no-browser", action="store_true", help="启动时不自动打开浏览器")
    csl.set_defaults(fn=cmd_console)

    mc = sub.add_parser("mcp", help="MCP 双向接入：serve / config / add / list / rm / tools / call")
    mcs = mc.add_subparsers(dest="mcp_act", required=True)
    mcs.add_parser("serve", help="以 stdio MCP Server 方式运行（给 MCP 客户端接）")
    mcs.add_parser("config", help="打印给 MCP 客户端用的接入配置片段")
    ma = mcs.add_parser("add", help="注册外部 MCP server")
    ma.add_argument("name")
    ma.add_argument("launch", nargs="+",
                    help="启动命令，可带 --env K=V：agenthub mcp add 名字 --env K=V 命令 参数…")
    ml = mcs.add_parser("list", help="已注册的外部 server")
    ml.add_argument("-n", type=int, default=10)
    mr = mcs.add_parser("rm", help="删除一个外部 server")
    mr.add_argument("name")
    mt = mcs.add_parser("tools", help="连上去看外部 server 提供的工具")
    mt.add_argument("name")
    mco = mcs.add_parser("call", help="调外部 server 的一个工具")
    mco.add_argument("name")
    mco.add_argument("tool")
    mco.add_argument("--args-json", dest="args_json",
                     help='参数 JSON 对象，如 \'{"k": "v"}\'')
    mco.add_argument("--timeout", type=int, default=MCP_CALL_TIMEOUT)
    mc.set_defaults(fn=cmd_mcp)

    wfc = sub.add_parser("workflow", help="工作流引擎：new / run / list / status / trace / stop")
    wfs = wfc.add_subparsers(dest="wf_act", required=True)
    wn = wfs.add_parser("new", help="生成工作流模板（YAML，含测试⇄修复闭环示例）")
    wn.add_argument("path", nargs="?", default="workflow.yaml")
    wn.add_argument("--force", action="store_true", help="覆盖已存在的文件")
    wn.set_defaults(fn=cmd_workflow)
    wr = wfs.add_parser("run", help="执行工作流（先 --dry-run 看计划；--resume 从断点续跑）")
    wr.add_argument("file", help="工作流 YAML 路径")
    wr.add_argument("--resume", action="store_true", help="续跑最近一次 stopped/failed 的运行")
    wr.add_argument("--dry-run", action="store_true", help="只看执行计划，不派工")
    wr.add_argument("--project", help="项目名（默认取工作流 name）")
    wr.add_argument("--cwd", help="项目工作目录（默认当前目录，或工作流里的 project_dir）")
    wr.add_argument("--mem", help="所有步骤统一注入的共享记忆查询词")
    wr.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    wr.add_argument("--write", action="store_true", help="codex 步骤放开 workspace-write")
    wr.add_argument("--yolo", action="store_true", help="claude 步骤跳过权限确认")
    wr.add_argument("--model", help="覆盖各 CLI Agent 的默认模型")
    wr.add_argument("--no-proxy", action="store_true", help="不自动注入本机代理")
    wr.add_argument("--ticket", help="关联工单 id；跑完自动写 ticket report")
    wr.add_argument("--strict-contract", action="store_true",
                    help="缺 objective/output_schema/acceptance 的步骤直接拒派")
    wr.add_argument("--budget-seconds", type=float, help="整次运行的时间预算（秒）")
    wr.add_argument("--budget-tokens", type=int, help="整次运行的 token 预算（尽力统计）")
    wr.add_argument("--max-parallel", type=int, help="单波最多并行几步（默认 8）")
    wr.add_argument("--max-depth", type=int, help="递归派工最大层数（默认 5）")
    wr.set_defaults(fn=cmd_workflow)
    wl = wfs.add_parser("list", help="列出项目与运行记录")
    wl.add_argument("-n", type=int, default=10, help="每个项目最多显示几条运行")
    wl.set_defaults(fn=cmd_workflow)
    wst = wfs.add_parser("status", help="看某次运行每个步骤的状态")
    wst.add_argument("project", help="项目名（workflow list 里看）")
    wst.add_argument("--run", help="run_id（默认最近一次）")
    wst.set_defaults(fn=cmd_workflow)
    wt = wfs.add_parser("trace", help="看某次运行的事件流水（事件日志 + trace.jsonl）")
    wt.add_argument("project")
    wt.add_argument("--run", help="run_id（默认最近一次）")
    wt.add_argument("--json", action="store_true", help="输出 JSON")
    wt.set_defaults(fn=cmd_workflow)
    wsp = wfs.add_parser("stop", help="请求停止运行中的工作流（写 STOP，正在跑的步骤会跑完）")
    wsp.add_argument("project")
    wsp.set_defaults(fn=cmd_workflow)

    pj = sub.add_parser("project", help="项目目录：new 建项目并放工作流模板 / list 看项目")
    pjs = pj.add_subparsers(dest="pj_act", required=True)
    pn = pjs.add_parser("new", help="建 ~/.agenthub/projects/<名>/，放一份工作流模板")
    pn.add_argument("name")
    pn.set_defaults(fn=cmd_project)
    pl = pjs.add_parser("list", help="列出项目与运行记录（同 workflow list）")
    pl.add_argument("-n", type=int, default=10)
    pl.set_defaults(fn=cmd_project)

    lg = sub.add_parser("log", help="派工记录（--show ID 看完整记录）")
    lg.add_argument("-n", type=int, default=10)
    lg.add_argument("--show", type=int)
    lg.set_defaults(fn=cmd_log)

    args = ap.parse_args()
    if args.cmd == "mcp":
        cmd_mcp(None, args)     # mcp 命令组全程不碰数据库
        return
    if args.cmd == "console":
        cmd_console(None, args)  # 控制台每个请求自连数据库
        return
    con = connect()
    try:
        # 索引为空时自动做一次原生记忆同步，保证开箱能查
        if args.cmd == "mem" and args.sub in ("search", "context", "list", "stats"):
            if con.execute("select count(*) c from notes").fetchone()["c"] == 0:
                cmd_mem_sync(con, args)
        args.fn(con, args)
    finally:
        con.close()


if __name__ == "__main__":
    main()
