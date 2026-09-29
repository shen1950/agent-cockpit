# Agent 驾驶舱：国内 AI Agent 多开 / 额度过期提醒 / 本地反代 一体机
# 单文件、零依赖（Python 3.10+ stdlib）。启动后自动打开浏览器仪表盘 http://127.0.0.1:8787
# 多开原理：Electron 系 --user-data-dir 隔离实例；CLI 系 CODEX_HOME / CLAUDE_CONFIG_DIR 隔离账号
import json
import os
import shutil
import subprocess
import threading
import time
import urllib.error
import urllib.request
import webbrowser
import zipfile
from datetime import date, datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

APP_NAME = "Agent 驾驶舱"
PORT = 8787
BASE_DIR = Path.home() / ".agent-cockpit"
INSTANCES_DIR = BASE_DIR / "instances"
BACKUP_DIR = BASE_DIR / "backups"
CONFIG_FILE = BASE_DIR / "config.json"

LOCAL = Path(os.environ.get("LOCALAPPDATA", ""))
NPM_GLOBAL = Path("D:/npm-global")

APPS = {
    "qoder": {"name": "Qoder", "kind": "electron",
              "exe": [LOCAL / "Programs/Qoder/Qoder.exe",
                      Path("D:/Artificial_Intelligence/Qoder/Qoder.exe")]},
    "qoder-cn": {"name": "Qoder CN", "kind": "electron",
                 "exe": [Path("D:/Artificial_Intelligence/Qoder CN/Qoder CN.exe")]},
    "qoder-cn-ide": {"name": "Qoder CN IDE", "kind": "electron",
                     "exe": [Path("D:/Artificial_Intelligence/Qoder CN IDE/Qoder CN IDE.exe")]},
    "workbuddy": {"name": "WorkBuddy", "kind": "electron",
                  "exe": [Path("D:/LenovoSoftstore/Install/WorkBuddy/WorkBuddy.exe")]},
    "traework-cn": {"name": "TraeWork CN", "kind": "electron",
                    "exe": [Path("D:/Artificial_Intelligence/TRAE SOLO CN/TRAE SOLO CN.exe")]},
    "zcode": {"name": "ZCode", "kind": "electron",
              "exe": [Path("D:/Artificial_Intelligence/ZCode/ZCode.exe")]},
    "codex": {"name": "Codex CLI", "kind": "cli", "exe": [NPM_GLOBAL / "codex.cmd"],
              "env": "CODEX_HOME"},
    "claude": {"name": "Claude Code CLI", "kind": "cli", "exe": [NPM_GLOBAL / "claude.cmd"],
               "env": "CLAUDE_CONFIG_DIR"},
}

DETACHED = 0x00000008 | 0x00000200  # DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
NO_WINDOW = 0x08000000  # CREATE_NO_WINDOW：pythonw 下调控制台程序不弹黑窗
ALERT_WARN_DAYS = 30
ALERT_DANGER_DAYS = 7

HOME = Path.home()
# 各 Agent 默认档案目录（换号模式把该目录改成指向实例的 junction）
PROFILE_DIRS = {
    "qoder": HOME / ".qoder", "qoder-cn": HOME / ".qoder-cn",
    "qoder-cn-ide": HOME / ".qoder-cn-ide", "workbuddy": HOME / ".workbuddy",
    "traework-cn": HOME / ".trae-cn", "zcode": HOME / ".zcode",
    "codex": HOME / ".codex", "claude": HOME / ".claude",
}
CLEAN_DIRS = {"Cache", "Code Cache", "GPUCache", "logs", "Crashpad", "DawnCache",
              "DawnGraphiteCache", "DawnWebGPUCache", "ShaderCache", "Service Worker",
              "CachedData", "CachedExtensionVSIXs", "CachedProfilesData", "tmp",
              "CrashFeedback", "Backups"}
# 已知会后台持有 profile 目录句柄的进程（codex 沙箱/桌面版），按 Agent 配置
LOCK_PROCS = {"chatgpt.exe", "codex.exe"}
LOCKERS_BY_APP = {"codex": LOCK_PROCS, "qoder": LOCK_PROCS}


def resolve_exe(app):
    for p in app["exe"]:
        if Path(p).exists():
            return Path(p)
    return None


def tasklist_snapshot():
    out = {}
    try:
        r = subprocess.run(["tasklist", "/fo", "csv", "/nh"], capture_output=True,
                           timeout=10, creationflags=NO_WINDOW)
        for line in r.stdout.decode("gbk", errors="replace").splitlines():
            parts = [x.strip('"') for x in line.split('","')]
            if len(parts) >= 2:
                try:
                    out[int(parts[1])] = parts[0]
                except ValueError:
                    pass
    except Exception:
        pass
    return out


def pid_alive(pid):
    return bool(pid) and pid in tasklist_snapshot()


def instance_pids(app, meta):
    """Electron 实例级进程：按命令行里的 --user-data-dir=<实例目录> 精确匹配"""
    exe_name = Path(app["exe"][0]).name
    data_dir = str(Path(meta["data_dir"])).lower()
    ps = (f"Get-CimInstance Win32_Process -Filter \"Name='{exe_name}'\" | "
          f"ForEach-Object {{ \"$($_.ProcessId)|$($_.CommandLine)\" }}")
    try:
        r = subprocess.run(["powershell", "-NoProfile", "-Command", ps],
                           capture_output=True, timeout=20, creationflags=NO_WINDOW)
    except Exception:
        return []
    raw = r.stdout
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        text = raw.decode("gbk", errors="replace")
    pids = []
    for line in text.splitlines():
        pid, _, cmd = line.partition("|")
        if cmd and data_dir in cmd.lower():
            try:
                pids.append(int(pid))
            except ValueError:
                pass
    return pids


def days_left(expires_on):
    if not expires_on:
        return None
    try:
        return (date.fromisoformat(expires_on) - date.today()).days
    except ValueError:
        return None


# ---------------- 存储 ----------------
def load_instances(app_id):
    out = []
    d = INSTANCES_DIR / app_id
    if d.exists():
        for f in sorted(d.glob("*.json")):
            try:
                meta = json.loads(f.read_text(encoding="utf-8"))
                meta["_file"] = str(f)
                out.append(meta)
            except Exception:
                pass
    return out


def save_instance(app_id, meta):
    d = INSTANCES_DIR / app_id
    d.mkdir(parents=True, exist_ok=True)
    f = d / (meta["name"] + ".json")
    f.write_text(json.dumps(meta, ensure_ascii=False, indent=2), encoding="utf-8")
    meta["_file"] = str(f)
    return meta


