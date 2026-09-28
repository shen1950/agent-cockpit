# Agent 驾驶舱：国内 AI Agent 多开管理器（单文件、零依赖，Python 3.10+）
# 原理：Electron 系用 --user-data-dir 隔离实例；CLI 系用 CODEX_HOME / CLAUDE_CONFIG_DIR 隔离账号
import json
import os
import shutil
import subprocess
import sys
import time
import tkinter as tk
import zipfile
from pathlib import Path
from tkinter import filedialog, messagebox, ttk

APP_NAME = "Agent 驾驶舱"
BASE_DIR = Path.home() / ".agent-cockpit"
INSTANCES_DIR = BASE_DIR / "instances"
BACKUP_DIR = BASE_DIR / "backups"

HOME = Path.home()
LOCAL = Path(os.environ.get("LOCALAPPDATA", ""))
NPM_GLOBAL = Path("D:/npm-global")

# ---------- 学术风配色（黑白灰底 + 深藏青/炭灰/学术红，无渐变） ----------
NAVY = "#0F172A"        # 深藏青：标题、侧栏
NAVY_SOFT = "#1E293B"   # 侧栏选中底色
CHARCOAL = "#334155"    # 炭灰：正文
RED = "#B91C1C"         # 学术红：强调/危险
BG = "#F1F5F9"          # 窗口底
CARD = "#FFFFFF"        # 卡片底
BORDER = "#E2E8F0"      # 描边
MUTED = "#64748B"       # 次要文字
GREEN = "#15803D"       # 运行中
HOVER = "#F8FAFC"

FONT = "Microsoft YaHei UI"

APPS = {
    "qoder": {
        "name": "Qoder",
        "kind": "electron",
        "exe": LOCAL / "Programs/Qoder/Qoder.exe",
        "exe_alt": [Path("D:/Artificial_Intelligence/Qoder/Qoder.exe")],
    },
    "qoder-cn": {
        "name": "Qoder CN",
        "kind": "electron",
        "exe": Path("D:/Artificial_Intelligence/Qoder CN/Qoder CN.exe"),
    },
    "qoder-cn-ide": {
        "name": "Qoder CN IDE",
        "kind": "electron",
        "exe": Path("D:/Artificial_Intelligence/Qoder CN IDE/Qoder CN IDE.exe"),
    },
    "workbuddy": {
        "name": "WorkBuddy",
        "kind": "electron",
        "exe": Path("D:/LenovoSoftstore/Install/WorkBuddy/WorkBuddy.exe"),
    },
    "traework-cn": {
        "name": "TraeWork CN (TRAE SOLO)",
        "kind": "electron",
        "exe": Path("D:/Artificial_Intelligence/TRAE SOLO CN/TRAE SOLO CN.exe"),
    },
    "zcode": {
        "name": "ZCode",
        "kind": "electron",
        "exe": Path("D:/Artificial_Intelligence/ZCode/ZCode.exe"),
    },
    "codex": {
        "name": "Codex CLI",
        "kind": "cli",
        "exe": NPM_GLOBAL / "codex.cmd",
        "env": "CODEX_HOME",
    },
    "claude": {
        "name": "Claude Code CLI",
        "kind": "cli",
        "exe": NPM_GLOBAL / "claude.cmd",
        "env": "CLAUDE_CONFIG_DIR",
    },
}

DETACHED = 0x00000008 | 0x00000200  # DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP


def resolve_exe(app: dict):
    exe = Path(app["exe"])
    if exe.exists():
        return exe
    for alt in app.get("exe_alt", []):
        if Path(alt).exists():
            return Path(alt)
    return None


def tasklist_snapshot():
    """返回 {pid: 映像名} 快照"""
    out = {}
    try:
        r = subprocess.run(
            ["tasklist", "/fo", "csv", "/nh"],
            capture_output=True, timeout=10,
        )
        text = r.stdout.decode("gbk", errors="replace")
        for line in text.splitlines():
            parts = [p.strip('"') for p in line.split('","')]
            if len(parts) >= 2:
                try:
                    out[int(parts[1])] = parts[0]
                except ValueError:
                    pass
    except Exception:
        pass
    return out


def pid_alive(pid: int) -> bool:
    if not pid:
        return False
    return pid in tasklist_snapshot()


def card(parent, title=None, subtitle=None):
    """白底描边卡片，返回 (卡片frame, 内容frame)"""
    outer = tk.Frame(parent, bg=CARD, highlightbackground=BORDER,
                     highlightthickness=1, bd=0)
    inner = tk.Frame(outer, bg=CARD)
    inner.pack(fill="both", expand=True, padx=14, pady=10)
    if title:
        row = tk.Frame(inner, bg=CARD)
        row.pack(fill="x", pady=(0, 8))
        tk.Label(row, text=title, bg=CARD, fg=NAVY,
                 font=(FONT, 11, "bold")).pack(side="left")
        if subtitle:
            tk.Label(row, text=subtitle, bg=CARD, fg=MUTED,
                     font=(FONT, 9)).pack(side="left", padx=8, pady=(2, 0))
    return outer, inner


class App:
    def __init__(self, root: tk.Tk):
        self.root = root
        root.title(APP_NAME)
        root.geometry("1080x800")
        root.minsize(940, 640)
        root.configure(bg=BG)

        BASE_DIR.mkdir(parents=True, exist_ok=True)
        INSTANCES_DIR.mkdir(parents=True, exist_ok=True)
        BACKUP_DIR.mkdir(parents=True, exist_ok=True)

        self._setup_styles()

        self.workdir = tk.StringVar(value=str(Path.home()))
        self.status_var = tk.StringVar(value="就绪")
        self.stats_var = tk.StringVar(value="")
        self._app_ids = list(APPS.keys())

        self._build_ui()
        self.refresh_apps()
        self.refresh_all()
        root.after(4000, self._auto_refresh)

    # ---------- 样式 ----------
    def _setup_styles(self):
        style = ttk.Style(self.root)
        try:
            style.theme_use("clam")
        except Exception:
            pass
        style.configure(".", background=BG, foreground=CHARCOAL,
                        font=(FONT, 10), bordercolor=BORDER,
                        focuscolor=NAVY, lightcolor=CARD, darkcolor=CARD)

        # 按钮基类：扁平、无边框
        style.configure("TButton", padding=(14, 7), borderwidth=0,
                        relief="flat", font=(FONT, 10))
        # 主操作：深藏青
        style.configure("Primary.TButton", background=NAVY, foreground="#FFFFFF",
                        font=(FONT, 10, "bold"))
        style.map("Primary.TButton", background=[("active", NAVY_SOFT), ("disabled", BORDER)],
                  foreground=[("disabled", MUTED)])
        # 危险：学术红
        style.configure("Danger.TButton", background=RED, foreground="#FFFFFF")
        style.map("Danger.TButton", background=[("active", "#991B1B")])
        # 启动：绿
        style.configure("Go.TButton", background=GREEN, foreground="#FFFFFF",
                        font=(FONT, 10, "bold"))
        style.map("Go.TButton", background=[("active", "#166534")])
        # 普通工具：浅灰
        style.configure("Tool.TButton", background="#E2E8F0", foreground=CHARCOAL)
        style.map("Tool.TButton", background=[("active", BORDER), ("pressed", "#CBD5E1")])
        # 侧栏深底上的工具按钮
        style.configure("Side.TButton", background=NAVY_SOFT, foreground="#E2E8F0",
                        padding=(10, 5))
        style.map("Side.TButton", background=[("active", "#334155")])

        # 表格
        style.configure("Treeview", background=CARD, fieldbackground=CARD,
                        foreground=CHARCOAL, rowheight=32, borderwidth=0,
                        font=(FONT, 10))
        style.configure("Treeview.Heading", background=HOVER, foreground=MUTED,
                        font=(FONT, 9, "bold"), borderwidth=0, relief="flat",
                        padding=(6, 6))
        style.map("Treeview.Heading", background=[("active", BORDER)])
        style.map("Treeview", background=[("selected", NAVY)],
                  foreground=[("selected", "#FFFFFF")])

        # 输入框
        style.configure("TEntry", fieldbackground=CARD, bordercolor=BORDER,
                        lightcolor=CARD, darkcolor=CARD, padding=6,
                        insertcolor=CHARCOAL)

        # 滚动条：扁平浅灰
        style.configure("Vertical.TScrollbar", background="#CBD5E1",
                        troughcolor=CARD, borderwidth=0, relief="flat", arrowsize=10)
        style.map("Vertical.TScrollbar", background=[("active", MUTED)])

    # ---------- UI ----------
    def _build_ui(self):
        # 顶部：深藏青标题栏
        header = tk.Frame(self.root, bg=NAVY, height=58)
        header.pack(fill="x")
        header.pack_propagate(False)
        tk.Label(header, text="▣ " + APP_NAME, bg=NAVY, fg="#FFFFFF",
                 font=(FONT, 13, "bold")).pack(side="left", padx=16)
        tk.Label(header, text="多开 = 独立数据目录，互不干扰 · 实例首次启动需各自登录账号",
                 bg=NAVY, fg="#94A3B8", font=(FONT, 9)).pack(side="left", pady=(6, 0))
        tk.Label(header, textvariable=self.stats_var, bg=NAVY, fg="#CBD5E1",
                 font=(FONT, 9)).pack(side="right", padx=16)

        # 底部状态栏（先 pack，保证空间不足时不被裁掉）
        tk.Frame(self.root, bg=BORDER, height=1).pack(side="bottom", fill="x")
        bottom = tk.Frame(self.root, bg=CARD, height=32)
        bottom.pack(side="bottom", fill="x")
        bottom.pack_propagate(False)
        tk.Label(bottom, textvariable=self.status_var, bg=CARD, fg=CHARCOAL,
                 font=(FONT, 9)).pack(side="left", padx=14)

        body = tk.Frame(self.root, bg=BG)
        body.pack(fill="both", expand=True)

        # 左：深藏青侧栏
        side = tk.Frame(body, bg=NAVY, width=236)
        side.pack(side="left", fill="y")
        side.pack_propagate(False)
        tk.Label(side, text="AGENTS", bg=NAVY, fg="#64748B",
                 font=(FONT, 9, "bold")).pack(anchor="w", padx=16, pady=(14, 6))

        list_wrap = tk.Frame(side, bg=NAVY)
        list_wrap.pack(fill="both", expand=True, padx=8)
        self.app_list = tk.Listbox(
            list_wrap, exportselection=False, font=(FONT, 10),
            bg=NAVY, fg="#E2E8F0", selectbackground=RED, selectforeground="#FFFFFF",
            activestyle="none", highlightthickness=0, bd=0,
            relief="flat", selectborderwidth=0)
        self.app_list.pack(fill="both", expand=True)
        self.app_list.bind("<<ListboxSelect>>", lambda e: self.refresh_instances())

        side_info = tk.Frame(side, bg=NAVY)
        side_info.pack(fill="x", padx=16, pady=(6, 14))
        tk.Label(side_info, text="红底 = 当前选中\n灰字 = 未检测到安装",
                 bg=NAVY, fg="#64748B", font=(FONT, 8), justify="left").pack(anchor="w")

        # 右：内容区
        content = tk.Frame(body, bg=BG)
        content.pack(side="left", fill="both", expand=True, padx=12, pady=10)

        # 卡片 1：实例
        inst_card, inst_in = card(content, "实例（账号档案）", "双击行 = 直接启动")
        inst_card.pack(fill="both", expand=True)

        cols = ("status", "name", "workdir", "created")
        inst_row = tk.Frame(inst_in, bg=CARD)
        inst_row.pack(fill="both", expand=True)
        self.inst_tree = ttk.Treeview(inst_row, columns=cols, show="headings", height=8)
        for c, t, w, a in (("status", "状态", 150, "w"), ("name", "实例名", 120, "w"),
                           ("workdir", "工作目录", 320, "w"), ("created", "创建时间", 130, "w")):
            self.inst_tree.heading(c, text=t)
            self.inst_tree.column(c, width=w, anchor=a, stretch=(c == "workdir"))
        vsb = ttk.Scrollbar(inst_row, orient="vertical", command=self.inst_tree.yview)
        self.inst_tree.configure(yscrollcommand=vsb.set)
        self.inst_tree.pack(side="left", fill="both", expand=True)
        vsb.pack(side="left", fill="y")
        self.inst_tree.bind("<Double-1>", lambda e: self.launch_instance())
        self.inst_tree.tag_configure("running", foreground=GREEN)
        self.inst_tree.tag_configure("stopped", foreground=MUTED)

        btns = tk.Frame(inst_in, bg=CARD)
        btns.pack(fill="x", pady=(10, 0))
        row1 = tk.Frame(btns, bg=CARD)
        row1.pack(fill="x")
        ttk.Button(row1, text="＋ 新建实例", style="Primary.TButton",
                   command=self.new_instance).pack(side="left")
        ttk.Button(row1, text="▶ 启动", style="Go.TButton",
                   command=self.launch_instance).pack(side="left", padx=6)
        ttk.Button(row1, text="■ 结束", style="Danger.TButton",
                   command=self.kill_instance).pack(side="left")
        ttk.Button(row1, text="删除实例", style="Tool.TButton",
                   command=self.delete_instance).pack(side="right")

        row2 = tk.Frame(btns, bg=CARD)
        row2.pack(fill="x", pady=(6, 0))
        for text, cmd in (("备份账号", self.backup_instance),
                          ("恢复账号", self.restore_instance),
                          ("打开数据目录", self.open_instance_dir)):
            ttk.Button(row2, text=text, style="Tool.TButton",
                       command=cmd).pack(side="left", padx=(0, 6))

        wd = tk.Frame(inst_in, bg=CARD)
        wd.pack(fill="x", pady=(10, 0))
        tk.Label(wd, text="工作目录", bg=CARD, fg=MUTED,
                 font=(FONT, 9, "bold")).pack(side="left")
        ent = ttk.Entry(wd, textvariable=self.workdir)
        ent.pack(side="left", fill="x", expand=True, padx=8)
        ttk.Button(wd, text="浏览…", style="Tool.TButton",
                   command=self.pick_workdir).pack(side="left")
        ttk.Button(wd, text="用当前测试区", style="Tool.TButton",
                   command=lambda: self.workdir.set(r"D:\Claude\人工智能产品测试")).pack(side="left", padx=6)

        # 卡片 2：进程
        proc_card, proc_in = card(content, "运行中的进程", "每 4 秒自动刷新")
        proc_card.pack(fill="both", expand=True, pady=(10, 0))

        pcols = ("app", "detail", "pid")
        proc_row = tk.Frame(proc_in, bg=CARD)
        proc_row.pack(fill="both", expand=True)
        self.proc_tree = ttk.Treeview(proc_row, columns=pcols, show="headings", height=6)
        for c, t, w in (("app", "Agent", 200), ("detail", "详情", 420), ("pid", "PID", 120)):
            self.proc_tree.heading(c, text=t)
            self.proc_tree.column(c, width=w, anchor="w", stretch=(c == "detail"))
        pvsb = ttk.Scrollbar(proc_row, orient="vertical", command=self.proc_tree.yview)
        self.proc_tree.configure(yscrollcommand=pvsb.set)
        self.proc_tree.pack(side="left", fill="both", expand=True)
        pvsb.pack(side="left", fill="y")

        pbtns = tk.Frame(proc_in, bg=CARD)
        pbtns.pack(fill="x", pady=(10, 0))
        ttk.Button(pbtns, text="刷新", style="Tool.TButton",
                   command=self.refresh_all).pack(side="left")
        ttk.Button(pbtns, text="结束选中进程", style="Danger.TButton",
                   command=self.kill_process).pack(side="left", padx=6)

    def _auto_refresh(self):
        self.refresh_all()
        self.root.after(4000, self._auto_refresh)

    def status(self, msg):
        self.status_var.set(msg)

    # ---------- 数据 ----------
    def instances_of(self, app_id: str):
        result = []
        d = INSTANCES_DIR / app_id
        if d.exists():
            for f in sorted(d.glob("*.json")):
                try:
                    meta = json.loads(f.read_text(encoding="utf-8"))
                    meta["_file"] = str(f)
                    result.append(meta)
                except Exception:
                    pass
        return result

    def save_instance(self, app_id, meta):
        d = INSTANCES_DIR / app_id
        d.mkdir(parents=True, exist_ok=True)
        f = d / (meta["name"] + ".json")
        f.write_text(json.dumps(meta, ensure_ascii=False, indent=2), encoding="utf-8")
        meta["_file"] = str(f)
        return meta

    def current_app(self):
        sel = self.app_list.curselection()
        if not sel:
            return None
        idx = sel[0]
        return self._app_ids[idx] if idx < len(self._app_ids) else None

    # ---------- 刷新 ----------
    def refresh_apps(self):
        prev = self.current_app()
        self.app_list.delete(0, "end")
        for i, app_id in enumerate(self._app_ids):
            app = APPS[app_id]
            exe = resolve_exe(app)
            n = len(self.instances_of(app_id))
            mark = "●" if exe else "○"
            suffix = f"  ·  {n} 实例" if n else ""
            self.app_list.insert("end", f" {mark}  {app['name']}{suffix}")
            self.app_list.itemconfig(i, foreground="#E2E8F0" if exe else "#64748B")
        # 保持之前的选中
        idx = self._app_ids.index(prev) if prev in self._app_ids else 0
        self.app_list.selection_clear(0, "end")
        self.app_list.selection_set(idx)

    def refresh_instances(self):
        app_id = self.current_app()
        self.inst_tree.delete(*self.inst_tree.get_children())
        if not app_id:
            return
        snap = tasklist_snapshot()
        app = APPS[app_id]
        exe_name = Path(app["exe"]).name.lower()
        for meta in self.instances_of(app_id):
            if app["kind"] == "cli":
                running = pid_alive(meta.get("pid", 0))
                st = f"● 运行中  PID {meta['pid']}" if running else "○ 未运行"
            else:
                n = sum(1 for p, name in snap.items() if name.lower() == exe_name)
                st = f"● 程序运行中 ×{n}" if n else "○ 未运行"
            created = time.strftime("%Y-%m-%d %H:%M", time.localtime(meta.get("created", 0)))
            self.inst_tree.insert(
                "", "end",
                values=(st, meta["name"], meta.get("workdir", ""), created),
                tags=("running" if st.startswith("●") else "stopped",))

    def refresh_procs(self):
        self.proc_tree.delete(*self.proc_tree.get_children())
        snap = tasklist_snapshot()
        n_proc_rows = 0
        for app_id, app in APPS.items():
            exe = resolve_exe(app)
            if not exe:
                continue
            if app["kind"] == "electron":
                pids = [p for p, name in snap.items() if name.lower() == exe.name.lower()]
                if pids:
                    self.proc_tree.insert("", "end", values=(
                        app["name"], f"{len(pids)} 个进程（含渲染/GPU 子进程）",
                        ",".join(map(str, sorted(pids)[:6]))))
                    n_proc_rows += 1
            else:
                for meta in self.instances_of(app_id):
                    pid = meta.get("pid", 0)
                    if pid_alive(pid):
                        self.proc_tree.insert("", "end", values=(
                            f"{app['name']} / {meta['name']}",
                            meta.get("workdir", ""), pid))
                        n_proc_rows += 1
        total_inst = sum(len(self.instances_of(a)) for a in APPS)
        self.stats_var.set(f"实例 {total_inst}  ·  运行中 {n_proc_rows}  ·  "
                           f"{time.strftime('%H:%M:%S')}")

    def refresh_all(self):
        self.refresh_instances()
        self.refresh_procs()

    # ---------- 实例操作 ----------
    def new_instance(self):
        app_id = self.current_app()
        if not app_id:
            return
        app = APPS[app_id]
        if not resolve_exe(app):
            messagebox.showwarning(APP_NAME, f"未找到 {app['name']} 的可执行文件，无法新建实例。")
            return
        existing = {m["name"] for m in self.instances_of(app_id)}
        i = 1
        while f"账号{i}" in existing:
            i += 1
        name = f"账号{i}"
        data_dir = INSTANCES_DIR / app_id / name
        data_dir.mkdir(parents=True, exist_ok=True)
        self.save_instance(app_id, {
            "name": name,
            "app": app_id,
            "workdir": self.workdir.get(),
            "data_dir": str(data_dir),
            "created": time.time(),
            "pid": 0,
        })
        self.refresh_apps()
        self.refresh_instances()
        self.status(f"已新建 {app['name']} 实例「{name}」，首次启动请在窗口内登录对应账号")

    def _selected_instance(self):
        app_id = self.current_app()
        sel = self.inst_tree.selection()
        if not app_id or not sel:
            messagebox.showinfo(APP_NAME, "请先在左侧选 Agent、中间选实例。")
            return None, None, None
        name = self.inst_tree.item(sel[0], "values")[1]
        for meta in self.instances_of(app_id):
            if meta["name"] == name:
                return app_id, name, meta
        return app_id, name, None

    def launch_instance(self):
        app_id, name, meta = self._selected_instance()
        if not meta:
            return
        app = APPS[app_id]
        exe = resolve_exe(app)
        if not exe:
            messagebox.showerror(APP_NAME, f"未找到 {app['name']} 可执行文件")
            return
        workdir = meta.get("workdir") or self.workdir.get()
        if not Path(workdir).exists():
            workdir = str(Path.home())
        data_dir = Path(meta["data_dir"])
        data_dir.mkdir(parents=True, exist_ok=True)

        if app["kind"] == "electron":
            cmd = [str(exe), f"--user-data-dir={data_dir}", workdir]
            try:
                subprocess.Popen(cmd, cwd=str(exe.parent), close_fds=True,
                                 creationflags=DETACHED)
                self.status(f"已启动 {app['name']}「{name}」 → {workdir}")
            except Exception as e:
                messagebox.showerror(APP_NAME, f"启动失败：{e}")
                return
        else:
            env = dict(os.environ)
            env[app["env"]] = str(data_dir)
            env["PYTHONIOENCODING"] = "utf-8"
            inner = f'cd /d "{workdir}" && set "{app["env"]}={data_dir}" && "{exe}"'
            try:
                p = subprocess.Popen(["cmd", "/c", "start", "cmd", "/k", inner],
                                     cwd=workdir, env=env, close_fds=True,
                                     creationflags=DETACHED)
                # start 命令本身立即退出，记录不了真实 PID；用窗口标题兜底
                meta["pid"] = 0
                self.save_instance(app_id, meta)
                self.status(f"已在新终端启动 {app['name']}「{name}」（{app['env']}={data_dir}）")
            except Exception as e:
                messagebox.showerror(APP_NAME, f"启动失败：{e}")
                return
        self.root.after(1500, self.refresh_all)

    def kill_instance(self):
        app_id, name, meta = self._selected_instance()
        if not meta:
            return
        app = APPS[app_id]
        if app["kind"] == "cli":
            pid = meta.get("pid", 0)
            if pid_alive(pid):
                subprocess.run(["taskkill", "/pid", str(pid), "/t", "/f"], capture_output=True)
                self.status(f"已结束 {name} (PID {pid})")
            else:
                self.status("该 CLI 实例未在运行（由 start 启动的窗口请从进程表结束）")
        else:
            exe = Path(app["exe"]).name
            snap = tasklist_snapshot()
            pids = [p for p, n in snap.items() if n.lower() == exe.lower()]
            if not pids:
                self.status("该程序当前没有运行中的进程")
                return
            if not messagebox.askyesno(
                    APP_NAME,
                    f"将结束 {app['name']} 的全部 {len(pids)} 个进程（含所有实例窗口）。\n"
                    f"目前按程序名结束，暂无法只结束单个实例。继续？"):
                return
            for p in pids:
                subprocess.run(["taskkill", "/pid", str(p), "/t", "/f"], capture_output=True)
            self.status(f"已结束 {app['name']} 全部进程")
        self.refresh_all()

    def kill_process(self):
        sel = self.proc_tree.selection()
        if not sel:
            return
        pids = self.proc_tree.item(sel[0], "values")[2]
        for pid in str(pids).split(","):
            pid = pid.strip()
            if pid.isdigit():
                subprocess.run(["taskkill", "/pid", pid, "/t", "/f"], capture_output=True)
        self.status(f"已结束进程 {pids}")
        self.refresh_all()

    def backup_instance(self):
        app_id, name, meta = self._selected_instance()
        if not meta:
            return
        data_dir = Path(meta["data_dir"])
        if not data_dir.exists():
            return
        ts = time.strftime("%Y%m%d-%H%M%S")
        zip_path = BACKUP_DIR / f"{app_id}__{name}__{ts}.zip"
        skip_dirs = {"Cache", "Code Cache", "GPUCache", "logs", "Crashpad",
                     "DawnCache", "ShaderCache", "Service Worker"}
        try:
            with zipfile.ZipFile(zip_path, "w", zipfile.ZIP_DEFLATED) as zf:
                for f in data_dir.rglob("*"):
                    if f.is_file() and not any(part in skip_dirs for part in f.parts):
                        zf.write(f, f.relative_to(data_dir))
            self.status(f"已备份到 {zip_path.name}（{zip_path.stat().st_size // 1024} KB）")
        except Exception as e:
            messagebox.showerror(APP_NAME, f"备份失败：{e}")

    def restore_instance(self):
        app_id, name, meta = self._selected_instance()
        if not meta:
            return
        cands = sorted(BACKUP_DIR.glob(f"{app_id}__{name}__*.zip"))
        if not cands:
            messagebox.showinfo(APP_NAME, "没有该实例的备份。")
            return
        zip_path = cands[-1]
        if not messagebox.askyesno(APP_NAME,
                                   f"用最新备份覆盖实例「{name}」的数据目录？\n{zip_path.name}\n"
                                   f"（请先关闭该实例的窗口）"):
            return
        data_dir = Path(meta["data_dir"])
        try:
            if data_dir.exists():
                shutil.rmtree(data_dir)
            data_dir.mkdir(parents=True)
            with zipfile.ZipFile(zip_path) as zf:
                zf.extractall(data_dir)
            self.status(f"已从 {zip_path.name} 恢复")
        except Exception as e:
            messagebox.showerror(APP_NAME, f"恢复失败：{e}")

    def open_instance_dir(self):
        app_id, name, meta = self._selected_instance()
        if not meta:
            return
        d = Path(meta["data_dir"])
        d.mkdir(parents=True, exist_ok=True)
        os.startfile(str(d))

    def delete_instance(self):
        app_id, name, meta = self._selected_instance()
        if not meta:
            return
        if not messagebox.askyesno(APP_NAME,
                                   f"删除实例「{name}」及其数据目录（含登录状态）？\n此操作不可恢复。"):
            return
        try:
            data_dir = Path(meta["data_dir"])
            if data_dir.exists():
                shutil.rmtree(data_dir, ignore_errors=True)
            Path(meta["_file"]).unlink(missing_ok=True)
            self.status(f"已删除实例「{name}」")
        except Exception as e:
            messagebox.showerror(APP_NAME, f"删除失败：{e}")
        self.refresh_apps()
        self.refresh_instances()

    def pick_workdir(self):
        d = filedialog.askdirectory(initialdir=self.workdir.get())
        if d:
            self.workdir.set(d)
            app_id = self.current_app()
            sel = self.inst_tree.selection()
            if app_id and sel:
                name = self.inst_tree.item(sel[0], "values")[1]
                for meta in self.instances_of(app_id):
                    if meta["name"] == name:
                        meta["workdir"] = d
                        self.save_instance(app_id, meta)
                self.refresh_instances()


def main():
    try:
        import ctypes
        ctypes.windll.shcore.SetProcessDpiAwareness(1)
    except Exception:
        pass
    root = tk.Tk()
    App(root)
    root.mainloop()


if __name__ == "__main__":
    main()