def load_config():
    if CONFIG_FILE.exists():
        try:
            return json.loads(CONFIG_FILE.read_text(encoding="utf-8"))
        except Exception:
            pass
    return {"proxy": {"upstream": "https://api.openai.com", "strategy": "round-robin"}}


def save_config(cfg):
    BASE_DIR.mkdir(parents=True, exist_ok=True)
    CONFIG_FILE.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")


# ---------------- 反代 ----------------
class ProxyState:
    def __init__(self):
        self.server = None
        self.thread = None
        self.logs = []
        self.idx = 0
        self.cfg = load_config().get("proxy", {})

    @property
    def running(self):
        return self.server is not None

    def pool(self):
        pool = []
        for app_id, app in APPS.items():
            for meta in load_instances(app_id):
                tok = (meta.get("token") or "").strip()
                if tok:
                    pool.append((f"{app['name']}/{meta['name']}", tok))
        return pool

    def start(self, port):
        if self.running:
            return False, "反代已在运行"
        state = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _forward(self, method):
                n = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(n) if n else None
                pool = state.pool()
                if not pool:
                    self._reply(502, b'{"error":"no tokens in pool"}', "application/json")
                    state._log(method, self.path, None, 502, "无可用 token")
                    return
                label, token = pool[state.idx % len(pool)]
                state.idx += 1
                model = None
                if body:
                    try:
                        model = json.loads(body).get("model")
                    except Exception:
                        pass
                hdrs = {k: v for k, v in self.headers.items()
                        if k.lower() not in ("host", "content-length", "authorization",
                                             "connection", "accept-encoding")}
                hdrs["Authorization"] = f"Bearer {token}"
                url = state.cfg.get("upstream", "").rstrip("/") + self.path
                req = urllib.request.Request(url, data=body, headers=hdrs, method=method)
                try:
                    with urllib.request.urlopen(req, timeout=300) as resp:
                        rbody, status = resp.read(), resp.status
                        ctype = resp.headers.get("Content-Type", "application/json")
                except urllib.error.HTTPError as e:
                    rbody, status = e.read(), e.code
                    ctype = e.headers.get("Content-Type", "application/json")
                except Exception as e:
                    rbody, status = json.dumps({"error": str(e)}).encode(), 502
                    ctype = "application/json"
                state._log(method, self.path, model, status, label)
                self._reply(status, rbody, ctype)

            def _reply(self, status, body, ctype):
                self.send_response(status)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = lambda self: self._forward(
                self.command)

        try:
            srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
        except OSError as e:
            return False, f"端口 {port} 占用：{e}"
        srv.daemon_threads = True
        self.server = srv
        self.thread = threading.Thread(target=srv.serve_forever, daemon=True)
        self.thread.start()
        return True, f"反代已启动 127.0.0.1:{port}"

    def stop(self):
        if not self.running:
            return False, "反代未运行"
        self.server.shutdown()
        self.server.server_close()
        self.server = None
        return True, "反代已停止"

    def _log(self, method, path, model, status, label):
        self.logs.append({"ts": datetime.now().strftime("%H:%M:%S"), "method": method,
                          "path": path[:60], "model": model or "-", "status": status,
                          "account": label})
        self.logs = self.logs[-100:]


PROXY = ProxyState()


# ---------------- 额度自动抓取 ----------------
def fetch_quota(meta):
    """返回 (剩余, 总额, 错误)。支持 openai 官方适配器与通用 quota_url。"""
    hdrs = {"User-Agent": "agent-cockpit/1.0"}
    tok = (meta.get("token") or "").strip()
    if tok:
        hdrs["Authorization"] = f"Bearer {tok}"
    try:
        try:
            extra = json.loads(meta.get("quota_headers") or "{}")
            hdrs.update({k: str(v) for k, v in extra.items()})
        except Exception:
            pass
    except Exception:
        pass
    adapter = (meta.get("quota_adapter") or "").strip()
    url = ("https://api.openai.com/dashboard/billing/credit_grants" if adapter == "openai"
           else (meta.get("quota_url") or "").strip())
    if not url:
        return None, None, "未配置适配器或 quota_url"
    if adapter == "openai" and not tok:
        return None, None, "OpenAI 适配器需要 API Token"
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=hdrs), timeout=15) as r:
            data = json.loads(r.read().decode("utf-8", errors="replace"))
    except Exception as e:
        return None, None, f"抓取失败：{e}"
    if adapter == "openai":
        total, used = data.get("total_granted"), data.get("total_used")
        if not isinstance(total, (int, float)):
            return None, None, "响应缺少 total_granted（key 可能无账单权限）"
        return float(total) - float(used or 0), float(total), None
    node = data
    for key in (meta.get("quota_path") or "").split("."):
        if key and isinstance(node, dict):
            node = node.get(key)
    if not isinstance(node, (int, float)):
        for key in ("remaining", "remaining_credits", "credits_left", "total_available",
                    "grant_remaining"):
            if isinstance(data, dict) and isinstance(data.get(key), (int, float)):
                node = data[key]
                break
    if not isinstance(node, (int, float)):
        return None, None, "响应中未找到数值字段，请设置 quota_path"
    return float(node), None, None


# ---------------- 换号模式（junction，零空间） ----------------
def is_junction(p):
    return os.path.islink(p) or os.path.isjunction(p)


def agent_running(app_id):
    app = APPS[app_id]
    if app["kind"] == "electron":
        snap = tasklist_snapshot()
        exe_name = Path(app["exe"][0]).name.lower()
        return any(nm.lower() == exe_name for nm in snap.values())
    return any(pid_alive(m.get("pid", 0)) for m in load_instances(app_id))


def swap_set(app_id, name, force=False):
    app = APPS[app_id]
    P = PROFILE_DIRS.get(app_id)
    if not P:
        return False, "该 Agent 未登记默认档案目录，不支持换号"
    meta = next((m for m in load_instances(app_id) if m["name"] == name), None)
    if not meta:
        return False, "实例不存在"
    if agent_running(app_id):
        return False, f"请先关闭所有 {app['name']} 窗口/终端再换号"
    snap = tasklist_snapshot()
    wanted = LOCKERS_BY_APP.get(app_id, set())
    lockers = [(p, nm) for p, nm in snap.items() if nm.lower() in wanted]
    if lockers and not force:
        names = "、".join(sorted({nm for _, nm in lockers}))
        return False, f"目录被后台进程锁定：{names}（确认后强切会结束它们）"
    for p, _ in lockers:
        subprocess.run(["taskkill", "/pid", str(p), "/t", "/f"], capture_output=True,
                       creationflags=NO_WINDOW)
    I = Path(meta["data_dir"])
    cfg = load_config()
    swaps = cfg.setdefault("swaps", {})
    cur = swaps.get(app_id)
    if cur and cur.get("instance") == name:
        return True, "该实例已是当前账号"
    if is_junction(P):
        os.rmdir(P)  # 只删 junction，不碰目标
    orig = None
    if P.exists():
        orig = str(P) + ".cockpit-original"
        if Path(orig).exists():
            return False, "已存在 .cockpit-original 备份目录，请先「还原默认」再换号"
        try:
            os.rename(P, orig)
        except PermissionError:
            return False, (f"{P} 被其他进程锁定（可能是官方客户端后台驻留），"
                           f"请从托盘/任务管理器完全退出后重试")
    r = subprocess.run(["cmd", "/c", "mklink", "/J", str(P), str(I)],
                       capture_output=True, creationflags=NO_WINDOW)
    if not is_junction(P):
        return False, "创建 junction 失败：" + r.stdout.decode("gbk", errors="replace")
    swaps[app_id] = {"instance": name, "original": orig}
    save_config(cfg)
    return True, f"已换号：{app['name']} 默认入口 → 「{name}」，零额外空间"


def swap_clear(app_id):
    app = APPS[app_id]
    P = PROFILE_DIRS.get(app_id)
    if not P:
        return False, "该 Agent 未登记默认档案目录"
    if agent_running(app_id):
        return False, f"请先关闭所有 {app['name']} 窗口/终端再还原"
    cfg = load_config()
    swaps = cfg.setdefault("swaps", {})
    cur = swaps.get(app_id)
    if not cur:
        return True, "当前未处于换号状态"
    if is_junction(P):
        os.rmdir(P)
    orig = cur.get("original")
    if orig and Path(orig).exists():
        os.rename(orig, P)
    swaps.pop(app_id, None)
    save_config(cfg)
    return True, f"已还原 {app['name']} 默认档案"


# ---------------- 空间治理 ----------------
def dir_size(p):
    total = 0
    for f in Path(p).rglob("*"):
        try:
            if f.is_file():
                total += f.stat().st_size
        except OSError:
            pass
    return total


def clean_cache(app_id, name):
    meta = next((m for m in load_instances(app_id) if m["name"] == name), None)
    if not meta:
        return False, "实例不存在"
    if agent_running(app_id):
        return False, "请先关闭该实例窗口再清理"
    d = Path(meta["data_dir"])
    freed = 0
    for sub in list(d.iterdir()):
        if sub.is_dir() and sub.name in CLEAN_DIRS:
            freed += dir_size(sub)
            shutil.rmtree(sub, ignore_errors=True)
        elif sub.is_file() and sub.suffix == ".log":
            freed += sub.stat().st_size
            sub.unlink(missing_ok=True)
    return True, f"已清理 {freed // 1024 // 1024} MB 缓存（登录态与设置保留）"


# ---------------- 业务动作 ----------------
def build_state():
    snap = tasklist_snapshot()
    cfg = load_config()
    swaps = cfg.get("swaps", {})
    apps, instances, alerts = [], [], []
    n_running = 0
    for app_id, app in APPS.items():
        exe = resolve_exe(app)
        apps.append({"id": app_id, "name": app["name"], "kind": app["kind"],
                     "installed": bool(exe),
                     "swapped": swaps.get(app_id, {}).get("instance"),
                     "swap_ok": app_id in PROFILE_DIRS})
        exe_name = Path(app["exe"][0]).name.lower()
        procs = sum(1 for _, nm in snap.items() if nm.lower() == exe_name) if exe else 0
        for meta in load_instances(app_id):
            dl = days_left(meta.get("expires_on"))
            running = (procs > 0) if app["kind"] == "electron" else (meta.get("pid", 0) in snap and meta.get("pid", 0) != 0)
            if running:
                n_running += 1
            left = meta.get("credits_left")
            if dl is not None and dl <= ALERT_WARN_DAYS and (left is None or left > 0):
                alerts.append({"app": app["name"], "name": meta["name"], "days": dl,
                               "left": left,
                               "level": "danger" if dl <= ALERT_DANGER_DAYS else "warn"})
            instances.append({
                "app": app_id, "app_name": app["name"], "kind": app["kind"],
                "name": meta["name"], "workdir": meta.get("workdir", ""),
                "created": time.strftime("%Y-%m-%d", time.localtime(meta.get("created", 0))),
                "running": running, "procs": procs,
                "swapped": swaps.get(app_id, {}).get("instance") == meta["name"],
                "quota": {"total": meta.get("credits_total"), "left": left,
                          "expires_on": meta.get("expires_on"), "days_left": dl,
                          "has_token": bool((meta.get("token") or "").strip()),
                          "quota_url": bool((meta.get("quota_url") or "").strip()),
                          "adapter": meta.get("quota_adapter") or ""},
            })
    alerts.sort(key=lambda a: a["days"])
    procs_rows = []
    for app_id, app in APPS.items():
        exe = resolve_exe(app)
        if not exe:
            continue
        if app["kind"] == "electron":
            pids = sorted(p for p, nm in snap.items() if nm.lower() == exe.name.lower())
            if pids:
                procs_rows.append({"app": app["name"],
                                   "detail": f"{len(pids)} 个进程（含渲染/GPU 子进程）",
                                   "pids": pids[:6]})
        else:
            for meta in load_instances(app_id):
                if meta.get("pid", 0) and meta.get("pid", 0) in snap:
                    procs_rows.append({"app": f"{app['name']} / {meta['name']}",
                                       "detail": meta.get("workdir", ""),
                                       "pids": [meta["pid"]]})
    return {"apps": apps, "instances": instances, "procs": procs_rows, "alerts": alerts,
            "stats": {"instances": len(instances), "running": n_running},
            "proxy": {"running": PROXY.running, "port": PORT + 1,
                      "upstream": PROXY.cfg.get("upstream", ""),
                      "strategy": PROXY.cfg.get("strategy", "round-robin"),
                      "pool": [p[0] for p in PROXY.pool()],
                      "logs": PROXY.logs[-30:][::-1]},
            "now": datetime.now().strftime("%H:%M:%S")}


def do_action(payload):
    act = payload.get("action")
    app_id = payload.get("app")
    name = payload.get("name")

    def meta_of():
        for m in load_instances(app_id):
            if m["name"] == name:
                return m
        return None

    if act == "new_instance":
        app = APPS[app_id]
        if not resolve_exe(app):
            return {"ok": False, "msg": f"未找到 {app['name']} 可执行文件"}
        existing = {m["name"] for m in load_instances(app_id)}
        i = 1
        while f"账号{i}" in existing:
            i += 1
        nm = f"账号{i}"
        data_dir = INSTANCES_DIR / app_id / nm
        data_dir.mkdir(parents=True, exist_ok=True)
        save_instance(app_id, {"name": nm, "app": app_id,
                               "workdir": payload.get("workdir") or str(Path.home()),
                               "data_dir": str(data_dir), "created": time.time(), "pid": 0})
        return {"ok": True, "msg": f"已新建 {app['name']}「{nm}」，首次启动请登录对应账号"}

    if act == "launch":
        meta = meta_of()
        if not meta:
            return {"ok": False, "msg": "实例不存在"}
        app = APPS[app_id]
        exe = resolve_exe(app)
        if not exe:
            return {"ok": False, "msg": f"未找到 {app['name']} 可执行文件"}
        workdir = meta.get("workdir") or str(Path.home())
        if not Path(workdir).exists():
            workdir = str(Path.home())
        data_dir = Path(meta["data_dir"])
        data_dir.mkdir(parents=True, exist_ok=True)
        if app["kind"] == "electron":
            # 防串号自检：实例目录的产品标识须与当前 Agent 同族，否则官方客户端会报
            # "产物内联的产品是 qoder，当前运行身份是 qoder-cn" 并拒绝启动
            if app_id.startswith("qoder"):
                st = data_dir / ".qoder-app-status.json"
                if st.exists():
                    try:
                        prod = json.loads(st.read_text(encoding="utf-8",
                                                       errors="replace")).get("product")
                    except Exception:
                        prod = None
                    want = "qoder-cn" if app_id.startswith("qoder-cn") else "qoder"
                    if prod and prod != want:
                        return {"ok": False,
                                "msg": f"实例目录身份是 {prod}，与 {app['name']} 不符，"
                                       f"混用会触发官方自检报错，请删除该实例重建"}
            subprocess.Popen([str(exe), f"--user-data-dir={data_dir}", workdir],
                             cwd=str(exe.parent), close_fds=True, creationflags=DETACHED)
            return {"ok": True, "msg": f"已启动 {app['name']}「{name}」"}
        env = dict(os.environ)
        env[app["env"]] = str(data_dir)
        inner = f'cd /d "{workdir}" && set "{app["env"]}={data_dir}" && "{exe}"'
        subprocess.Popen(["cmd", "/c", "start", "cmd", "/k", inner], cwd=workdir, env=env,
                         close_fds=True, creationflags=DETACHED | NO_WINDOW)
        return {"ok": True, "msg": f"已在新终端启动 {app['name']}「{name}」"}

    if act == "kill":
        meta = meta_of()
        app = APPS[app_id]
        if app["kind"] == "cli":
            pid = (meta or {}).get("pid", 0)
            if pid_alive(pid):
                subprocess.run(["taskkill", "/pid", str(pid), "/t", "/f"], capture_output=True,
                               creationflags=NO_WINDOW)
                return {"ok": True, "msg": f"已结束 PID {pid}"}
            return {"ok": False, "msg": "该 CLI 实例未在运行"}
        if not meta:
            return {"ok": False, "msg": "实例不存在"}
        pids = instance_pids(app, meta)
        if not pids:
            return {"ok": False, "msg": "该实例没有运行中的窗口（整个程序级结束请用进程表「结束全部」）"}
        for p in pids:
            subprocess.run(["taskkill", "/pid", str(p), "/t", "/f"], capture_output=True,
                           creationflags=NO_WINDOW)
        return {"ok": True, "msg": f"已结束「{name}」的 {len(pids)} 个窗口进程，不影响其他实例"}

    if act == "kill_pids":
        for pid in payload.get("pids", []):
            subprocess.run(["taskkill", "/pid", str(pid), "/t", "/f"], capture_output=True,
                               creationflags=NO_WINDOW)
        return {"ok": True, "msg": f"已结束进程 {payload.get('pids')}"}

    if act == "delete":
        meta = meta_of()
        if not meta:
            return {"ok": False, "msg": "实例不存在"}
        shutil.rmtree(meta["data_dir"], ignore_errors=True)
        Path(meta["_file"]).unlink(missing_ok=True)
        return {"ok": True, "msg": f"已删除实例「{name}」"}

    if act in ("backup", "restore"):
        meta = meta_of()
        if not meta:
            return {"ok": False, "msg": "实例不存在"}
        data_dir = Path(meta["data_dir"])
        skip = {"Cache", "Code Cache", "GPUCache", "logs", "Crashpad", "DawnCache",
                "ShaderCache", "Service Worker"}
        if act == "backup":
            ts = time.strftime("%Y%m%d-%H%M%S")
            zp = BACKUP_DIR / f"{app_id}__{name}__{ts}.zip"
            BACKUP_DIR.mkdir(parents=True, exist_ok=True)
            with zipfile.ZipFile(zp, "w", zipfile.ZIP_DEFLATED) as zf:
                for f in data_dir.rglob("*"):
                    if f.is_file() and not any(p in skip for p in f.parts):
                        zf.write(f, f.relative_to(data_dir))
            return {"ok": True, "msg": f"已备份 {zp.name}（{zp.stat().st_size // 1024} KB）"}
        cands = sorted(BACKUP_DIR.glob(f"{app_id}__{name}__*.zip"))
        if not cands:
            return {"ok": False, "msg": "没有该实例的备份"}
        if data_dir.exists():
            shutil.rmtree(data_dir)
        data_dir.mkdir(parents=True)
        with zipfile.ZipFile(cands[-1]) as zf:
            zf.extractall(data_dir)
        return {"ok": True, "msg": f"已从 {cands[-1].name} 恢复（请先关闭该实例窗口）"}

    if act == "open_dir":
        meta = meta_of()
        if not meta:
            return {"ok": False, "msg": "实例不存在"}
        d = Path(meta["data_dir"])
        d.mkdir(parents=True, exist_ok=True)
        os.startfile(str(d))
        return {"ok": True, "msg": "已打开数据目录"}

    if act == "save_meta":
        meta = meta_of()
        if not meta:
            return {"ok": False, "msg": "实例不存在"}
        for key in ("workdir", "credits_total", "credits_left", "expires_on", "token",
                    "quota_url", "quota_headers", "quota_path", "quota_adapter"):
            if key in payload:
                v = payload[key]
                if key in ("credits_total", "credits_left"):
                    v = float(v) if str(v).strip() not in ("", "None") else None
                meta[key] = v
        save_instance(app_id, meta)
        return {"ok": True, "msg": "已保存"}

    if act == "fetch_quota":
        meta = meta_of()
        if not meta:
            return {"ok": False, "msg": "实例不存在"}
        left, total, err = fetch_quota(meta)
        if err:
            return {"ok": False, "msg": err}
        meta["credits_left"] = left
        if total is not None:
            meta["credits_total"] = total
        save_instance(app_id, meta)
        return {"ok": True, "msg": f"已更新剩余：{left:g}" + (f" / 总额 {total:g}" if total else "")}

    if act == "swap_set":
        ok, msg = swap_set(app_id, name, force=bool(payload.get("force")))
        return {"ok": ok, "msg": msg}

    if act == "swap_clear":
        ok, msg = swap_clear(app_id)
        return {"ok": ok, "msg": msg}

    if act == "calc_size":
        meta = meta_of()
        if not meta:
            return {"ok": False, "msg": "实例不存在"}
        mb = dir_size(meta["data_dir"]) // 1024 // 1024
        return {"ok": True, "msg": f"「{name}」占用 {mb} MB"}

    if act == "clean_cache":
        ok, msg = clean_cache(app_id, name)
        return {"ok": ok, "msg": msg}

    if act == "proxy_start":
        PROXY.cfg.update({k: payload[k] for k in ("upstream", "strategy") if payload.get(k)})
        save_config({"proxy": PROXY.cfg})
        ok, msg = PROXY.start(PORT + 1)
        return {"ok": ok, "msg": msg}

    if act == "proxy_stop":
        ok, msg = PROXY.stop()
        return {"ok": ok, "msg": msg}

    return {"ok": False, "msg": f"未知动作 {act}"}


# ---------------- HTTP ----------------
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _send(self, code, body, ctype):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/api/state":
            body = json.dumps(build_state(), ensure_ascii=False).encode("utf-8")
            self._send(200, body, "application/json; charset=utf-8")
        elif self.path.startswith("/api/meta"):
            qs = parse_qs(urlparse(self.path).query)
            app_id = qs.get("app", [""])[0]
            name = qs.get("name", [""])[0]
            meta = next((m for m in load_instances(app_id) if m["name"] == name), {})
            body = json.dumps({k: meta.get(k, "") or "" for k in
                               ("token", "quota_url", "quota_path", "quota_headers",
                                "quota_adapter")},
                              ensure_ascii=False).encode("utf-8")
            self._send(200, body, "application/json; charset=utf-8")
        elif self.path in ("/", "/index.html"):
            self._send(200, PAGE.encode("utf-8"), "text/html; charset=utf-8")
        else:
            self._send(404, b"not found", "text/plain")

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n)
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            text = raw.decode("gbk", errors="replace")  # Windows curl 命令行常按 GBK 传参
        try:
            payload = json.loads(text)
        except Exception:
            payload = {}
        try:
            res = do_action(payload)
        except Exception as e:
            res = {"ok": False, "msg": f"内部错误：{type(e).__name__}: {e}"}
        self._send(200, json.dumps(res, ensure_ascii=False).encode("utf-8"),
                   "application/json; charset=utf-8")


PAGE = r"""<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Agent 驾驶舱</title>
<style>
:root{
  --bg:#0C0E14; --panel:#141822; --panel2:#1A202C; --line:rgba(255,255,255,.07);
  --line2:rgba(255,255,255,.13); --text:#E9E7E2; --muted:#8C93A5; --faint:#5A6172;
  --accent:#C8956C; --accent-hi:#E4B58A; --accent-dim:rgba(200,149,108,.13);
  --ok:#3ECF8E; --warn:#E2B34E; --danger:#E5484D; --r:12px;
  --serif:"Noto Serif SC","Source Han Serif SC","Songti SC",Georgia,"Times New Roman",serif;
  --sans:"Segoe UI","Microsoft YaHei UI","PingFang SC",system-ui,sans-serif;
  --mono:"Cascadia Code",Consolas,"Courier New",monospace;
}
*{box-sizing:border-box;margin:0;padding:0}
html,body{height:100%}
body{background:var(--bg);color:var(--text);font:14px/1.6 var(--sans);
     -webkit-font-smoothing:antialiased}
button{font:inherit;cursor:pointer;border:0;border-radius:8px;background:none;color:inherit}
input,select{font:inherit;color:var(--text);background:var(--panel2);
     border:1px solid var(--line2);border-radius:8px;padding:7px 10px;outline:none}
input:focus,select:focus{border-color:var(--accent)}
::-webkit-scrollbar{width:10px;height:10px}
::-webkit-scrollbar-thumb{background:#2A3140;border-radius:5px;border:3px solid var(--bg)}

header{display:flex;align-items:center;gap:16px;padding:0 24px;height:60px;
  border-bottom:1px solid var(--line)}
header .logo{font:700 20px/1 var(--serif);letter-spacing:.5px}
header .logo b{color:var(--accent)}
header .tag{color:var(--faint);font-size:12px;border-left:1px solid var(--line2);
  padding-left:16px}
header .sp{flex:1}
.chip{display:inline-flex;align-items:center;gap:6px;padding:4px 10px;border-radius:999px;
  background:var(--panel);border:1px solid var(--line);color:var(--muted);font-size:12px}
.chip .dot{width:7px;height:7px;border-radius:50%;background:var(--faint)}
.chip.on .dot{background:var(--ok)}
.chip.on{color:var(--text)}

#alerts{display:none;margin:16px 24px 0;padding:12px 16px;border-radius:var(--r);
  border:1px solid rgba(229,72,77,.35);background:rgba(229,72,77,.08)}
#alerts.warn{border-color:rgba(226,179,78,.35);background:rgba(226,179,78,.07)}
#alerts .t{font:700 13px var(--sans);margin-bottom:4px}
#alerts ul{list-style:none;display:flex;flex-wrap:wrap;gap:8px}
#alerts li{font-size:12.5px;color:var(--text)}
#alerts li b{color:var(--danger)} #alerts.warn li b{color:var(--warn)}

main{display:grid;grid-template-columns:224px 1fr;gap:24px;padding:24px;align-items:start}
nav{position:sticky;top:24px}
nav .h{font:700 11px var(--sans);letter-spacing:2px;color:var(--faint);margin:0 0 8px 12px}
nav button{display:flex;align-items:center;gap:10px;width:100%;padding:9px 12px;
  border-radius:10px;color:var(--muted);font-size:13.5px;text-align:left}
nav button .dot{width:7px;height:7px;border-radius:50%;background:var(--faint);flex:none}
nav button.inst .dot{background:var(--ok)}
nav button .n{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
nav button .c{font-size:11px;color:var(--faint);background:var(--panel);
  border:1px solid var(--line);border-radius:999px;padding:1px 8px}
nav button:hover{background:var(--panel);color:var(--text)}
nav button.sel{background:var(--panel2);color:var(--text);
  box-shadow:inset 2px 0 0 var(--accent)}
nav .foot{margin:16px 12px 0;font-size:11.5px;color:var(--faint);line-height:1.7}

.toolbar{display:flex;align-items:center;gap:12px;margin-bottom:16px}
.toolbar h2{font:700 19px/1.2 var(--serif)}
.toolbar .sp{flex:1}
.btn{padding:8px 16px;border-radius:9px;font-size:13px;font-weight:600;
  background:var(--panel2);border:1px solid var(--line2);color:var(--text)}
.btn:hover{border-color:var(--accent);color:var(--accent-hi)}
.btn.primary{background:var(--accent);border-color:var(--accent);color:#14100B}
.btn.primary:hover{background:var(--accent-hi);color:#14100B}
.btn.danger{color:var(--danger);border-color:rgba(229,72,77,.4)}
.btn.danger:hover{background:rgba(229,72,77,.12);border-color:var(--danger);color:var(--danger)}
.btn.sm{padding:5px 11px;font-size:12px;font-weight:500;border-radius:7px}

.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(312px,1fr));gap:16px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:var(--r);
  padding:16px;display:flex;flex-direction:column;gap:12px}
.card .top{display:flex;align-items:flex-start;gap:8px}
.card .nm{font-size:15px;font-weight:650}
.card .ap{font-size:12px;color:var(--muted)}
.card .st{margin-left:auto;display:flex;align-items:center;gap:6px;font-size:12px;
  color:var(--faint)}
.card .st .dot{width:8px;height:8px;border-radius:50%;background:var(--faint)}
.card.run .st{color:var(--ok)} .card.run .st .dot{background:var(--ok);
  box-shadow:0 0 0 3px rgba(62,207,142,.15)}
.card .quota{background:var(--panel2);border:1px solid var(--line);border-radius:10px;
  padding:10px 12px;display:flex;flex-direction:column;gap:8px}
.card .qrow{display:flex;align-items:baseline;gap:8px;font-size:12.5px;color:var(--muted)}
.card .qrow .v{font:650 16px var(--mono);color:var(--text)}
.card .qrow .exp{margin-left:auto;font-size:11.5px;padding:2px 9px;border-radius:999px;
  background:var(--panel);border:1px solid var(--line)}
.card .exp.danger{color:var(--danger);border-color:rgba(229,72,77,.45)}
.card .exp.warn{color:var(--warn);border-color:rgba(226,179,78,.45)}
.bar{height:5px;border-radius:3px;background:#242B39;overflow:hidden}
.bar i{display:block;height:100%;background:var(--accent);border-radius:3px}
.card .acts{display:flex;gap:8px;flex-wrap:wrap}
.empty{color:var(--faint);font-size:12.5px;padding:2px 0}

section{background:var(--panel);border:1px solid var(--line);border-radius:var(--r);
  padding:16px;margin-top:24px}
section h3{font:700 15px var(--serif);margin-bottom:12px;display:flex;align-items:center;
  gap:10px}
section h3 .sub{font:400 12px var(--sans);color:var(--faint)}
table{width:100%;border-collapse:collapse;font-size:12.5px}
th{color:var(--faint);font-weight:600;text-align:left;padding:6px 10px;
  border-bottom:1px solid var(--line)}
td{padding:7px 10px;border-bottom:1px solid var(--line);color:var(--muted);
  font-family:var(--mono);font-size:12px}
td.txt{font-family:var(--sans);font-size:12.5px;color:var(--text)}
tr:last-child td{border-bottom:0}
.s200{color:var(--ok)} .s400,.s500{color:var(--danger)}

.prow{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-bottom:12px}
.prow label{font-size:12px;color:var(--muted)}
.prow input{width:280px}

#modal{position:fixed;inset:0;background:rgba(6,8,12,.72);display:none;
  align-items:center;justify-content:center;z-index:50}
#modal.show{display:flex}
#modal .box{width:460px;background:var(--panel);border:1px solid var(--line2);
  border-radius:14px;padding:20px 22px}
#modal h4{font:700 16px var(--serif);margin-bottom:14px}
#modal .f{display:grid;grid-template-columns:96px 1fr;gap:10px;align-items:center;
  margin-bottom:10px}
#modal .f label{font-size:12.5px;color:var(--muted)}
#modal .f input{width:100%}
#modal .acts{display:flex;justify-content:flex-end;gap:10px;margin-top:16px}

#toast{position:fixed;right:24px;bottom:24px;display:flex;flex-direction:column;gap:8px;
  z-index:60}
#toast div{background:var(--panel2);border:1px solid var(--line2);border-left:3px solid
  var(--accent);border-radius:10px;padding:10px 16px;font-size:13px;
  box-shadow:0 8px 24px rgba(0,0,0,.45);animation:in .18s ease-out}
#toast div.err{border-left-color:var(--danger)}
@keyframes in{from{transform:translateY(8px);opacity:0}to{transform:none;opacity:1}}
</style>
</head>
<body>
<header>
  <div class="logo">▣ Agent <b>驾驶舱</b></div>
  <div class="tag">多开 · 额度过期提醒 · 本地反代</div>
  <div class="sp"></div>
  <span class="chip" id="c-inst"><span class="dot"></span><span></span></span>
  <span class="chip" id="c-run"><span class="dot"></span><span></span></span>
  <span class="chip" id="c-proxy"><span class="dot"></span><span></span></span>
  <span class="chip" id="c-clock"><span></span></span>
</header>
<div id="alerts"></div>
<main>
  <nav>
    <div class="h">AGENTS</div>
    <div id="applist"></div>
    <div class="foot">实例 = 独立数据目录<br>首次启动需各自登录账号<br>赠送积分请登记过期日</div>
  </nav>
  <div>
    <div class="toolbar">
      <h2 id="cur-title">—</h2>
      <div class="sp"></div>
      <button class="btn primary" onclick="act('new_instance',{app:CUR})">＋ 新建实例</button>
      <button class="btn" onclick="load()">刷新</button>
    </div>
    <div class="cards" id="cards"></div>

    <section>
      <h3>运行中的进程 <span class="sub">每 4 秒自动刷新</span></h3>
      <table><thead><tr><th style="width:220px">Agent</th><th>详情</th>
        <th style="width:150px">PID</th><th style="width:90px"></th></tr></thead>
        <tbody id="procs"></tbody></table>
    </section>

    <section>
      <h3>本地反代 <span class="sub">token 池轮询替换 Authorization · 不常用时保持停止</span></h3>
      <div class="prow">
        <label>上游</label><input id="p-up" placeholder="https://api.openai.com">
        <label>策略</label>
        <select id="p-st"><option value="round-robin">轮询</option></select>
        <button class="btn primary sm" id="p-btn" onclick="proxyToggle()">启动反代</button>
        <span class="chip" id="p-pool"><span></span></span>
      </div>
      <table><thead><tr><th style="width:70px">时间</th><th style="width:60px">方法</th>
        <th>路径</th><th style="width:150px">模型</th><th style="width:60px">状态</th>
        <th style="width:170px">账号</th></tr></thead>
        <tbody id="plogs"></tbody></table>
    </section>
  </div>
</main>

<div id="modal"><div class="box">
  <h4 id="m-title">实例设置</h4>
  <div class="f"><label>工作目录</label><input id="m-workdir"></div>
  <div class="f"><label>总额度</label><input id="m-total" type="number" step="any"></div>
  <div class="f"><label>剩余</label><input id="m-left" type="number" step="any"></div>
  <div class="f"><label>过期日</label><input id="m-exp" type="date"></div>
  <div class="f"><label>API Token</label><input id="m-token" type="password"
    placeholder="用于反代 token 池 / 额度抓取"></div>
  <div class="f"><label>额度适配器</label><select id="m-ad">
    <option value="">无（手动/通用接口）</option>
    <option value="openai">OpenAI 官方 billing</option></select></div>
  <div class="f"><label>额度接口</label><input id="m-qurl" placeholder="可选，返回 JSON 的 URL（适配器优先）"></div>
  <div class="f"><label>取值路径</label><input id="m-qpath" placeholder="如 data.remaining，留空自动猜"></div>
  <div class="acts">
    <button class="btn sm" onclick="closeModal()">取消</button>
    <button class="btn primary sm" onclick="saveModal()">保存</button>
  </div>
</div></div>
<div id="toast"></div>

<script>
let S=null, CUR=null, EDIT=null;
const $=id=>document.getElementById(id);
const esc=s=>String(s??'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));

async function load(){
  S=await (await fetch('/api/state')).json();
  if(!CUR||!S.apps.some(a=>a.id===CUR)) CUR=S.apps[0]?.id;
  render();
}
function render(){
  $('c-inst').lastElementChild.textContent=`实例 ${S.stats.instances}`;
  $('c-run').lastElementChild.textContent=`运行中 ${S.stats.running}`;
  const pc=$('c-proxy'); pc.className='chip'+(S.proxy.running?' on':'');
  pc.lastElementChild.textContent=S.proxy.running?`反代 :${S.proxy.port}`:'反代 停止';
  $('c-clock').lastElementChild.textContent=S.now;

  const al=$('alerts');
  if(S.alerts.length){
    const danger=S.alerts.some(a=>a.level==='danger');
    al.style.display='block'; al.className=danger?'':'warn';
    al.innerHTML=`<div class="t">⏳ ${S.alerts.length} 笔赠送额度临期，请尽快调用</div><ul>`+
      S.alerts.map(a=>`<li>「${esc(a.app)}/${esc(a.name)}」剩余 ${a.left??'—'} · <b>${a.days} 天后过期</b></li>`).join('')+`</ul>`;
    document.title=`(${S.alerts.length} 笔临期) Agent 驾驶舱`;
  }else{al.style.display='none';document.title='Agent 驾驶舱';}

  $('applist').innerHTML=S.apps.map(a=>{
    const n=S.instances.filter(i=>i.app===a.id).length;
    const run=S.instances.some(i=>i.app===a.id&&i.running);
    return `<button class="${a.id===CUR?'sel':''} ${run?'inst':''}" onclick="CUR='${a.id}';render()">
      <span class="dot"></span><span class="n">${esc(a.name)}${a.installed?'':' · 未安装'}</span>
      ${a.swapped?`<span class="c" style="color:var(--accent-hi);border-color:var(--accent)">⇄${esc(a.swapped)}</span>`:''}
      ${n?`<span class="c">${n}</span>`:''}</button>`;}).join('');

  const app=S.apps.find(a=>a.id===CUR);
  $('cur-title').textContent=app?app.name:'—';
  const list=S.instances.filter(i=>i.app===CUR);
  $('cards').innerHTML=list.length?list.map(card).join(''):
    `<div class="empty">暂无实例。点右上「＋ 新建实例」创建第一个账号档案。</div>`;

  $('procs').innerHTML=S.procs.map(p=>`<tr><td class="txt">${esc(p.app)}</td>
    <td class="txt">${esc(p.detail)}</td><td>${p.pids.join(',')}</td>
    <td><button class="btn danger sm" onclick='killAll(${JSON.stringify(p.pids)},"${esc(p.app)}")'>结束全部</button></td></tr>`).join('')
    ||`<tr><td colspan="4" class="txt" style="color:var(--faint)">当前没有运行中的 Agent 进程</td></tr>`;

  $('p-up').value=S.proxy.upstream||'';
  $('p-st').value=S.proxy.strategy||'round-robin';
  $('p-btn').textContent=S.proxy.running?'停止反代':'启动反代';
  $('p-btn').className='btn sm '+(S.proxy.running?'danger':'primary');
  $('p-pool').lastElementChild.textContent=`token 池 ${S.proxy.pool.length}：${S.proxy.pool.join('、')||'空'}`;
  $('plogs').innerHTML=S.proxy.logs.map(l=>`<tr><td>${l.ts}</td><td>${l.method}</td>
    <td>${esc(l.path)}</td><td>${esc(l.model)}</td><td class="s${String(l.status)[0]}00">${l.status}</td>
    <td class="txt">${esc(l.account)}</td></tr>`).join('')
    ||`<tr><td colspan="6" class="txt" style="color:var(--faint)">暂无请求记录</td></tr>`;
}
function card(i){
  const q=i.quota; let quota='';
  const ap=S.apps.find(a=>a.id===i.app);
  if(q.total||q.left||q.expires_on){
    const pct=q.total?Math.max(0,Math.min(100,q.left/q.total*100)):0;
    const exp=q.days_left==null?'':(q.days_left<0?`<span class="exp danger">已过期</span>`:
      q.days_left<=7?`<span class="exp danger">${q.days_left} 天后过期</span>`:
      q.days_left<=30?`<span class="exp warn">${q.days_left} 天后过期</span>`:
      `<span class="exp">${q.days_left} 天后过期</span>`);
    quota=`<div class="quota"><div class="qrow"><span class="v">${q.left??'—'}</span>
      <span>/ ${q.total??'—'} 积分</span>${exp}</div>
      ${q.total?`<div class="bar"><i style="width:${pct}%"></i></div>`:''}</div>`;
  }else quota=`<div class="quota"><div class="qrow empty">未登记额度 · 赠送积分请登记过期日以便提醒</div></div>`;
  const swapBtn=ap&&ap.swap_ok?(i.swapped?
    `<button class="btn danger sm" onclick="act('swap_clear',{app:'${i.app}'})">还原默认</button>`:
    `<button class="btn sm" onclick="swapSet('${i.app}','${esc(i.name)}')">设为当前</button>`):'';
  return `<div class="card ${i.running?'run':''}">
    <div class="top"><div><div class="nm">${esc(i.name)}${i.swapped?' <span style="color:var(--accent-hi);font-size:12px">⇄ 当前账号</span>':''}</div>
      <div class="ap">${esc(i.app_name)} · ${i.kind==='cli'?'CLI':'桌面端'} · 建于 ${i.created}</div></div>
      <div class="st"><span class="dot"></span>${i.running?(i.kind==='cli'?'运行中':`运行中 ×${i.procs}`):'未运行'}</div></div>
    ${quota}
    <div class="acts">
      <button class="btn primary sm" onclick="act('launch',{app:'${i.app}',name:'${esc(i.name)}'})">▶ 启动</button>
      <button class="btn sm" onclick="act('kill',{app:'${i.app}',name:'${esc(i.name)}'})">■ 结束</button>
      ${swapBtn}
      <button class="btn sm" onclick="openModal('${i.app}','${esc(i.name)}')">额度/设置</button>
      ${(q.quota_url||q.adapter)?`<button class="btn sm" onclick="act('fetch_quota',{app:'${i.app}',name:'${esc(i.name)}'})">抓额度</button>`:''}
      <button class="btn sm" onclick="act('calc_size',{app:'${i.app}',name:'${esc(i.name)}'})">占用</button>
      <button class="btn sm" onclick="cleanCache('${i.app}','${esc(i.name)}')">清缓存</button>
      <button class="btn sm" onclick="act('backup',{app:'${i.app}',name:'${esc(i.name)}'})">备份</button>
      <button class="btn sm" onclick="act('restore',{app:'${i.app}',name:'${esc(i.name)}'})">恢复</button>
      <button class="btn sm" onclick="act('open_dir',{app:'${i.app}',name:'${esc(i.name)}'})">目录</button>
      <button class="btn danger sm" onclick="del('${i.app}','${esc(i.name)}')">删除</button>
    </div></div>`;
}
function cleanCache(app,name){
  if(confirm(`清理「${name}」的缓存/日志/扩展缓存？登录态与设置保留。需先关闭该实例。`))
    act('clean_cache',{app,name});
}
async function act(action,extra={}){
  const r=await (await fetch('/api/action',{method:'POST',
    headers:{'Content-Type':'application/json'},
    body:JSON.stringify({action,...extra})})).json();
  toast(r.msg,r.ok); load(); return r;
}
function swapSet(app,name){
  act('swap_set',{app,name}).then(r=>{
    if(!r.ok&&/锁定/.test(r.msg)&&confirm(r.msg+'\n\n确认结束这些后台进程并换号？'))
      act('swap_set',{app,name,force:true});
  });
}
function del(app,name){ if(confirm(`删除实例「${name}」及其数据目录（含登录状态）？不可恢复。`)) act('delete',{app,name}); }
function killAll(pids,app){
  if(confirm(`将结束 ${app} 的全部 ${pids.length}+ 个进程——包括你正在使用的窗口！\n只关单个实例请回到上方卡片用「■ 结束」。\n确认继续？`))
    act('kill_pids',{pids});
}
function proxyToggle(){
  if(S.proxy.running) act('proxy_stop');
  else act('proxy_start',{upstream:$('p-up').value,strategy:$('p-st').value});
}
function openModal(app,name){
  EDIT={app,name};
  const i=S.instances.find(x=>x.app===app&&x.name===name);
  $('m-title').textContent=`${i.app_name} / ${name}`;
  $('m-workdir').value=i.workdir||''; $('m-total').value=i.quota.total??'';
  $('m-left').value=i.quota.left??''; $('m-exp').value=i.quota.expires_on||'';
  $('m-token').value=''; $('m-qurl').value=''; $('m-qpath').value=''; $('m-ad').value='';
  fetch('/api/meta?app='+app+'&name='+encodeURIComponent(name)).then(r=>r.ok?r.json():null)
    .then(m=>{ if(m){$('m-token').value=m.token||'';$('m-qurl').value=m.quota_url||'';
      $('m-qpath').value=m.quota_path||'';$('m-ad').value=m.quota_adapter||'';} });
  $('modal').classList.add('show');
}
function closeModal(){$('modal').classList.remove('show');}
function saveModal(){
  act('save_meta',{app:EDIT.app,name:EDIT.name,workdir:$('m-workdir').value,
    credits_total:$('m-total').value,credits_left:$('m-left').value,
    expires_on:$('m-exp').value,token:$('m-token').value,
    quota_url:$('m-qurl').value,quota_path:$('m-qpath').value,
    quota_adapter:$('m-ad').value});
  closeModal();
}
function toast(msg,ok){
  const d=document.createElement('div'); if(!ok)d.className='err';
  d.textContent=msg; $('toast').appendChild(d);
  setTimeout(()=>d.remove(),3600);
}
load(); setInterval(load,4000);
</script>
</body>
</html>
"""


ICON = Path(__file__).resolve().parent / "cockpit.ico"


def main():
    BASE_DIR.mkdir(parents=True, exist_ok=True)
    INSTANCES_DIR.mkdir(parents=True, exist_ok=True)
    BACKUP_DIR.mkdir(parents=True, exist_ok=True)
    try:
        srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    except OSError:
        import ctypes
        ctypes.windll.user32.MessageBoxW(
            0, "Agent 驾驶舱已在运行，本实例退出。", APP_NAME, 0x40)
        return
    srv.daemon_threads = True
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        # 桌面壳：pywebview(WebView2)，与 Cockpit Tools 的 Tauri 同构——原生壳+Web UI+本地后端
        import webview
    except ImportError:
        webbrowser.open(f"http://127.0.0.1:{PORT}")  # 仅缺桌面壳时降级
        try:
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            srv.shutdown()
        return
    webview.create_window(APP_NAME, f"http://127.0.0.1:{PORT}", width=1280, height=820,
                          min_size=(1024, 680))
    webview.start()  # 阻塞至窗口关闭
    srv.shutdown()


if __name__ == "__main__":
    main()
