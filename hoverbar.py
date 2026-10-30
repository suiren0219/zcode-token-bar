#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""ZCode Token 用量 · 独立悬浮条（不依赖 asar 注入）

为什么有这个：注入方案改的是 resources/app.asar，官方每次升级都会整包替换把它抹掉，
重装还得先完全退出客户端。这个悬浮条走另一条路——独立进程 + 无边框窗口，
只读 ~/.zcode/cli/db/db.sqlite，跟客户端怎么升级完全无关。

外观照注入版状态条还原：半透明深色圆角胶囊、1px 细描边、数值三档配色、
上下文带微进度条；位置默认贴在输入卡片下方。

用法：
    python hoverbar.py              # 启动（已在运行则打开设置面板）
    python hoverbar.py --settings   # 打开设置面板
    python hoverbar.py --quit       # 关掉正在运行的悬浮条
    python hoverbar.py --once       # 只在终端打印一次数据，不开窗（排查用）
    python hoverbar.py --check-update   # 只问 GitHub 有没有新版，不动文件
    python hoverbar.py --update     # 拉取最新版（工作区干净时才拉）
    python hoverbar.py --debug      # 开窗并打印定位/取数过程

打开设置（三种等价入口）：点条面最右端的切角方块（悬停变酸黄）、双击条面、
右键菜单「设置…」，或命令行 --settings。设置面板用终末地（endfield-ui）风格：
acid yellow #FFFA00 唯一强调色、切角=可点、展示容器直角、1px 细线分区。

设置项：位置 / 跟随窗口还是整块屏幕 / 偏移 / 透明度 / 字号 / 显示哪些项 /
失焦是否隐藏 / 是否置顶 / 是否启动时检查更新。拖动条面可直接改位置。

更新：默认**不自动安装**。勾上「启动时自动检查」后，每次启动 5 秒悄悄问一次
GitHub，有新版本只在设置面板「06 更新」和右键菜单里提示；拉不拉、什么时候拉
由用户点。手动入口是设置面板里的「检查更新 / 立即更新」或上面的命令行两个开关。
"""
from __future__ import annotations

import json
import os
import queue
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
import tkinter as tk
import tkinter.font as tkfont
from pathlib import Path
from tkinter import filedialog, messagebox

HERE = Path(__file__).parent.resolve()
ZUSAGE = HERE / "zusage.py"
CONFIG = HERE / "hoverbar.json"
LOCK = Path(os.environ.get("TEMP", ".")) / "zusage-hoverbar.lock"
DUMP = Path(os.environ.get("TEMP", ".")) / "zusage-hoverbar-dump.json"
IPC_PORT = 47615          # --settings 唤起已在运行实例的控制端口

# 注入版状态条的配色（overlay.js 同名变量）
BG = "#0f1219"
KEY = "#010203"           # -transparentcolor 键色：胶囊外的直角区域
BORDER = "#2b2f39"
FG = "#e9edf4"
DIM = "#7e8899"
LABEL = "#8791a3"
OK = "#3ecf8e"
WARM = "#f5b944"
HOT = "#ff6b57"
BADGE = "#ff8a73"

# 终末地（endfield-ui）配色：设置面板用。唯一强调色 acid yellow 只做填充，
# 只表示"当前 / 可点 / 注意"；切角=可点，展示容器一律直角；radius 全程 0；
# 1px 细线分区，无阴影无发光无渐变。
EF_PAPER = "#191919"
EF_PANEL = "#222222"
EF_INK = "#EDEDED"
EF_INK2 = "#A3A3A3"
EF_RULE = "#353535"
EF_CHIP = "#2E2E2E"
EF_SIGNAL = "#FFFA00"
EF_ON_SIGNAL = "#161616"
EF_DANGER = "#FF8A7A"
EF_TRI_A = "#FF00F0"
EF_TRI_B = "#FFFA00"
EF_TRI_C = "#00FFA2"
EF_CMD = "#161616"        # 顶部命令栏（两套主题都深色）

ANCHORS = ("bottom-left", "bottom-center", "bottom-right",
           "top-left", "top-center", "top-right", "free")
ANCHOR_TXT = {
    "bottom-left": "左下", "bottom-center": "下中", "bottom-right": "右下",
    "top-left": "左上", "top-center": "上中", "top-right": "右上",
    "free": "自定义（拖动过）",
}

DEFAULTS = {
    "anchor": "bottom-left",
    "scope": "window",        # window=跟随 ZCode 窗口 / screen=整块屏幕
    "offset_x": 16,
    "offset_y": 6,
    "free_x": 0,
    "free_y": 0,
    "opacity": 0.92,
    "font_px": 13,
    "font_family": "",
    "always_on_top": False,   # 默认靠"属主窗口"绑定：只浮在 ZCode 之上，不盖别的程序
    "hide_when_inactive": False,  # 属主绑定已随 ZCode 最小化/切换自动让路
    "show": {"speed": True, "ctx": True, "turn": True,
             "session": True, "today": True, "tools": True, "cache": True},
    "labels": True,
    "poll_ms": 1500,
    "panel_opacity": 1.0,     # 设置面板自身透明度；1.0=不透明
    "auto_update": False,     # 启动时静默问一次 GitHub；只提示，绝不自动安装
    "pinned": False,          # 钉住：显示停在 pinned_sid，位置锁死
    "pinned_sid": "",         # 钉住那一刻的会话 id；解除时清空
    "pin_x": -1,              # 钉住时冻结的位置；-1 = 未定（下个定位节拍按锚点重钉）
    "pin_y": -1,
}

DEBUG = False


def log(*a):
    if DEBUG:
        print("[hoverbar]", *a, flush=True)


# ---------------------------------------------------------------- DPI / Win32
def _init_dpi() -> float:
    """开进程级 DPI 感知，返回缩放比（96dpi=1.0）。不开的话坐标会被系统再缩放一次。"""
    import ctypes
    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(2)   # PER_MONITOR
    except Exception:
        try:
            ctypes.windll.user32.SetProcessDPIAware()
        except Exception:
            pass
    gdi = ctypes.WinDLL("gdi32")
    u32 = ctypes.WinDLL("user32")
    hdc = u32.GetDC(0)
    try:
        dpi = gdi.GetDeviceCaps(hdc, 88) or 96      # LOGPIXELSX
    finally:
        u32.ReleaseDC(0, hdc)
    return dpi / 96.0


SCALE = _init_dpi()


def _u32():
    import ctypes
    return ctypes.WinDLL("user32", use_last_error=True)


def zcode_hwnd() -> int:
    """ZCode 主窗口：可见、有标题含 ZCode、非工具窗口、足够大，取最大的那个。"""
    import ctypes
    from ctypes import wintypes

    u32 = _u32()
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _):
        if not u32.IsWindowVisible(hwnd):
            return True
        n = u32.GetWindowTextLengthW(hwnd)
        if n <= 0:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        u32.GetWindowTextW(hwnd, buf, n + 1)
        if "ZCode" not in buf.value:
            return True
        if u32.GetWindowLongW(hwnd, -20) & 0x00000080:   # WS_EX_TOOLWINDOW
            return True
        r = wintypes.RECT()
        u32.GetWindowRect(hwnd, ctypes.byref(r))
        w, h = r.right - r.left, r.bottom - r.top
        if w < 400 or h < 300:
            return True
        found.append((w * h, hwnd))
        return True

    u32.EnumWindows(cb, 0)
    return max(found)[1] if found else 0


def win_state(hwnd: int):
    """(left, top, w, h)；最小化或句柄失效返回 None。"""
    import ctypes
    from ctypes import wintypes

    if not hwnd:
        return None
    u32 = _u32()
    if not u32.IsWindow(hwnd) or u32.IsIconic(hwnd):
        return None
    r = wintypes.RECT()
    if not u32.GetWindowRect(hwnd, ctypes.byref(r)):
        return None
    return r.left, r.top, r.right - r.left, r.bottom - r.top


def monitor_work(hwnd: int):
    """ZCode 所在显示器的工作区（已排除任务栏）。"""
    import ctypes
    from ctypes import wintypes

    class MONITORINFO(ctypes.Structure):
        _fields_ = [("cbSize", wintypes.DWORD), ("rcMonitor", wintypes.RECT),
                    ("rcWork", wintypes.RECT), ("dwFlags", wintypes.DWORD)]

    u32 = _u32()
    MONITOR_DEFAULTTONEAREST = 2
    mon = u32.MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
    mi = MONITORINFO()
    mi.cbSize = ctypes.sizeof(MONITORINFO)
    if mon and u32.GetMonitorInfoW(mon, ctypes.byref(mi)):
        r = mi.rcWork
        return r.left, r.top, r.right - r.left, r.bottom - r.top
    r = wintypes.RECT()
    u32.SystemParametersInfoW(0x0030, 0, ctypes.byref(r), 0)   # SPI_GETWORKAREA
    return r.left, r.top, r.right - r.left, r.bottom - r.top


def foreground_is(hwnd: int) -> bool:
    """ZCode 是不是（或它的子窗口/我们自己的窗口是不是）在前台。

    只看 GetForegroundWindow() == hwnd 不够。用户和悬浮条交互时——打开设置
    面板、右键菜单、拖条面——前台窗口是**我们自己的** Toplevel，此时如果
    判成"ZCode 不在前台"，条面会在用户正操作它的时候把自己藏起来，体感就是
    "条丢了"。所以把"我们自己的窗口在前台"也算作可以显示。
    """
    import ctypes
    u32 = _u32()
    fg = u32.GetForegroundWindow()
    if not fg or not hwnd:
        return False
    if fg == hwnd or u32.GetAncestor(fg, 2) == hwnd:   # GA_ROOT=2
        return True
    # 前台是我们自己的窗口（设置面板 / 浮层 / 条面本身）→ 也算"在用"
    mine = _OWN_HWNDS.get("bar", 0)
    if mine and (fg == mine or u32.GetAncestor(fg, 2) == mine):
        return True
    for h in _OWN_HWNDS.get("extra", ()):
        if h and (fg == h or u32.GetAncestor(fg, 2) == h):
            return True
    return False


# 我们自己的窗口句柄，给 foreground_is 判断"用户正在操作条面"用。
# bar = 条面根窗口；extra = 设置面板、浮层等临时 Toplevel。
_OWN_HWNDS: dict = {"bar": 0, "extra": set()}


def register_own_window(win) -> int:
    """把一个 Toplevel/顶层窗口的根 HWND 记进 _OWN_HWNDS，返回它。
    失败返回 0（不影响功能，只是这项判断会退回旧行为）。"""
    try:
        h = _u32().GetAncestor(win.winfo_id(), 2)      # GA_ROOT
        if h:
            _OWN_HWNDS["extra"].add(h)
        return h
    except Exception:
        return 0


def unregister_own_window(h: int):
    if h:
        _OWN_HWNDS["extra"].discard(h)


def lbutton_down() -> bool:
    """左键物理状态。拖动状态机的兜底：<ButtonRelease-1> 会丢（快速点击、
    拖出屏幕、拖动中被 set_owner 重排打断、别的窗口抢焦点），丢了 _drag
    就永远非空，定位节拍跟着永久停摆——条面看起来"概率丢失"。
    直接问系统按键状态，比信任最后收到的事件可靠。"""
    import ctypes
    try:
        return bool(ctypes.windll.user32.GetAsyncKeyState(0x01) & 0x8000)
    except Exception:
        return True        # 查不出来就当还按着，宁可慢一拍也别误松手


# ---------------------------------------------------------------- 设置读写
def load_settings() -> dict:
    cfg = json.loads(json.dumps(DEFAULTS))     # 深拷贝
    try:
        raw = json.loads(CONFIG.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return cfg
    for k, v in raw.items():
        if k == "show" and isinstance(v, dict):
            # 只认识 DEFAULTS 里有的键，但方向是「用户值覆盖默认、缺的保留默认」。
            # 反过来写（只收 cfg 里已存在的键）会让新增的显示项在老配置文件上
            # 永远补不进来——文件是上一版写的，自然没有这个键。
            for kk, vv in v.items():
                if kk in cfg["show"]:
                    cfg["show"][kk] = bool(vv)
        elif k in cfg:
            cfg[k] = v
    return cfg


def save_settings(cfg: dict):
    try:
        CONFIG.write_text(json.dumps(cfg, ensure_ascii=False, indent=2),
                          encoding="utf-8")
    except OSError as e:
        log("保存设置失败", e)


# ---------------------------------------------------------------- 取数
class SnapshotSource:
    """常驻 `zusage.py serve`：stdin 一行请求，stdout 一行 JSON。
    比每次起解释器省 60–100ms，也不会每 1.5s 闪一个控制台。
    pin_fn 每次请求前取一次钉住的 sid（钉住时顶层快照给指定会话而不是最近活跃）。"""

    def __init__(self, interval_ms: int, pin_fn=None):
        self.interval = max(300, interval_ms) / 1000.0
        self.pin_fn = pin_fn or (lambda: "")
        self.q: queue.Queue = queue.Queue()
        self._proc = None
        self._stop = False
        self._th = None

    def start(self):
        self._th = threading.Thread(target=self._loop, daemon=True)
        self._th.start()

    def stop(self):
        self._stop = True
        self._kill()

    def _kill(self):
        p, self._proc = self._proc, None
        if p is not None:
            try:
                p.kill()
            except Exception:
                pass

    def alive(self) -> bool:
        return bool(self._th and self._th.is_alive())

    def _spawn(self):
        self._proc = subprocess.Popen(
            [sys.executable, str(ZUSAGE), "serve"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, encoding="utf-8",
            errors="replace", bufsize=1,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))

    def _loop(self):
        while not self._stop:
            t0 = time.time()
            try:
                if self._proc is None:
                    self._spawn()
                pin = ""
                try:
                    pin = str(self.pin_fn() or "")
                except Exception:
                    pin = ""
                # 无 pin 走老的空行协议；钉住时用 JSON 请求（zusage 两头都认）
                self._proc.stdin.write(
                    (json.dumps({"force": "", "pin": pin}, ensure_ascii=False)
                     if pin else "") + "\n")
                self._proc.stdin.flush()
                line = self._proc.stdout.readline()
                if not line:
                    raise RuntimeError("serve 已退出")
                d = json.loads(line)
                if isinstance(d, dict) and not d.get("error"):
                    self.q.put(d)
                else:
                    log("远端查询报错", d.get("error") if isinstance(d, dict) else d)
            except Exception as e:
                log("取数失败，重启 serve：", e)
                self._kill()
                time.sleep(0.5)
            time.sleep(max(0.2, self.interval - (time.time() - t0)))


def fmt(n) -> str:
    n = float(n or 0)
    for unit, div in (("B", 1e9), ("M", 1e6), ("K", 1e3)):
        if n >= div:
            return ("%.1f" % (n / div)).rstrip("0").rstrip(".") + unit
    return str(int(n))


def sec(ms) -> str:
    ms = float(ms or 0)
    if ms <= 0:
        return "-"
    if ms < 1000:
        return "%dms" % ms
    s = ms / 1000.0
    if s < 60:
        return "%.1fs" % s
    return "%d:%02d" % (s // 60, s % 60)


# ---------------------------------------------------------------- 悬浮条
class HoverBar:
    def __init__(self, cfg: dict):
        self.cfg = cfg
        self.data: dict | None = None
        self._last_draw = None
        self._hwnd = 0
        self._bar_hwnd = 0
        self._drag = None
        self._hover = None
        self._gear_armed = False
        self._pin_armed = False
        self._win_rect = None
        self._tip = None           # 悬停详情浮层（Toplevel），没弹时是 None
        self._tip_job = None       # 450ms 延迟任务；None 表示没在等
        self._tip_out_job = None   # 离开条面后的 150ms 收起延时
        self._tip_hover = False    # 指针在浮层上（进了就别收）
        self._upd_new = False      # 启动静默检查：发现新版就是 True
        self._upd_ver = ""         # "本地 → 远端"，右键菜单里直接显示
        self._upd_q: queue.Queue = queue.Queue()   # 后台检查结果回主线程

        self.root = tk.Tk()
        self.root.withdraw()
        self.root.overrideredirect(True)
        self.root.configure(bg=KEY)
        try:
            self.root.attributes("-transparentcolor", KEY)
        except tk.TclError:
            self.root.configure(bg=BG)
        self._apply_winprops()

        self.canvas = tk.Canvas(self.root, bg=KEY, highlightthickness=0, bd=0)
        self.canvas.pack()
        self._draw_key = None      # 上次画的内容签名；不变就跳过整次重绘

        self.font = None
        self._pick_font()

        self.canvas.bind("<Button-1>", self._drag_start)
        self.canvas.bind("<B1-Motion>", self._drag_move)
        self.canvas.bind("<ButtonRelease-1>", self._drag_end)
        # 双击开面板，但两端方块不算——图钉上连点两下是"钉/解钉"，不该带出面板
        self.canvas.bind(
            "<Double-Button-1>",
            lambda e: None if (self._hit_pin(e.x, e.y) or self._hit_gear(e.x, e.y))
            else self.open_settings())
        self.canvas.bind("<Button-3>", self._menu)
        self.canvas.bind("<Motion>", self._on_motion)
        self.canvas.bind("<Leave>", self._on_leave)
        self.root.bind("<Escape>", lambda e: None)

        self.src = SnapshotSource(
            cfg["poll_ms"],
            pin_fn=lambda: self.cfg["pinned_sid"] if self.cfg["pinned"] else "")
        self.src.start()

        self._start_ipc()
        self.root.after(60, self._tick_data)
        self.root.after(80, self._tick_anchor)
        # 启动 5 秒后才问，避开开机那一阵的网络拥塞；没勾自动检查就什么都不做
        if self.cfg.get("auto_update"):
            self.root.after(5000, self._upd_silent)

    # -- 启动时静默检查更新 --
    def _upd_silent(self):
        """只查、不装。查到的差异留给右键菜单和设置面板展示，
        要不要更新由用户在那个面板里点。"""
        def work():
            try:
                remote, local = remote_commit(), local_commit()
            except Exception:
                return
            # 走队列回主线程：跨线程直接调 root.after 在循环不运行时
            # 会静默失败，那个检查就永远没有下文
            try:
                self._upd_q.put((remote, local))
            except Exception:
                pass

        threading.Thread(target=work, daemon=True).start()

    def _upd_drain(self):
        """把后台检查的结果收进主线程。数据轮询同一个节拍上顺便做，
        不另起定时器。"""
        while True:
            try:
                remote, local = self._upd_q.get_nowait()
            except queue.Empty:
                return
            self._upd_new = bool(remote) and remote != local
            if self._upd_new:
                self._upd_ver = "%s → %s" % (local, remote)

    # -- 窗口属性 --
    def _apply_winprops(self):
        self.root.attributes("-alpha", min(1.0, max(0.25, float(self.cfg["opacity"]))))
        try:
            self.root.attributes("-topmost", bool(self.cfg["always_on_top"]))
        except tk.TclError:
            pass

    def _pick_font(self):
        fams = set(tkfont.families())
        want = self.cfg.get("font_family") or ""
        order = [want] if want else []
        order += ["Cascadia Mono", "Consolas", "JetBrains Mono", "Sarasa Mono SC",
                  "Microsoft YaHei UI", "Microsoft YaHei"]
        fam = next((f for f in order if f and f in fams), "Consolas")
        px = max(9, int(round(float(self.cfg["font_px"]) * SCALE)))
        self.font = tkfont.Font(family=fam, size=-px)
        self._px = px

    # -- 数据轮询 --
    def _tick_data(self):
        got = False
        while True:
            try:
                self.data = self.src.q.get_nowait()
                got = True
            except queue.Empty:
                break
        if got:
            self._check_pin_lost()
            self._draw()
            self._tip_refresh()        # 浮层开着就地把数字刷上去
        if not self.src.alive():           # 后台线程挂了 → 拉起来
            self.src = SnapshotSource(
                self.cfg["poll_ms"],
                pin_fn=lambda: self.cfg["pinned_sid"] if self.cfg["pinned"] else "")
            self.src.start()
        self._upd_drain()
        self.root.after(self.cfg["poll_ms"], self._tick_data)

    def _check_pin_lost(self):
        """钉住的会话在本地库和远端都查不到了（被删/换库）：自动解除钉住，
        回到跟随模式——比一直挂着一份查无此会话的旧显示诚实。"""
        if self.cfg["pinned"] and (self.data or {}).get("pin_lost"):
            self.cfg["pinned"] = False
            self.cfg["pinned_sid"] = ""
            self.cfg["pin_x"] = self.cfg["pin_y"] = -1
            save_settings(self.cfg)

    # -- 拼内容 --
    def _items(self):
        d = self.data or {}
        s = d.get("session") or {}
        lt = d.get("last_turn") or {}
        last = d.get("last") or {}
        today = d.get("today") or {}
        tls = d.get("tools") or {}
        show = self.cfg["show"]
        lab = self.cfg["labels"]
        items = []

        tps = last.get("tps") or 0
        if show.get("speed") and tps:
            col = OK if tps >= 70 else WARM if tps >= 40 else HOT
            it = []
            if lab:
                it.append(("t", "速度", LABEL))
            it += [("t", "%.1f" % tps, col), ("t", "t/s", DIM)]
            items.append(it)

        cw = int(d.get("context_window") or 0)
        ctx = s.get("ctx") or 0
        if show.get("ctx") and cw and ctx:
            pct = ctx / cw * 100.0
            big = cw >= 1_000_000
            col = HOT if pct >= (60 if big else 85) else \
                WARM if pct >= (40 if big else 70) else OK
            it = []
            if lab:
                it.append(("t", "上下文", LABEL))
            it += [("bar", min(100.0, pct), col), ("t", "%.1f%%" % pct, col)]
            items.append(it)

        if show.get("turn") and (lt.get("total") or lt.get("requests")):
            it = []
            if lab:
                it.append(("t", "本轮", LABEL))
            it.append(("t", fmt(lt.get("total")), FG))
            tail = "%d次" % (lt.get("requests") or 0)
            if lt.get("duration_ms"):
                tail += " " + sec(lt["duration_ms"])
            it.append(("t", tail, DIM))
            items.append(it)

        if show.get("session") and s.get("total"):
            it = []
            if lab:
                it.append(("t", "会话", LABEL))
            it += [("t", fmt(s["total"]), FG),
                   ("t", "%d轮 %d次" % (s.get("turns") or 0, s.get("requests") or 0), DIM)]
            items.append(it)

        if show.get("today") and today.get("total"):
            it = []
            if lab:
                it.append(("t", "今日", LABEL))
            it.append(("t", fmt(today["total"]), FG))
            items.append(it)

        if show.get("tools") and tls.get("total"):
            it = []
            if lab:
                it.append(("t", "工具", LABEL))
            it.append(("t", str(tls["total"]), FG))
            if tls.get("errors"):
                it.append(("t", "✕%d" % tls["errors"], BADGE))
            items.append(it)

        # 缓存率：命中缓存的输入 token 占全部输入的比例。
        # 分母用 input 本身——input_tokens 的口径已经包含 cache_read，
        # 再加一次 today.input 会把命中率算成一半。
        if show.get("cache") and s.get("input"):
            cin = int(s["input"])
            if cin > 0:
                rate = int(s.get("cache_read") or 0) / float(cin) * 100.0
                col = OK if rate >= 60 else WARM if rate >= 30 else DIM
                it = []
                if lab:
                    it.append(("t", "缓存", LABEL))
                it.append(("t", "%.0f%%" % rate, col))
                items.append(it)

        if d.get("remote"):
            items.insert(0, [("t", "远端", LABEL)])
        if d.get("not_local"):
            items.insert(0, [("t", "不在本地库", WARM)])
        return items
    # -- 绘制 --
    def _draw(self):
        items = self._items()
        if not items:
            items = [[("t", "等待数据…", DIM)]]

        pad, gap, inner = 10, 9, 4
        GAP = self.font.measure(" ")
        BAR_W_HINT = 46

        def twidth(t):
            return self.font.measure(t)

        blocks = []       # (宽, [draw ops])
        for it in items:
            ops, w = [], 0
            for tok in it:
                if tok[0] == "t":
                    _, txt, col = tok
                    ops.append(("t", txt, col))
                    w += twidth(txt)
                else:
                    _, pct, col = tok
                    ops.append(("bar", pct, col))
                    w += BAR_W_HINT
                w += GAP
            w -= GAP
            blocks.append((max(0, w), ops))

        sep_w = 1
        # 两端各一个方块（左钉住右设置）：边长跟字号走，各留 12px 与内容断开
        gear_s = self._gear_size()
        gear_gap = 12
        pin_s = gear_s
        pin_gap = 12
        total = (pad * 2 + sum(b[0] for b in blocks)
                 + sep_w * len(blocks) + gap * 2 * (len(blocks) - 1)
                 + gear_gap + gear_s + pin_gap + pin_s)
        h = self._px + 14
        total = int(total)
        if total < 60:
            total = 60

        # 内容签名：文字、颜色、尺寸、钉住态、悬停态。全没变就不重画——
        # 每 1.5s 一次的轮询里大多数时候数字确实没动（空闲时速度/本轮
        # 都不更新），重画一遍纯属白闪。
        # 注意 ops 必须物化成元组：生成器对象每次新建，拿它比等于永远不相等。
        sig = (total, h, self._px, bool(self.cfg["pinned"]),
               self._hover,
               tuple(tuple(tuple(o) for o in ops) for _, ops in blocks))
        if sig == self._draw_key:
            self._last_draw = (total, h)
            self._place()
            return
        self._draw_key = sig

        self.canvas.configure(width=total, height=h)
        self.canvas.delete("all")
        cv = self.canvas

        r = 9
        x1, y1, x2, y2 = 0.5, 0.5, total - 1.0, h - 1.0
        pts = [x1 + r, y1, x2 - r, y1, x2 - r, y1, x2, y1, x2, y1 + r,
               x2, y2 - r, x2, y2, x2, y2 - r, x2 - r, y2, x1 + r, y2,
               x1, y2, x1, y2 - r, x1, y1 + r, x1, y1, x1 + r, y1]
        cv.create_polygon(pts, smooth=True, splinesteps=20,
                          fill=BG, outline=BORDER, width=1)

        cy = h / 2.0
        # 左端图钉方块：内容从它右侧起。钉住时整块反 acid yellow——
        # 终末地语义里"填充=当前状态"，读作"这根条被钉住了"
        self._draw_pin(pad, cy, h)
        x = pad + pin_s + pin_gap
        for i, (w, ops) in enumerate(blocks):
            for op in ops:
                if op[0] == "t":
                    _, txt, col = op
                    cv.create_text(x, cy, text=txt, fill=col,
                                   font=self.font, anchor="w")
                    x += twidth(txt) + GAP
                else:
                    _, pct, col = op
                    bh = 5
                    bw = BAR_W_HINT
                    by = cy - bh / 2.0
                    cv.create_polygon(
                        [x + 2.5, by, x + bw - 2.5, by, x + bw, by + bh / 2,
                         x + bw - 2.5, by + bh, x + 2.5, by + bh, x, by + bh / 2],
                        smooth=False, fill="#272c36", outline="")
                    fw = max(0.0, bw * pct / 100.0)
                    if fw > 1:
                        cv.create_polygon(
                            [x + 2.5, by, x + max(2.6, fw - 2.5), by,
                             x + fw, by + bh / 2, x + max(2.6, fw - 2.5), by + bh,
                             x + 2.5, by + bh, x, by + bh / 2],
                            smooth=False, fill=col, outline="")
                    x += bw + GAP
            x -= GAP
            if i < len(blocks) - 1:
                x += gap
                cv.create_line(x, cy - 7, x, cy + 7, fill="#2f343e")
                x += sep_w + gap

        # 右端 ⚙：整条最后一项，切角方块 —— 终末地语义里"切角=可点"。
        # 悬停变 acid yellow 反白，点击开设置面板。
        # 方块右边距与左边 pad 对齐，视觉上才是"紧贴末端"而不是悬在中间。
        self._draw_gear(total - pad - gear_s, cy, h)

        self._last_draw = (total, h)
        self._place()

    def _gear_size(self):
        """齿轮方块边长：跟字号走，但不小于 14，免得小字号点不中。"""
        size = max(14, self._px)
        return int(size)

    def _gear_rect(self):
        """返回齿轮热区 (x1, y1, x2, y2)；没画过就是空。"""
        return getattr(self, "_gear_box", (0, 0, 0, 0))

    def _draw_gear(self, x, cy, h):
        s = self._gear_size()
        y1 = cy - s / 2.0
        y2 = cy + s / 2.0
        x2 = x + s
        # 右下角切 4px：终末地"可点/可操作"的视觉标记，不是装饰
        cut = 4
        pts = [x, y1, x2, y1, x2, y2 - cut, x2 - cut, y2, x, y2]
        hover = isinstance(self._hover, str) and self._hover == "gear"
        cv = self.canvas
        self.gear_bg = cv.create_polygon(
            pts, smooth=False,
            fill=EF_SIGNAL if hover else EF_CHIP,
            outline="")
        gcol = EF_ON_SIGNAL if hover else EF_INK2
        c = s / 2.0
        r = max(3.0, s * 0.30)
        cv.create_oval(
            x + c - r, cy - r, x + c + r, cy + r,
            outline=gcol, width=1.2, fill="")
        # 齿：四根短线，绕中心 45° 交叉，够小仍能读成齿轮
        for dx, dy in ((0, -1), (0, 1), (-1, 0), (1, 0)):
            cv.create_line(
                x + c + dx * r * 0.72, cy + dy * r * 0.72,
                x + c + dx * (r + 1.8), cy + dy * (r + 1.8),
                fill=gcol, width=1.2)
        self._gear_box = (x, y1, x2, y2)

    def _draw_pin(self, x, cy, h):
        """左端图钉方块，与右端齿轮同一套语言：切角=可点，填充=当前状态。
        图钉图形用三笔凑：横杠帽 + 针身 + 针尖，14px 内也读得出是"钉"。"""
        s = self._gear_size()
        y1 = cy - s / 2.0
        y2 = cy + s / 2.0
        x2 = x + s
        # 左下角切 4px，与齿轮的右下切角成镜像
        cut = 4
        pts = [x, y1, x2, y1, x2, y2, x + cut, y2, x, y2 - cut, x, y1]
        pinned = bool(self.cfg["pinned"])
        hover = isinstance(self._hover, str) and self._hover == "pin"
        hot = pinned or hover
        cv = self.canvas
        self.pin_bg = cv.create_polygon(
            pts, smooth=False,
            fill=EF_SIGNAL if hot else EF_CHIP, outline="")
        gcol = EF_ON_SIGNAL if hot else EF_INK2
        c = x + s / 2.0
        cap_y = y1 + s * 0.22
        cap_h = s * 0.16
        cv.create_rectangle(c - s * 0.24, cap_y, c + s * 0.24,
                            cap_y + cap_h, fill=gcol, outline="")
        cv.create_line(c, cap_y + cap_h, c, y2 - s * 0.18,
                       fill=gcol, width=max(1.6, s * 0.12))
        self._pin_box = (x, y1, x2, y2)

    def _pin_rect(self):
        """返回图钉热区 (x1, y1, x2, y2)；没画过就是空。"""
        return getattr(self, "_pin_box", (0, 0, 0, 0))

    def _hit_pin(self, x, y):
        x1, y1, x2, y2 = self._pin_rect()
        return x1 <= x <= x2 and y1 <= y <= y2

    # -- 定位 --
    def _place(self):
        if not self._last_draw:
            return
        bw, bh = self._last_draw
        cfg = self.cfg
        anchor = cfg["anchor"]

        if anchor == "free":
            # 自由位置也要跟 ZCode 绑：不绑的话拖过一次之后，条就既不随 ZCode
            # 最小化让路、也不会在客户端关掉后收起，跟「贴边」两种行为不一致。
            self._hwnd = zcode_hwnd()
            wr = win_state(self._hwnd)
            if not wr or (cfg["hide_when_inactive"]
                          and not foreground_is(self._hwnd)):
                self._maybe_hide()
                return
            self._win_rect = wr
            x, y = int(cfg["free_x"]), int(cfg["free_y"])
            if cfg["pinned"]:
                x, y = self._pinned_xy(x, y, bw, bh)
            self._settle(bw, bh, x, y)
            return

        self._hwnd = zcode_hwnd()
        wr = win_state(self._hwnd)
        if not wr or (cfg["hide_when_inactive"] and not foreground_is(self._hwnd)):
            self._maybe_hide()
            return
        self._win_rect = wr

        if cfg["scope"] == "screen":
            rx, ry, rw, rh = monitor_work(self._hwnd)
        else:
            rx, ry, rw, rh = wr
            # 全屏/最大化的窗口矩形不可直接当贴靠基准：最大化带 9px 隐形边框
            # （实测 -9,-9,1938,1098），F11 全屏则盖住整块显示器——贴它的边
            # 等于把条面送出屏幕、或压进任务栏底下被任务栏盖住。与所在显示器
            # 工作区取交集，两种形态都落回可见区；普通窗口与工作区本就相容，
            # 交集不改变结果。
            wx, wy, ww, wh = monitor_work(self._hwnd)
            il, it = max(rx, wx), max(ry, wy)
            ir, ib = min(rx + rw, wx + ww), min(ry + rh, wy + wh)
            if ir - il >= 400 and ib - it >= 100:   # 交集太小（极端窗口）不采用
                rx, ry, rw, rh = il, it, ir - il, ib - it
        ox, oy = int(cfg["offset_x"]), int(cfg["offset_y"])

        v, hpart = anchor.split("-", 1)          # bottom / top, left / center / right
        x = rx + (rw - bw) // 2 + ox if hpart == "center" else \
            rx + rw - bw - ox if hpart == "right" else rx + ox
        y = ry + rh - bh - oy if v == "bottom" else ry + oy
        if cfg["pinned"]:
            x, y = self._pinned_xy(x, y, bw, bh)

        # 只在真的变了才动窗口。geometry() 和改 HWLP_HWNDPARENT 都会让 Windows
        # 重排窗口，坐标没变也做一遍就是每 400ms 闪一次——肉眼看得见的抖动。
        self._settle(bw, bh, x, y)

    def _settle(self, bw, bh, x, y):
        """把窗口落到 (bw, bh, x, y)。已经是这个状态就什么都不做。

        为什么不用"我们上次设过什么"来判断：_placed/_visible 只是信念，
        不是事实。窗口会被外部原因改掉（别的程序移动它、系统重排、DPI 变化、
        Tk 重映射），信念和事实一分叉就再也修不回来——这轮修的一串"条面
        丢失"全是这一类。所以每次直接问 Tk 真实几何，跟目标比对。
        实测 4µs 一次（比枚举窗口便宜 65 倍），每秒 2.5 拍完全是噪声级；
        换来的是这类 bug 从"要靠自愈逻辑救"变成"结构上不可能发生"。
        """
        self._miss = 0                      # 这一拍能定位到，清空隐藏计数
        want = (bw, bh, x, y)
        real = self._real_state()
        if real is not None and real == want:
            return                          # 事实就是目标：不动窗口，零闪
        # 事实不等于目标（或问不到）：无条件 deiconify + 落位。
        # deiconify 必须发：对 withdraw 的窗口调 geometry() 不生效，
        # 只改标志位没用，必须重新映射。
        self.root.deiconify()
        self.root.geometry("%dx%d+%d+%d" % (bw, bh, x, y))
        self._bind_zorder()

    def _real_state(self):
        """窗口真实的 (宽, 高, x, y)；查不到返回 None。
        被藏起来（ismapped 为假）时返回全 0 —— 非空且不等于任何目标，
        正好触发一次真正的落位。"""
        try:
            if not self.root.winfo_exists():
                return None
            if not self.root.winfo_ismapped():
                return (0, 0, 0, 0)
            return (self.root.winfo_width(), self.root.winfo_height(),
                    self.root.winfo_x(), self.root.winfo_y())
        except tk.TclError:
            return None

    def _maybe_hide(self):
        """连续两拍都判定"该藏"才真藏。

        单拍判断不可靠：zcode_hwnd() 在客户端重启/切全屏/子窗口短暂变大时
        会瞬间找不到主窗口，foreground_is() 在前台切换瞬间也会误判。以前
        单拍就 withdraw，配合拖动状态卡死就会变成永久丢失。攒够两拍再藏，
        代价是多等 400ms，换来的是不会因为一次采样噪声把条面弄没。"""
        self._miss = getattr(self, "_miss", 0) + 1
        if self._miss >= 2:
            self._hide()

    _miss = 0

    def _pinned_xy(self, x, y, bw, bh):
        """钉住时位置锁死：一律用冻结坐标。pin_x/y 是 -1（刚在面板里改过
        位置类设置）就把这次按锚点算出的位置就地重新冻结。坐标再夹进
        ZCode 所在显示器的工作区，免得换屏/改分辨率后条飞出屏幕。"""
        px, py = int(self.cfg["pin_x"]), int(self.cfg["pin_y"])
        if px < 0 and py < 0:
            self.cfg["pin_x"], self.cfg["pin_y"] = x, y
            save_settings(self.cfg)
            return x, y
        wr = monitor_work(self._hwnd) if self._hwnd else None
        if not wr:
            return px, py
        rx, ry, rw, rh = wr
        return (min(max(rx, px), max(rx, rx + rw - bw)),
                min(max(ry, py), max(ry, ry + rh - bh)))

    def _show(self):
        """确保窗口是映射状态。已经映射就什么都不做（问 Tk 比记标志可靠）。"""
        try:
            if not self.root.winfo_ismapped():
                self.root.deiconify()
        except tk.TclError:
            pass

    def _hide(self):
        """收起。已经收起就不用再动。"""
        try:
            if self.root.winfo_ismapped():
                self.root.withdraw()
        except tk.TclError:
            pass

    @property
    def _visible(self) -> bool:
        """条面现在是否真的可见。直接问 Tk，不维护副本——
        这轮修的几个"丢失"全是副本与现实不同步。属性形式保住旧调用点。"""
        try:
            return bool(self.root.winfo_ismapped())
        except tk.TclError:
            return False

    def _bind_zorder(self):
        """把条设成 ZCode 的「属主窗口」(GWLP_HWNDPARENT)。
        Windows 原生绑定：属主窗口恒浮在属主之上、随属主最小化一起隐藏、不抢焦点、
        不进任务栏——不用 -topmost 也不会盖住别的程序，这才是真"绑在 ZCode 上"。"""
        if self._hwnd <= 0:
            return
        import ctypes
        u32 = _u32()
        if not self._bar_hwnd:
            self._bar_hwnd = u32.GetAncestor(self.root.winfo_id(), 2)   # GA_ROOT
            if not self._bar_hwnd:
                return
            # 登记给 foreground_is：用户在拖条面/点图钉时前台是我们自己，
            # 那种时候绝不能判成"ZCode 不在前台"而把条藏了
            _OWN_HWNDS["bar"] = self._bar_hwnd
            ex = u32.GetWindowLongW(self._bar_hwnd, -20)               # GWL_EXSTYLE
            u32.SetWindowLongW(self._bar_hwnd, -20,
                               ex | 0x00000080 | 0x08000000)           # TOOLWINDOW|NOACTIVATE
        if getattr(self, "_owner", None) == self._hwnd:
            return
        try:
            set_owner = ctypes.windll.user32.SetWindowLongPtrW
        except AttributeError:
            set_owner = u32.SetWindowLongW
        set_owner(self._bar_hwnd, -8, self._hwnd)                      # GWLP_HWNDPARENT
        self._owner = self._hwnd

    def _tick_anchor(self):
        # 拖动中不定位：_place() 会 set_owner 重排窗口，拖起来一抖一抖。
        # 但拖动状态可能永远解不开——<ButtonRelease-1> 是窗口事件，快速点击、
        # 拖出屏幕、拖动中被 set_owner 重排打断、别的窗口抢焦点，任何一个都能
        # 让它丢掉。丢了之后 _drag 非空 → 这里永久跳过 _place → 条面卡在旧
        # 位置、hide_when_inactive 一触发就彻底藏住，表现为"概率丢失"。
        # 两道兜底：问系统按键的真实状态，以及拖动总时长封顶。
        if getattr(self, "_drag", None) is not None:
            stuck = (not lbutton_down()
                     or (time.time() - getattr(self, "_drag_t0", 0)) > 30)
            if stuck:
                self._drag = None
                self._pin_armed = self._gear_armed = False
                try:
                    save_settings(self.cfg)     # 拖到哪算哪，落盘
                except Exception:
                    pass
                log("拖动状态卡住，已自动松手")
        if getattr(self, "_drag", None) is None:
            try:
                self._place()
            except Exception as e:
                log("定位失败", e)
        self.root.after(400, self._tick_anchor)

    # -- 拖动 / 菜单 --
    def _hit_gear(self, x, y):
        x1, y1, x2, y2 = self._gear_rect()
        return x1 <= x <= x2 and y1 <= y <= y2

    def _on_leave(self, e=None):
        self._set_hover(None)
        # 不立刻收：等 150ms 看指针去哪了。条面重绘变宽会引发假 Leave，
        # 鼠标往浮层挪的路上也会先 Leave 一下——两种都不能把浮层杀掉
        if getattr(self, "_tip_out_job", None):
            return
        self._tip_out_job = self.root.after(150, self._tip_leave_check)

    def _tip_leave_check(self):
        self._tip_out_job = None
        if getattr(self, "_tip_hover", False):
            return                      # 已经进浮层了，继续看
        try:
            w = self.root.winfo_containing(self.root.winfo_pointerx(),
                                           self.root.winfo_pointery())
        except Exception:
            w = None
        if w is self.canvas:
            return                      # 指针其实还在条面上，假 Leave
        self._tip_hide()

    def _set_hover(self, what):
        if getattr(self, "_hover", None) == what:
            return
        self._hover = what
        if self._last_draw:
            self._draw()

    def _on_motion(self, e):
        # 悬停在两端方块上就反白，让它们读起来像按钮；其它地方不折腾，省重绘
        if self._hit_pin(e.x, e.y):
            self._set_hover("pin")
            self._tip_hide()
        elif self._hit_gear(e.x, e.y):
            self._set_hover("gear")
            self._tip_hide()
        else:
            self._set_hover(None)
            self._tip_maybe(e.x_root, e.y_root)

    # -- 悬停详情 --
    def _tip_lines(self):
        """把快照摊成详情行。字段全部来自 zusage 的同一份 payload，
        远端会话也带同样的结构，所以这一块不用区分本地/远端。"""
        d = self.data or {}
        s = d.get("session") or {}
        lt = d.get("last_turn") or {}
        last = d.get("last") or {}
        today = d.get("today") or {}
        sub = s.get("sub") or d.get("sub") or {}
        tls = d.get("tools") or {}
        code = d.get("code") or {}
        rows = []

        def add(k, v):
            if v not in (None, "", "-"):
                rows.append((k, str(v)))

        def pct(a, b):
            return "%.1f%%" % (a / float(b) * 100.0) if b else "-"

        def clean(txt, n):
            """标题可能是路径或带换行的多段文本：只留最后一节、压成一行。"""
            txt = str(txt or "").replace("\r", " ").replace("\n", " ").strip()
            txt = re.split(r"[\\/]", txt)[-1].strip() or txt
            return txt[:n]

        add("会话", clean(s.get("title"), 28) or s.get("sid", "")[-8:])
        add("状态", "生成中" if s.get("active") else "空闲")

        ctx, cw = int(s.get("ctx") or 0), int(d.get("context_window") or 0)
        if ctx and cw:
            add("上下文", "%s / %s  %s" % (fmt(ctx), fmt(cw), pct(ctx, cw)))

        if s.get("input"):
            cin = int(s["input"])
            add("缓存率", "%s  (读 %s)" % (pct(int(s.get("cache_read") or 0), cin),
                                          fmt(s.get("cache_read"))))

        if lt.get("total"):
            seg = "%s  %d次" % (fmt(lt["total"]), lt.get("requests") or 0)
            if lt.get("duration_ms"):
                seg += "  " + sec(lt["duration_ms"])
            add("本轮", seg)
            if lt.get("input"):
                add("本轮输入", "%s  缓存 %s" % (fmt(lt["input"]),
                                                pct(int(lt.get("cache_read") or 0),
                                                    int(lt["input"]))))
            if lt.get("output"):
                add("本轮输出", fmt(lt["output"]))

        if s.get("total"):
            add("会话累计", "%s  %d轮 %d次" % (fmt(s["total"]), s.get("turns") or 0,
                                              s.get("requests") or 0))
        if today.get("total"):
            add("今日", "%s  %d次" % (fmt(today["total"]), today.get("requests") or 0))

        if last.get("model"):
            add("最近请求", "%s  %.1f t/s" % (last["model"], last.get("tps") or 0))
        if last.get("duration_ms"):
            add("首 token", sec(last.get("ttft_ms")))
        if s.get("retries"):
            add("重试", "%d 次" % s["retries"])

        if sub.get("list"):
            add("子代理", "%d 个  %s" % (len(sub["list"]), fmt(sub.get("total"))))
            for x in sub["list"][:3]:
                nm = clean(x.get("task") or x.get("title") or x.get("agent") or "sub", 16)
                rows.append(("  · " + nm, fmt(x.get("total"))))

        if tls.get("list"):
            top = sorted(tls["list"], key=lambda r: -r.get("count", 0))[:3]
            add("工具", "%d 次%s" % (tls.get("total") or 0,
                                    "  ✕%d" % tls["errors"] if tls.get("errors") else ""))
            for t in top:
                rows.append(("  · " + str(t.get("name"))[:16], "%d 次" % t.get("count", 0)))

        if code.get("add") or code.get("del"):
            add("改动", "+%s / -%s  %s 文件" % (code.get("add") or 0,
                                               code.get("del") or 0,
                                               code.get("files") or 0))
        if d.get("remote"):
            add("数据源", "远端 · %s" % (d.get("remote_host") or "ssh"))
        elif d.get("remote_error"):
            add("远端", str(d["remote_error"])[:40])
        if d.get("not_local"):
            add("提示", "该会话不在本地库")
        return rows

    def _tip_build(self):
        """详情浮层：无边框 Toplevel + 自绘行，跟条面同一套深色语言。
        只用 Label，不做滚动——行数由 _tip_lines 限制在十行出头。
        完全不透明：详情是拿来读的，透出底下的正文就没法看了，
        所以不吃条面自己的透明度设置。"""
        if getattr(self, "_tip", None) is not None:
            return self._tip
        rows = self._tip_lines()
        if not rows:
            return None
        t = tk.Toplevel(self.root)
        t.overrideredirect(True)
        try:
            t.attributes("-topmost", True)
        except tk.TclError:
            pass
        t.configure(bg=BORDER)
        inner = tk.Frame(t, bg=BG)
        inner.pack(fill="both", expand=True, padx=1, pady=1)
        # 顶部 2px 酸黄：跟条面同一处强调色，读起来是"这条的展开"
        tk.Frame(inner, bg=EF_SIGNAL, height=2).pack(fill="x")
        f_k = _ef_font(EF_UI, 8)
        f_v = _ef_font(EF_UI, 8, bold=True)
        self._tip_row_labels = []
        for k, v in rows:
            line = tk.Frame(inner, bg=BG)
            line.pack(fill="x", padx=10, pady=1)
            kl = tk.Label(line, text=k, bg=BG, fg=EF_INK2, font=f_k,
                          anchor="w")
            vl = tk.Label(line, text=v, bg=BG, fg=EF_INK, font=f_v,
                          anchor="e")
            kl.pack(side="left")
            vl.pack(side="right")
            self._tip_row_labels.append((kl, vl))
        tk.Frame(inner, bg=BG, height=3).pack(fill="x")
        # 鼠标移进浮层 = 继续看：收起延时就地作废；移出去才真的收
        t.bind("<Enter>", self._on_tip_enter)
        t.bind("<Leave>", self._on_tip_leave)
        register_own_window(t)            # 视线在浮层上也算"在用条面"
        t.update_idletasks()
        self._tip_rows = rows
        self._tip = t
        return t

    def _on_tip_enter(self, e=None):
        self._tip_hover = True
        if getattr(self, "_tip_out_job", None):
            try:
                self.root.after_cancel(self._tip_out_job)
            except Exception:
                pass
            self._tip_out_job = None

    def _on_tip_leave(self, e=None):
        self._tip_hover = False
        self._tip_hide()

    def _tip_refresh(self):
        """浮层开着时数据到点了：就地改字，不整窗重建（重建会闪）。
        行数对不上才按原位置重建一份。"""
        t = getattr(self, "_tip", None)
        if t is None:
            return
        rows = self._tip_lines()
        if rows == getattr(self, "_tip_rows", None):
            return
        labels = getattr(self, "_tip_row_labels", None)
        if labels is None or len(rows) != len(labels):
            bx, by = t.winfo_x(), t.winfo_y()
            self._tip_hide()
            nt = self._tip_build()
            if nt is not None:
                nt.geometry("+%d+%d" % (bx, by))
            return
        for (kl, vl), (k, v) in zip(labels, rows):
            kl.configure(text=k)
            vl.configure(text=v)
        self._tip_rows = rows
        t.update_idletasks()
        self._tip_position(t)

    def _tip_maybe(self, xr, yr):
        """悬停 450ms 才弹，鼠标一路划过条面不会闪一串浮层。"""
        if getattr(self, "_tip_job", None):
            return
        if getattr(self, "_tip", None) is not None:
            return
        self._tip_pos = (xr, yr)
        self._tip_job = self.root.after(450, self._tip_show)

    def _tip_show(self):
        self._tip_job = None
        if getattr(self, "_hover", None) in ("gear", "pin"):
            return
        t = self._tip_build()
        if t is None:
            return
        self._tip_position(t)
        try:
            t.deiconify()
        except tk.TclError:
            pass

    def _tip_position(self, t):
        # 条面正下方；下面放不下就翻到上方，左右夹在屏幕内
        bx, by = self.root.winfo_x(), self.root.winfo_y()
        bw, bh = self.root.winfo_width(), self.root.winfo_height()
        tw, th = t.winfo_reqwidth(), t.winfo_reqheight()
        sw, sh = t.winfo_screenwidth(), t.winfo_screenheight()
        x = min(max(8, bx), max(8, sw - tw - 8))
        y = by + bh + 6
        if y + th > sh - 8:
            y = max(8, by - th - 6)
        # 坐标没变就不 geometry：刷新数字时每拍都摆一次，浮层会跟着抖
        if (x, y) != getattr(t, "_placed_at", None):
            t.geometry("+%d+%d" % (x, y))
            t._placed_at = (x, y)

    def _tip_hide(self):
        if getattr(self, "_tip_out_job", None):
            try:
                self.root.after_cancel(self._tip_out_job)
            except Exception:
                pass
            self._tip_out_job = None
        self._tip_hover = False
        if getattr(self, "_tip_job", None):
            try:
                self.root.after_cancel(self._tip_job)
            except Exception:
                pass
            self._tip_job = None
        t = getattr(self, "_tip", None)
        if t is not None:
            try:
                t.destroy()
            except tk.TclError:
                pass
            self._tip = None


    def _drag_start(self, e):
        self._tip_hide()
        if self._hit_pin(e.x, e.y):         # 图钉是按钮，不是拖动把手
            self._pin_armed = True
            return
        self._pin_armed = False
        if self._hit_gear(e.x, e.y):        # 齿轮是按钮，不是拖动把手
            self._gear_armed = True
            return
        self._gear_armed = False
        if self.cfg["pinned"]:
            return                           # 钉住时位置锁死，拖不动
        self._drag = (e.x_root - self.root.winfo_x(),
                      e.y_root - self.root.winfo_y())
        self._drag_t0 = time.time()          # 拖动超时兜底的起点（见 _tick_anchor）

    def _drag_move(self, e):
        if not self._drag:
            return
        x, y = e.x_root - self._drag[0], e.y_root - self._drag[1]
        self.cfg["anchor"] = "free"
        self.cfg["free_x"], self.cfg["free_y"] = x, y
        self.root.geometry("+%d+%d" % (x, y))

    def _drag_end(self, e):
        if getattr(self, "_pin_armed", False):
            self._pin_armed = False
            if self._hit_pin(e.x, e.y):
                self._toggle_pin()
            return
        if getattr(self, "_gear_armed", False):
            self._gear_armed = False
            if self._hit_gear(e.x, e.y):
                self.open_settings()
            return
        if self._drag:
            self._drag = None
            save_settings(self.cfg)

    def _toggle_pin(self):
        """左端图钉的开关：钉住 = 显示停在当前会话 + 位置冻结在当下；
        解除 = 两样都恢复跟随。没有会话数据时钉不了。"""
        if self.cfg["pinned"]:
            self.cfg["pinned"] = False
            self.cfg["pinned_sid"] = ""
            self.cfg["pin_x"] = self.cfg["pin_y"] = -1
        else:
            sid = ((self.data or {}).get("session") or {}).get("sid") or ""
            if not sid:
                return
            self.cfg["pinned"] = True
            self.cfg["pinned_sid"] = sid
            self.cfg["pin_x"] = self.root.winfo_x()
            self.cfg["pin_y"] = self.root.winfo_y()
        save_settings(self.cfg)
        self._draw()

    def _menu(self, e):
        m = tk.Menu(self.root, tearoff=0)
        m.add_command(label="解除钉住" if self.cfg["pinned"] else "钉住当前",
                      command=self._toggle_pin)
        m.add_command(label="设置…", command=self.open_settings)
        m.add_command(label="重置位置", command=self._reset_pos)
        if getattr(self, "_upd_new", False):
            m.add_separator()
            m.add_command(label="有新版本 %s，去更新" % self._upd_ver,
                          command=self.open_settings)
        m.add_separator()
        m.add_command(label="退出", command=self.quit)
        try:
            m.tk_popup(e.x_root, e.y_root)
        finally:
            m.grab_release()

    def _reset_pos(self):
        self.cfg.update({"anchor": DEFAULTS["anchor"], "scope": DEFAULTS["scope"],
                         "offset_x": DEFAULTS["offset_x"],
                         "offset_y": DEFAULTS["offset_y"]})
        if self.cfg["pinned"]:
            self.cfg["pin_x"] = self.cfg["pin_y"] = -1
        save_settings(self.cfg)
        self._place()

    # -- 设置面板 --
    def open_settings(self):
        self._tip_hide()
        self._set_hover(None)
        dlg = getattr(self, "_dlg", None)
        if dlg is not None:            # 已经开着就抬起来，别开一摞
            try:
                dlg.top.deiconify()
                dlg.top.lift()
                dlg.top.focus_force()
                return dlg
            except tk.TclError:
                self._dlg = None
        self._dlg = SettingsDialog(self)
        return self._dlg

    def apply_settings(self, repin=False):
        self.cfg["poll_ms"] = max(400, int(self.cfg["poll_ms"]))
        if repin and self.cfg["pinned"]:
            # 位置类设置改了：旧冻结坐标作废，下个定位节拍按新锚点重新钉
            self.cfg["pin_x"] = self.cfg["pin_y"] = -1
        self._apply_winprops()
        self._pick_font()
        self._draw()

    # -- IPC：--settings 唤起已运行实例 --
    def _start_ipc(self):
        def srv():
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                s.bind(("127.0.0.1", IPC_PORT))
                s.listen(2)
            except OSError as e:
                log("IPC 端口占用", e)
                return
            while True:
                try:
                    c, _ = s.accept()
                    msg = c.recv(64).decode("utf-8", "replace").strip()
                    c.close()
                    if msg == "settings":
                        self.root.after(0, self.open_settings)
                    elif msg == "quit":
                        self.root.after(0, self.quit)
                    elif msg == "dump":
                        # 排查用：把进程内真实状态打到一个文件里。
                        # 条面"看不见"时，光看窗口几何分不清是没画出来、
                        # 画到屏幕外、还是被藏起来了。
                        try:
                            DUMP.write_text(json.dumps(self._dump(), ensure_ascii=False,
                                                       indent=2), encoding="utf-8")
                        except OSError:
                            pass
                    elif msg == "repaint":
                        # 强制丢弃签名缓存重画一次（排查/兜底用）
                        self.root.after(0, self._force_redraw)
                except OSError:
                    return

        threading.Thread(target=srv, daemon=True).start()

    def _dump(self) -> dict:
        """进程内真实状态快照。只读，不改任何东西。"""
        d = {
            "pid": os.getpid(),
            "visible": self._visible,
            "real_state": self._real_state(),
            "last_draw": self._last_draw,
            "draw_key_len": len(self._draw_key) if self._draw_key else 0,
            "draw_key_head": (self._draw_key[:4] if self._draw_key else None),
            "canvas_items": len(self.canvas.find_all()),
            "canvas_size": [self.canvas.winfo_width(), self.canvas.winfo_height()],
            "canvas_req": [self.canvas.winfo_reqwidth(), self.canvas.winfo_reqheight()],
            "root_geom": [self.root.winfo_x(), self.root.winfo_y(),
                          self.root.winfo_width(), self.root.winfo_height()],
            "root_mapped": bool(self.root.winfo_ismapped()),
            "drag": self._drag,
            "miss": self._miss,
            "hover": self._hover,
            "hwnd": self._hwnd,
            "font_px": self._px,
            "data_none": self.data is None,
            "session": ((self.data or {}).get("session") or {}).get("sid", "")[-8:],
            "anchor": self.cfg.get("anchor"),
            "scope": self.cfg.get("scope"),
        }
        return d

    def _force_redraw(self):
        """丢掉签名缓存，无条件重画并落位。

        签名门控正常情况下是省开销的好东西，但画布被外部原因清空时
        （Tk 重映射、DPI 变化）签名不变就会一直跳过重画——这是那种情况的兜底。
        """
        self._draw_key = None
        self._draw()

    def quit(self):
        try:
            self.src.stop()
            LOCK.unlink()
        except OSError:
            pass
        self.root.destroy()

    def run(self):
        try:
            LOCK.write_text(str(os.getpid()))
        except OSError:
            pass
        self.root.mainloop()


# ---------------------------------------------------------------- 设置面板
# 风格：终末地（endfield-ui）
#   唯一强调色 acid yellow #FFFA00，只做填充，只表示"当前 / 可点 / 注意"；
#   切角 = 可点击（按钮、勾选项），展示容器一律直角，radius 全程 0；
#   1px 细线分区，无阴影无发光无渐变；宽体大写拉丁眉标 + 粗体中文；
#   数字用等宽字体。
EF_UI = ("Saira", "Noto Sans SC", "Microsoft YaHei UI", "Microsoft YaHei")
EF_MONO = ("Cascadia Mono", "Consolas")


def _ef_font(fams, size, bold=False):
    have = set(tkfont.families())
    fam = next((f for f in fams if f in have), "Microsoft YaHei UI")
    return (fam, size, "bold") if bold else (fam, size)


class _EfButton(tk.Canvas):
    """切角按钮。终末地里切角代表"可操作"，所以按钮必须有切角；
    主按钮实心 acid yellow + 反白字，次按钮只有 1px 边线。"""

    def __init__(self, master, text, command, primary=False, width=None):
        self.text = text
        self.command = command
        self.primary = primary
        self._hover = False
        f = _ef_font(EF_UI, 9, bold=primary)
        probe = tkfont.Font(root=master, font=f)
        h, cut = 30, 8
        w = width or (probe.measure(text) + 34)
        super().__init__(master, width=w, height=h, bg=EF_PANEL,
                         highlightthickness=0, bd=0, cursor="hand2")
        self._font = f
        self._cw, self._ch, self._cut = w, h, cut
        self.bind("<Enter>", lambda e: self._set_hover(True))
        self.bind("<Leave>", lambda e: self._set_hover(False))
        self.bind("<Button-1>", lambda e: self.command())
        self._paint()

    def _set_hover(self, v):
        if self._hover != v:
            self._hover = v
            self._paint()

    def _paint(self):
        self.delete("all")
        w, h, cut = self._cw, self._ch, self._cut
        pts = [0.5, 0.5, w - cut, 0.5, w - 0.5, cut + 0.5, w - 0.5, h - 0.5,
               0.5, h - 0.5]
        if self.primary:
            fill = EF_SIGNAL if not self._hover else "#FFF86B"
            self.create_polygon(pts, smooth=False, fill=fill,
                                outline=fill, width=1)
            fg = EF_ON_SIGNAL
        else:
            fill = EF_CHIP if self._hover else EF_PANEL
            self.create_polygon(pts, smooth=False, fill=fill,
                                outline=EF_RULE, width=1)
            fg = EF_INK if self._hover else EF_INK2
        self.create_text(w / 2.0, h / 2.0, text=self.text, fill=fg,
                         font=self._font)


class _EfCheck(tk.Canvas):
    """勾选项。未选中是 1px 空方框，选中是实心 acid yellow 方块 + 墨色 ✓；
    文字跟着变亮——状态靠"填满/未填满"，不靠颜色区分。"""

    def __init__(self, master, text, var, big=False):
        self.var = var
        self.text = text
        self._hover = False
        f = _ef_font(EF_UI, 10 if big else 9, bold=big)
        probe = tkfont.Font(root=master, font=f)
        self._box = 15 if big else 13
        self._h = 24 if big else 21
        w = self._box + 9 + probe.measure(text)
        super().__init__(master, width=w, height=self._h, bg=EF_PANEL,
                         highlightthickness=0, bd=0, cursor="hand2")
        self._font = f
        self.bind("<Enter>", lambda e: self._hov(True))
        self.bind("<Leave>", lambda e: self._hov(False))
        self.bind("<Button-1>", lambda e: self._toggle())
        var.trace_add("write", lambda *_: self._paint())
        self._paint()

    def _hov(self, v):
        if self._hover != v:
            self._hover = v
            self._paint()

    def _toggle(self):
        self.var.set(not self.var.get())

    def _paint(self):
        self.delete("all")
        b, hgt = self._box, self._h
        on = bool(self.var.get())
        top = (hgt - b) / 2.0
        if on:
            self.create_rectangle(0.5, top + 0.5, b - 0.5, top + b - 0.5,
                                  fill=EF_SIGNAL, outline=EF_SIGNAL)
            self.create_line(3.5, top + b / 2.0, b * 0.42, top + b - 4,
                             b - 3.5, top + 4, fill=EF_ON_SIGNAL, width=2)
        else:
            self.create_rectangle(0.5, top + 0.5, b - 0.5, top + b - 0.5,
                                  fill=EF_PANEL,
                                  outline=EF_SIGNAL if self._hover else EF_RULE)
        self.create_text(b + 9, hgt / 2.0, text=self.text, anchor="w",
                         fill=EF_INK if on else EF_INK2, font=self._font)


class _EfScale(tk.Frame):
    """1px 轨道 + 实心方块把手。终末地不用圆点滑块，用方块。"""

    def __init__(self, master, var, lo, hi, fmt="%g", suffix="", width=190):
        super().__init__(master, bg=EF_PANEL)
        self.var, self.lo, self.hi = var, lo, hi
        self.fmt, self.suffix = fmt, suffix
        self.cv = tk.Canvas(self, width=width, height=22, bg=EF_PANEL,
                            highlightthickness=0, bd=0, cursor="hand2")
        self.cv.pack(side="left")
        self.lb = tk.Label(self, text="", bg=EF_PANEL, fg=EF_SIGNAL,
                           font=_ef_font(EF_MONO, 9, bold=True), width=6,
                           anchor="e")
        self.lb.pack(side="left", padx=(8, 0))
        self.cv.bind("<Button-1>", self._set_from_event)
        self.cv.bind("<B1-Motion>", self._set_from_event)
        self.cv.bind("<Configure>", lambda e: self._paint())
        var.trace_add("write", lambda *_: self._paint())
        self._paint()

    def _set_from_event(self, e):
        w = max(1, self.cv.winfo_width())
        pad = 6
        r = min(1.0, max(0.0, (e.x - pad) / float(max(1, w - pad * 2))))
        self.var.set(round(self.lo + (self.hi - self.lo) * r, 2))

    def _paint(self):
        self.cv.delete("all")
        w = self.cv.winfo_width() or int(self.cv["width"])
        h, pad = 22, 6
        y = h / 2.0
        try:
            frac = (float(self.var.get()) - self.lo) / float(self.hi - self.lo)
        except (ValueError, ZeroDivisionError):
            frac = 0.0
        frac = min(1.0, max(0.0, frac))
        x = pad + (w - pad * 2) * frac
        self.cv.create_line(pad, y, w - pad, y, fill=EF_RULE, width=1)
        self.cv.create_line(pad, y, x, y, fill=EF_SIGNAL, width=1)
        self.cv.create_rectangle(x - 3.5, y - 6.5, x + 3.5, y + 6.5,
                                 fill=EF_SIGNAL, outline=EF_SIGNAL)
        self.lb.configure(text=self.fmt % float(self.var.get()) + self.suffix)


class _EfChips(tk.Frame):
    """切角 chip 单选组。选中 = 实心 acid yellow（唯一"当前"表示）。"""

    def __init__(self, master, var, pairs, bg=EF_PAPER):
        super().__init__(master, bg=bg)
        self.var = var
        for val, txt in pairs:
            cv = tk.Canvas(self, height=24,
                           width=int(tkfont.Font(root=master,
                                                 font=_ef_font(EF_UI, 8)
                                                 ).measure(txt)) + 24,
                           bg=bg, highlightthickness=0, bd=0, cursor="hand2")
            cv._hover = False
            cv.pack(side="left", padx=(0, 5))

            def paint(c=cv, v=val, t=txt):
                c.delete("all")
                w, h, cut = int(c["width"]), 24, 6
                on = self.var.get() == v
                hov = c._hover
                pts = [0.5, 0.5, w - cut, 0.5, w - 0.5, cut + 0.5,
                       w - 0.5, h - 0.5, 0.5, h - 0.5]
                if on:
                    c.create_polygon(pts, smooth=False, fill=EF_SIGNAL,
                                     outline=EF_SIGNAL)
                    fg = EF_ON_SIGNAL
                else:
                    c.create_polygon(pts, smooth=False,
                                     fill=EF_CHIP if hov else bg,
                                     outline=EF_RULE)
                    fg = EF_INK if hov else EF_INK2
                c.create_text(w / 2.0, h / 2.0, text=t, fill=fg,
                              font=_ef_font(EF_UI, 8))

            cv.bind("<Enter>",
                    lambda e, c=cv, p=paint: (setattr(c, "_hover", True), p()))
            cv.bind("<Leave>",
                    lambda e, c=cv, p=paint: (setattr(c, "_hover", False), p()))
            cv.bind("<Button-1>", lambda e, v=val: self.var.set(v))
            self.var.trace_add("write", lambda *_, p=paint: p())
            paint()


# ---------------------------------------------------------------- 壁纸（zcode-beautify）
# 壁纸是 zcode-beautify 插件在管。直接改它的 config.json，不另存一份，
# 免得两边各记一份、谁都不知道以谁为准。插件没装就返回 None，面板隐藏这段。
WP_DIRS = [
    Path(os.environ.get("APPDATA", "")) / ".zcode/cli/plugins/data/zcode-beautify@zcode-beautify",
    Path.home() / ".zcode/cli/plugins/data/zcode-beautify@zcode-beautify",
]


def _wp_dir() -> Path | None:
    for d in WP_DIRS:
        if (d / "config.json").is_file():
            return d
    return None


def _wp_read() -> dict | None:
    """读插件配置；文件在但解析失败也返回 None（面板就显示不可用）。"""
    d = _wp_dir()
    if d is None:
        return None
    try:
        return json.loads((d / "config.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _wp_write(patch: dict) -> bool:
    """把 patch 并进插件配置。写失败返回 False，调用方负责提示。"""
    d = _wp_dir()
    if d is None:
        return False
    cur = _wp_read() or {}
    cur.update(patch)
    try:
        (d / "config.json").write_text(
            json.dumps(cur, ensure_ascii=False, indent=2), encoding="utf-8")
        return True
    except OSError as e:
        log("写壁纸配置失败", e)
        return False


def _wp_short(p: str) -> str:
    """配置里是绝对路径，面板上只显示文件名——既省地方也不暴露目录。"""
    return Path(p).name if p else "（未设置）"


BEAUTIFY_CLI = [
    Path(os.environ.get("APPDATA", "")) / ".zcode/cli/plugins/cache/zcode-beautify",
    Path.home() / ".zcode/cli/plugins/cache/zcode-beautify",
]


def _beautify_cli() -> Path | None:
    """找插件自带的 CLI 入口。目录带版本号（cache/zcode-beautify/zcode-beautify/0.3.3），
    所以按名字 glob 最新版，别写死版本——插件一升级写死的路径就废了。"""
    for root in BEAUTIFY_CLI:
        if not root.is_dir():
            continue
        cands = sorted(root.glob("zcode-beautify/*/dist/cli.js"),
                       key=lambda p: p.parts[-3])
        if cands:
            return cands[-1]
    return None


def _node_exe() -> str | None:
    """ZCode 自带 node（~/.zcode/nodejs/node），也认 PATH 上的。"""
    for p in (Path.home() / ".zcode/nodejs/node.exe",
              Path.home() / ".zcode/nodejs/node"):
        if p.is_file():
            return str(p)
    return shutil.which("node")


def _wp_refresh(port: int | None = None) -> bool:
    """让 zcode-beautify 立刻把壁纸/配色重新注入 ZCode。

    插件没有 MCP 工具可调，但它自带 CLI，`colors` 子命令就是"重放已存的
    壁纸与配色"，正好是换图之后需要的一步。走 CDP，端口取插件配置里那个
    （默认 9222，用户可能在客户端里改过）。失败就老实返回 False，
    让上层提示"重启后生效"，绝不假装成功。
    """
    cli, node = _beautify_cli(), _node_exe()
    if cli is None or not node:
        return False
    cmd = [node, str(cli), "colors"]
    if port:
        cmd += ["--port", str(port)]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30,
                           encoding="utf-8", errors="replace",
                           cwd=str(cli.parent.parent),
                           creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        out = ((r.stdout or "") + (r.stderr or "")).lower()
        return r.returncode == 0 and "error" not in out and "fail" not in out
    except (OSError, subprocess.SubprocessError) as e:
        log("重注入壁纸失败", e)
        return False


class SettingsDialog:
    def __init__(self, bar: HoverBar):
        self.bar = bar
        self.cfg = bar.cfg
        self._closed = False
        self._upd_q: queue.Queue = queue.Queue()
        top = tk.Toplevel(bar.root)
        self.top = top
        top.title("悬浮条设置")
        top.configure(bg=EF_PAPER)
        # 不要系统白标题栏：它跟终末地这套暗色完全不是一路。改成无边框 + 自绘标题行，
        # 拖动靠下面 bind 的 <B1-Motion>，关闭靠右侧的切角方块。
        top.overrideredirect(True)
        top.attributes("-topmost", True)
        top.resizable(False, False)
        top.protocol("WM_DELETE_WINDOW", self.close)
        # 面板自己的透明度：下次打开沿用上次的值
        try:
            top.attributes("-alpha", min(1.0, max(
                0.4, float(bar.cfg.get("panel_opacity", 1.0)))))
        except tk.TclError:
            pass

        # 顶部标题行：三色条 + 品牌 + 关闭。整段合成一块，不再有白色断层。
        bar_hdr = tk.Frame(top, bg=EF_CMD)
        bar_hdr.pack(fill="x")

        # 三色条贴在标题行顶边，只出现一次（终末地签名元素）
        tri = tk.Frame(bar_hdr, bg=EF_PAPER, height=3)
        tri.pack(fill="x")
        for col in (EF_TRI_A, EF_TRI_B, EF_TRI_C):
            tk.Frame(tri, bg=col, height=3).pack(side="left", fill="both",
                                                 expand=True)

        # 命令栏：品牌 + 面包屑，拉丁字母大写拉开
        cmd = tk.Frame(bar_hdr, bg=EF_CMD)
        cmd.pack(fill="x")
        mark = tk.Canvas(cmd, width=11, height=11, bg=EF_CMD,
                         highlightthickness=0, bd=0)
        mark.create_polygon([0, 0, 11, 0, 11, 4.4, 4.4, 11, 0, 11],
                            smooth=False, fill=EF_SIGNAL, outline="")
        mark.pack(side="left", padx=(16, 8), pady=12)
        tk.Label(cmd, text="Z C O D E   T O K E N", bg=EF_CMD, fg=EF_INK,
                 font=_ef_font(EF_UI, 9, bold=True)).pack(side="left")
        crumb = tk.Frame(cmd, bg=EF_CMD)
        crumb.pack(side="left", padx=(16, 0))
        tk.Label(crumb, text="B A R", bg=EF_CMD, fg="#8b8b8b",
                 font=_ef_font(EF_UI, 8)).pack(side="left")
        on = tk.Frame(crumb, bg=EF_SIGNAL)
        on.pack(side="left", padx=(10, 0))
        tk.Label(on, text="S E T T I N G S", bg=EF_SIGNAL, fg=EF_ON_SIGNAL,
                 font=_ef_font(EF_UI, 8, bold=True)).pack(padx=8, pady=3)

        # 关闭按钮：跟条面上的设置入口同一种语言——切角方块，悬停变酸黄。
        # 系统标题栏去掉之后，这是唯一的关闭入口（Esc 也绑了）。
        cl = tk.Canvas(cmd, width=22, height=22, bg=EF_CMD,
                       highlightthickness=0, bd=0, cursor="hand2")
        cl.pack(side="right", padx=(0, 14), pady=8)
        cut = 5
        pts = [2, 2, 20, 2, 20, 20 - cut, 20 - cut, 20, 2, 20]
        cl_box = cl.create_polygon(pts, smooth=False, fill=EF_CHIP, outline="")
        cl.create_line(8, 8, 14, 14, fill=EF_INK2, width=1.4)
        cl.create_line(14, 8, 8, 14, fill=EF_INK2, width=1.4)

        def cl_hover(on_):
            cl.itemconfigure(cl_box, fill=EF_SIGNAL if on_ else EF_CHIP)
            col = EF_ON_SIGNAL if on_ else EF_INK2
            for i in cl.find_all()[1:]:
                cl.itemconfigure(i, fill=col)

        cl.bind("<Enter>", lambda e: cl_hover(True))
        cl.bind("<Leave>", lambda e: cl_hover(False))
        cl.bind("<Button-1>", lambda e: self.close())

        # 无边框窗口没有系统拖动区，自己绑：按住标题行任意空白处拖。
        def hdr_press(e):
            self._drag_off = (e.x_root - top.winfo_x(), e.y_root - top.winfo_y())

        def hdr_move(e):
            off = getattr(self, "_drag_off", None)
            if off:
                top.geometry("+%d+%d" % (e.x_root - off[0], e.y_root - off[1]))

        for w in (cmd, crumb):
            w.bind("<Button-1>", hdr_press)
            w.bind("<B1-Motion>", hdr_move)
        top.bind("<Escape>", lambda e: self.close())

        # 内容区可滚动：125% 缩放下整套控件约 900 逻辑像素高，1080p 屏幕放不下，
        # 装进 Canvas + 竖向滚动条（细线风格，不用 tk 原生沟槽）。
        outer = tk.Frame(top, bg=EF_PAPER)
        outer.pack(fill="both", expand=True, padx=(20, 0), pady=(14, 0))
        self._scroll_cv = tk.Canvas(outer, bg=EF_PAPER, highlightthickness=0,
                                    bd=0)
        # body 必须挂在 canvas 自己下面：create_window 只把「canvas 的子控件」
        # 真正合成进画布。挂到 outer 的话它是一个独立的同级 HWND，会被 canvas
        # 盖住——控件、几何全对，整块正文却是一片空白。
        body = tk.Frame(self._scroll_cv, bg=EF_PAPER)
        self._scroll_cv.pack(side="left", fill="both", expand=True)
        self._scrollbar = tk.Canvas(outer, width=3, bg=EF_PAPER,
                                    highlightthickness=0, bd=0)
        self._scrollbar.pack(side="right", fill="y", padx=(6, 8))
        body_id = self._scroll_cv.create_window((0, 0), window=body,
                                                anchor="nw")
        body.bind("<Configure>", lambda e: self._sync_scroll(body_id))
        self._scroll_cv.bind("<Configure>", lambda e: self._sync_scroll(body_id))
        for w in (self._scroll_cv, body, self._scrollbar):
            w.bind("<MouseWheel>", self._on_wheel)

        tk.Label(body, text="C O N F I G U R A T I O N", bg=EF_PAPER,
                 fg=EF_INK2, font=_ef_font(EF_UI, 8)).pack(anchor="w")
        tk.Label(body, text="悬浮条设置", bg=EF_PAPER, fg=EF_INK,
                 font=_ef_font(EF_UI, 15, bold=True)).pack(anchor="w",
                                                           pady=(2, 0))
        tk.Label(body, text="拖动条面可直接移动；条最右端的方块就是设置入口。",
                 bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_UI, 9)).pack(anchor="w", pady=(3, 8))

        sec_no = [0]

        def section(cn, en):
            """分区标题：编号 + 中文 + 拉开字距的拉丁眉标 + 1px 通栏细线。"""
            sec_no[0] += 1
            head = tk.Frame(body, bg=EF_PAPER)
            head.pack(fill="x", pady=(10, 0))
            tk.Label(head, text="%02d" % sec_no[0], bg=EF_PAPER,
                     fg=EF_SIGNAL,
                     font=_ef_font(EF_MONO, 9, bold=True)
                     ).pack(side="left", padx=(0, 10))
            tk.Label(head, text=cn, bg=EF_PAPER, fg=EF_INK,
                     font=_ef_font(EF_UI, 10, bold=True)).pack(side="left")
            tk.Label(head, text=en, bg=EF_PAPER, fg=EF_INK2,
                     font=_ef_font(EF_UI, 8)).pack(side="right")
            tk.Frame(body, bg=EF_RULE, height=1).pack(fill="x", pady=(4, 8))

        def row(cn, note=None):
            """一行：左边说明（中文 + 小字注释），右边控件区。"""
            r = tk.Frame(body, bg=EF_PAPER)
            r.pack(fill="x", pady=3)
            left = tk.Frame(r, bg=EF_PAPER)
            left.pack(side="left", fill="x", expand=True)
            tk.Label(left, text=cn, bg=EF_PAPER, fg=EF_INK,
                     font=_ef_font(EF_UI, 9)).pack(anchor="w")
            if note:
                tk.Label(left, text=note, bg=EF_PAPER, fg=EF_INK2,
                         font=_ef_font(EF_UI, 8)).pack(anchor="w")
            right = tk.Frame(r, bg=EF_PAPER)
            right.pack(side="right")
            return right

        # ---- 01 位置 ----
        section("位置", "P O S I T I O N")
        self.v_anchor = tk.StringVar(value=self.cfg["anchor"])
        _EfChips(row("贴靠", "相对 ZCode 窗口或整块屏幕"), self.v_anchor,
                 [(a, ANCHOR_TXT[a]) for a in ANCHORS if a != "free"]
                 ).pack(anchor="e")

        self.v_scope = tk.StringVar(value=self.cfg["scope"])
        _EfChips(row("范围", "跟随窗口 = 只贴 ZCode；整块屏幕 = 贴显示器"),
                 self.v_scope,
                 [("window", "跟随窗口"), ("screen", "整块屏幕")]
                 ).pack(anchor="e")

        self.v_ox = tk.IntVar(value=int(self.cfg["offset_x"]))
        self.v_oy = tk.IntVar(value=int(self.cfg["offset_y"]))
        r = row("边距", "左右 / 上下，单位 px")
        for i, (v, tip) in enumerate(((self.v_ox, "左右"), (self.v_oy, "上下"))):
            sp = tk.Spinbox(r, from_=0, to=600, textvariable=v, width=5,
                            bg=EF_CHIP, fg=EF_INK, insertbackground=EF_SIGNAL,
                            buttonbackground=EF_CHIP, relief="flat", bd=0,
                            highlightthickness=1,
                            highlightbackground=EF_RULE,
                            highlightcolor=EF_SIGNAL,
                            font=_ef_font(EF_MONO, 9))
            sp.pack(side="left", padx=(0, 8))
            tk.Label(r, text=tip, bg=EF_PAPER, fg=EF_INK2,
                     font=_ef_font(EF_UI, 8)).pack(side="left", padx=(0, 12))

        # 钉住：与条面左端图钉是同一个状态。勾上 = 显示停在当前会话 +
        # 位置冻结；改贴靠/范围/边距后应用会按新锚点重新钉。
        # 勾选框独立成行——塞进 row 右列会被长注释挤出面板边缘
        self.v_pin = tk.BooleanVar(value=bool(self.cfg["pinned"]))
        row("钉住", "锁住当前位置；显示停在钉住那一刻的会话，不再跟着最近活跃的会话跳")
        _EfCheck(body, "钉住条面（等于条面左端的图钉）", self.v_pin,
                 big=True).pack(anchor="w", pady=(0, 4))

        # ---- 02 外观 ----
        section("外观", "A P P E A R A N C E")
        self.v_op = tk.DoubleVar(value=float(self.cfg["opacity"]))
        _EfScale(row("透明度", "拖动即时预览，最终值点\u201c应用\u201d才落盘"),
                 self.v_op, 0.30, 1.0, fmt="%.2f").pack(anchor="e")

        self.v_fs = tk.IntVar(value=int(self.cfg["font_px"]))
        _EfScale(row("字号", "px；上下文微进度条会一起缩放"),
                 self.v_fs, 10, 22, fmt="%d", suffix="px").pack(anchor="e")

        # ---- 03 显示项 ----
        section("显示项", "E L E M E N T S")
        self.v_show = {}
        grid = tk.Frame(body, bg=EF_PAPER)
        grid.pack(fill="x", pady=(0, 2))
        names = [("speed", "速度"), ("ctx", "上下文"), ("turn", "本轮"),
                 ("session", "会话"), ("today", "今日"), ("tools", "工具"),
                 ("cache", "缓存率")]
        for i, (k, txt) in enumerate(names):
            v = tk.BooleanVar(value=bool(self.cfg["show"].get(k, True)))
            self.v_show[k] = v
            _EfCheck(grid, txt, v).grid(row=i // 3, column=i % 3,
                                        sticky="w", padx=(0, 14), pady=2)

        self.v_lab = tk.BooleanVar(value=bool(self.cfg["labels"]))
        _EfCheck(body, "显示文字标签（速度 / 上下文 / 本轮 …）", self.v_lab,
                 big=True).pack(anchor="w", pady=(6, 2))

        # ---- 04 面板 / 壁纸 ----
        section("面板与壁纸", "P A N E L   +   W A L L P A P E R")
        self.v_pop = tk.DoubleVar(value=float(self.cfg.get("panel_opacity", 1.0)))
        _EfScale(row("面板透明度", "这个设置面板自己；调到 1.00 就是纯不透明"),
                 self.v_pop, 0.55, 1.0, fmt="%.2f").pack(anchor="e")
        self.v_pop.trace_add("write", lambda *_: self._preview_panel())

        # 壁纸：读 zcode-beautify 插件的数据目录，跟它共用同一份配置，
        # 不另存一份以免两边打架。插件没装就整段隐藏。
        self._wp = _wp_read()
        if self._wp is not None:
            self.v_wp_vis = tk.BooleanVar(value=bool(self._wp.get("wallpaperVisible", True)))
            _EfCheck(body, "显示壁纸（关掉后 ZCode 回到纯色背景）", self.v_wp_vis,
                     big=True).pack(anchor="w", pady=(2, 4))
            self.v_wp_dim = tk.DoubleVar(value=float(self._wp.get("dim", 25)))
            _EfScale(row("壁纸压暗", "越高壁纸越沉，字越清楚"),
                     self.v_wp_dim, 0, 100, fmt="%.0f").pack(anchor="e")
            self.v_wp_blur = tk.DoubleVar(value=float(self._wp.get("blur", 0)))
            _EfScale(row("壁纸模糊", "px；界面会更糊但更安静"),
                     self.v_wp_blur, 0, 40, fmt="%.0f").pack(anchor="e")
            r_pick = row("换壁纸", "jpg / png，选完立刻生效")
            _EfButton(r_pick, "选择图片…", self._pick_wallpaper).pack(side="left")
            cur = tk.Label(r_pick, text=_wp_short(self._wp.get("wallpaperPath", "")),
                           bg=EF_PAPER, fg=EF_INK2,
                           font=_ef_font(EF_UI, 8))
            cur.pack(side="left", padx=(10, 0))
            self._wp_label = cur
            tk.Label(body, text="壁纸由 zcode-beautify 插件接管，这里改的是同一份配置。",
                     bg=EF_PAPER, fg=EF_INK2,
                     font=_ef_font(EF_UI, 8)).pack(anchor="w", pady=(2, 4))
        else:
            tk.Label(body, text="没找到 zcode-beautify 插件的数据目录，壁纸设置不可用。",
                     bg=EF_PAPER, fg=EF_INK2,
                     font=_ef_font(EF_UI, 8)).pack(anchor="w", pady=(0, 4))

        # ---- 05 行为 ----
        section("行为", "B E H A V I O U R")
        self.v_top = tk.BooleanVar(value=bool(self.cfg["always_on_top"]))
        _EfCheck(body, "总是置顶（会盖住其它程序）", self.v_top,
                 big=True).pack(anchor="w", pady=2)
        tk.Label(body, text="默认不勾：靠 Windows 属主窗口绑定，只浮在 ZCode 上，"
                            "随它最小化一起让路。",
                 bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_UI, 8)).pack(anchor="w", pady=(0, 6))

        self.v_hide = tk.BooleanVar(value=bool(self.cfg["hide_when_inactive"]))
        _EfCheck(body, "ZCode 不在前台时隐藏", self.v_hide,
                 big=True).pack(anchor="w", pady=(0, 4))

        # ---- 06 更新 ----
        section("更新", "U P D A T E")
        # 当前版本号直接读 git：没有 tag 体系，短 hash 就是最好的版本标识
        self._upd_local = local_commit() or "（非 git 安装）"
        r = row("当前版本", "git 短 hash；没有就是解压使用时不在版本控制里")
        tk.Label(r, text=self._upd_local, bg=EF_PAPER, fg=EF_INK,
                 font=_ef_font(EF_MONO, 9, bold=True)).pack(side="left",
                                                            padx=(0, 10))
        self._upd_btn = _EfButton(r, "检查更新", self._upd_check)
        self._upd_btn.pack(side="left")
        self._upd_msg = tk.Label(body, text="", bg=EF_PAPER, fg=EF_INK2,
                                 font=_ef_font(EF_UI, 8))
        self._upd_msg.pack(anchor="w", pady=(2, 4))

        self.v_auto = tk.BooleanVar(
            value=bool(self.cfg.get("auto_update", False)))
        _EfCheck(body, "启动时自动检查（只提示，不自动安装）", self.v_auto,
                 big=True).pack(anchor="w", pady=2)
        tk.Label(body, text="勾上后每次启动悄悄问一次 GitHub：有新版本就在本区和"
                            "右键菜单里提示，点不点由你定。默认不勾。",
                 bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_UI, 8)).pack(anchor="w", pady=(0, 4))

        # ---- 07 状态 ----
        # 面板是在运行中的实例里打开的，所以"悬浮条进程"这一行必然是运行中——
        # 它的价值在于给出 PID，以及配合下面三行一起判断"哪儿断了"。
        section("状态", "S T A T U S")
        st = tk.Frame(body, bg=EF_PAPER)
        st.pack(fill="x", pady=(0, 2))
        self._st_labels = {}
        for key, cn in (("run", "悬浮条进程"), ("data", "数据链路"),
                        ("zcode", "ZCode 窗口"), ("auto", "开机自启")):
            rr = tk.Frame(st, bg=EF_PAPER)
            rr.pack(fill="x", pady=1)
            tk.Label(rr, text=cn, bg=EF_PAPER, fg=EF_INK2, width=11,
                     anchor="w",
                     font=_ef_font(EF_UI, 9)).pack(side="left")
            v = tk.Label(rr, text="—", bg=EF_PAPER, fg=EF_INK,
                         font=_ef_font(EF_MONO, 9))
            v.pack(side="left")
            self._st_labels[key] = v

        r_st = tk.Frame(body, bg=EF_PAPER)
        r_st.pack(fill="x", pady=(4, 2))
        _EfButton(r_st, "刷新", self._st_refresh).pack(side="left")
        _EfButton(r_st, "重启悬浮条", self._st_restart).pack(side="left",
                                                              padx=(8, 0))
        self._st_msg = tk.Label(body, text="", bg=EF_PAPER, fg=EF_INK2,
                                font=_ef_font(EF_UI, 8))
        self._st_msg.pack(anchor="w", pady=(2, 4))

        # 自启开关即时生效：写的是注册表 Run 项，属于系统级状态，
        # 不该等「应用」——而且取消自启这种操作，用户期望点了就走。
        self.v_autostart = tk.BooleanVar(value=self._st_autostart_on())
        _EfCheck(body, "开机自动启动悬浮条（写当前用户 Run 项）",
                 self.v_autostart, big=True).pack(anchor="w", pady=(0, 2))
        self.v_autostart.trace_add("write", lambda *_: self._st_set_autostart())
        tk.Label(body, text="只用当前用户权限，不需要管理员。路径写在注册表里，"
                            "换了 Python 或搬了目录会失效——上面那行会标红提醒。",
                 bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_UI, 8)).pack(anchor="w", pady=(0, 4))

        # ---- 底部按钮 ----
        tk.Frame(top, bg=EF_RULE, height=1).pack(fill="x", pady=(12, 0))
        foot = tk.Frame(top, bg=EF_PANEL)
        foot.pack(fill="x")
        inner = tk.Frame(foot, bg=EF_PANEL)
        inner.pack(fill="x", padx=20, pady=12)
        _EfButton(inner, "恢复默认", self.reset).pack(side="left")
        _EfButton(inner, "应用", self.apply, primary=True).pack(side="right")
        _EfButton(inner, "关闭", self.close).pack(side="right", padx=(0, 8))

        # 透明度/字号拖动即生效：先在条上看效果，再决定要不要"应用"
        for v in (self.v_op, self.v_fs):
            v.trace_add("write", lambda *_: self._preview())

        # Tk 的 <MouseWheel> 不向上冒泡：鼠标停在标签、滑块、勾选框上时，
        # 事件只发给那个控件。所以整棵子树都要绑一遍，否则只有在空白处
        # 滚得动，正文一长出视口就像"卡住"。
        self._bind_wheel(body)

        top.update_idletasks()
        # Canvas 不会把内嵌 body 的尺寸传上去，得自己算：
        # 足够高就全长显示，超过屏幕 86% 就封顶，剩下的靠滚动。
        bh = body.winfo_reqheight()
        bw = body.winfo_reqwidth()
        maxh = max(360, int(top.winfo_screenheight() * 0.86))
        chrome = top.winfo_reqheight() - self._scroll_cv.winfo_reqheight()
        vh = min(bh, max(280, maxh - chrome))
        self._scroll_cv.configure(width=bw, height=vh)
        top.update_idletasks()
        self._sync_scroll(None)
        self._place_near_pointer()
        self._st_refresh()          # 状态区先填一次，别让用户看到四行 —
        # 面板开着时用户就是在操作悬浮条，别让 hide_when_inactive 把条藏了
        self._own_hwnd = register_own_window(top)
        # 后台 git 调用的结果回主线程：起泵，跟着面板同生共死
        self._upd_pump()

    def _unregister_own_window(self):
        unregister_own_window(getattr(self, "_own_hwnd", 0))
        self._own_hwnd = 0

    def _sync_scroll(self, body_id):
        """内容比视口高时，画出可滚动的细线指示条。"""
        cv, sb = self._scroll_cv, self._scrollbar
        if body_id is None:
            items = cv.find_all()
            if not items:
                return
            body_id = items[0]
        h = cv.winfo_height()
        total = cv.bbox("all")
        if not total:
            return
        th = total[3]
        # scrollregion 不设的话 yview 永远是 (0.0, 1.0)，滚轮滚不动。
        cv.configure(scrollregion=(0, 0, total[2], th))
        sb.delete("all")
        if th <= h + 1:
            return
        frac = h / float(th)
        top_y = max(0.0, min(1.0, -cv.canvasy(0) / float(max(1, th - h))))
        sh = sb.winfo_height() or h          # 滚动条是 fill="y" 撑开的，高度取实际值
        y1 = top_y * sh
        y2 = y1 + max(18.0, sh * frac)
        sb.create_rectangle(1, y1, 2, y2, fill=EF_SIGNAL, outline=EF_SIGNAL)

    def _bind_wheel(self, w):
        """把滚轮事件绑到整棵子树。新增控件时再调一次即可。"""
        w.bind("<MouseWheel>", self._on_wheel)
        for c in w.winfo_children():
            self._bind_wheel(c)

    def _on_wheel(self, e):
        cv = self._scroll_cv
        total = cv.bbox("all")
        if not total or total[3] <= cv.winfo_height() + 1:
            return
        cv.yview_scroll(int(-1 * (e.delta / 120)) * 3, "units")
        self.top.after_idle(lambda: self._sync_scroll(None))

    def _place_near_pointer(self):
        """把面板摆在鼠标附近，但不许出屏（125% 缩放下很容易掉底边）。"""
        top = self.top
        w, h = top.winfo_reqwidth(), top.winfo_reqheight()
        px, py = top.winfo_pointerx(), top.winfo_pointery()
        sw, sh = top.winfo_screenwidth(), top.winfo_screenheight()
        x = min(max(8, px - w // 2), max(8, sw - w - 8))
        y = min(max(8, py - 40), max(8, sh - h - 48))
        top.geometry("+%d+%d" % (x, y))

    def _preview(self):
        """只实时预览外观两项，不落盘；其余项点"应用"才生效。"""
        try:
            self.bar.root.attributes(
                "-alpha", min(1.0, max(0.25, float(self.v_op.get()))))
            px = max(9, int(round(float(self.v_fs.get()) * SCALE)))
            self.bar.font.configure(size=-px)
            self.bar._px = px
            self.bar._draw()
        except (tk.TclError, ValueError):
            pass

    def _preview_panel(self):
        """面板自己的透明度也做实时预览，边拖边看。"""
        try:
            a = min(1.0, max(0.4, float(self.v_pop.get())))
            self.top.attributes("-alpha", a)
        except (tk.TclError, ValueError):
            pass

    def _pick_wallpaper(self):
        """选一张图交给 zcode-beautify，写完立刻让它重注入，不用重启客户端。"""
        from tkinter import filedialog
        p = filedialog.askopenfilename(
            parent=self.top, title="选择壁纸",
            filetypes=[("图片", "*.jpg *.jpeg *.png *.webp *.bmp"),
                       ("全部文件", "*.*")])
        if not p:
            return
        if not _wp_write({"wallpaperPath": p, "wallpaperVisible": True}):
            messagebox.showerror("保存失败", "写不进 zcode-beautify 的配置目录。",
                                 parent=self.top)
            return
        try:
            self._wp_label.configure(text=_wp_short(p))
        except tk.TclError:
            pass
        self._wp = _wp_read() or self._wp
        if not _wp_refresh(self._wp.get("port")):
            messagebox.showwarning(
                "已保存，但没能立刻生效",
                "壁纸路径已写入配置。要让 ZCode 立刻换图，重启一次客户端，"
                "或在插件设置里再点一次应用。", parent=self.top)

    # -- 状态 / 自启 --
    def _st_autostart_on(self) -> bool:
        """读 HKCU Run 项。只认我们自己那个名字，别把别的软件算进来。"""
        try:
            import winreg
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                                r"Software\Microsoft\Windows\CurrentVersion\Run") as k:
                winreg.QueryValueEx(k, "ZCodeTokenBar")
            return True
        except (OSError, ImportError):
            return False

    def _st_set_autostart(self):
        """勾选即时写注册表。失败就把勾退回原样并说清楚原因，
        不能让它显示"已开"而注册表里其实没有。"""
        want = bool(self.v_autostart.get())
        if want == self._st_autostart_on():
            return
        try:
            import winreg
            key = r"Software\Microsoft\Windows\CurrentVersion\Run"
            if want:
                exe = Path(sys.executable)
                pw = exe.with_name("pythonw.exe")
                py = pw if pw.is_file() else exe
                cmd = '"%s" "%s"' % (py, HERE / "hoverbar.py")
                with winreg.CreateKey(winreg.HKEY_CURRENT_USER, key) as k:
                    winreg.SetValueEx(k, "ZCodeTokenBar", 0, winreg.REG_SZ, cmd)
                self._st_say("已开启开机自启。")
            else:
                with winreg.OpenKey(winreg.HKEY_CURRENT_USER, key, 0,
                                    winreg.KEY_SET_VALUE) as k:
                    winreg.DeleteValue(k, "ZCodeTokenBar")
                self._st_say("已取消开机自启。")
        except (OSError, ImportError) as e:
            self._st_say("改注册表失败：%s" % e, EF_DANGER)
            try:
                self.v_autostart.set(not want)      # 退回原样
            except tk.TclError:
                pass
            return
        self._st_refresh()

    def _st_say(self, text, fg=None):
        try:
            self._st_msg.configure(text=text, fg=fg or EF_INK2)
        except tk.TclError:
            pass

    def _st_set(self, key, text, fg=None):
        lb = getattr(self, "_st_labels", {}).get(key)
        if lb is None:
            return
        try:
            lb.configure(text=text, fg=fg or EF_INK)
        except tk.TclError:
            pass

    def _st_refresh(self):
        """四行状态，各查各的，任何一项查不出来都只影响自己那一行。"""
        # 1) 悬浮条进程：面板就活在这个进程里，能打开就说明在跑
        self._st_set("run", "运行中  PID %d" % os.getpid(), OK)
        # 2) 数据链路：serve 子进程还活着吗、有没有拿到快照
        src = getattr(self.bar, "src", None)
        if src is not None and src.alive():
            if self.bar.data:
                s = (self.bar.data.get("session") or {})
                sid = (s.get("sid") or "")[-8:] or "—"
                self._st_set("data", "正常  会话 %s" % sid, OK)
            else:
                self._st_set("data", "已连接，还没取到数据", WARM)
        else:
            self._st_set("data", "断开（serve 子进程没起来）", HOT)
        # 3) ZCode 窗口：找不到窗口条面会主动隐藏，最常见的"条不见了"原因。
        #    还要区分"窗口在但条面被故意藏了"——那是失焦隐藏生效，
        #    不是丢了；不写清楚用户会以为是 bug。
        try:
            hwnd = zcode_hwnd()
        except Exception:
            hwnd = 0
        hidden = not self.bar._visible
        if hwnd and hidden and self.bar.cfg.get("hide_when_inactive"):
            self._st_set("zcode",
                         "已找到，但条面因失焦隐藏（见「05 行为」那项）", WARM)
        elif hwnd and hidden:
            self._st_set("zcode", "已找到 HWND %d，条面当前不可见" % hwnd, WARM)
        elif hwnd:
            self._st_set("zcode", "已找到  HWND %d" % hwnd, OK)
        else:
            self._st_set("zcode", "没找到（客户端没开？条面会主动隐藏）", HOT)
        # 4) 开机自启：不只是"注册表有值"，还要验目标路径还在
        try:
            import winreg
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                                r"Software\Microsoft\Windows\CurrentVersion\Run") as k:
                cmd = winreg.QueryValueEx(k, "ZCodeTokenBar")[0]
            try:
                target = Path(cmd.split('"')[1])
                alive_path = target.is_file()
            except (IndexError, OSError):
                alive_path = False
            if alive_path:
                self._st_set("auto", "已开", OK)
            else:
                self._st_set("auto", "已开，但解释器路径失效", HOT)
        except (OSError, ImportError, TypeError):
            self._st_set("auto", "未开", EF_INK2)

    def _st_restart(self):
        """起一个新实例，然后本进程退出。先起后杀，中间留一拍让新实例
        把锁文件写进去——顺序反了旧实例会把新实例的锁一起删掉。"""
        if not relaunch():
            self._st_say("新实例起不来，手动跑一次 hoverbar.py。", EF_DANGER)
            return
        self._st_say("已拉起新实例，本窗口马上关闭。")
        self.bar.root.after(400, self.bar.quit)

    # -- 更新 --
    def _upd_say(self, text, fg=None):
        """更新区那一行状态字。按钮文字单独控制，避免和正文提示缠在一起。"""
        try:
            self._upd_msg.configure(text=text, fg=fg or EF_INK2)
        except tk.TclError:
            pass

    def _upd_button(self, text, cmd):
        """就地换一个按钮：文字、动作都能改，不用重建整个面板。"""
        old = self._upd_btn
        self._upd_btn = _EfButton(old.master, text, cmd, primary=True)
        self._upd_btn.pack(side="left")
        try:
            old.destroy()
        except tk.TclError:
            pass

    def _upd_pump(self):
        """后台线程的结果回主线程的唯一入口。

        不用 root.after(0, ...)：跨线程注册在 Tk 循环没跑的时候会静默失败，
        按钮就永远卡在「检查中…」。改用队列 + 200ms 轮询，循环在不在都丢不了。
        """
        try:
            while True:
                fn, a = self._upd_q.get_nowait()
                fn(*a)
        except queue.Empty:
            pass
        except tk.TclError:
            return                      # 面板已销毁，别再排下一轮
        if not self._closed:
            self._upd_job = self.top.after(200, self._upd_pump)

    def _upd_check(self):
        """联网问一次远端 HEAD。全程在后台线程里跑，界面不卡。"""
        self._upd_say("正在连接 GitHub…")
        self._upd_button("检查中…", lambda: None)

        def work():
            remote = remote_commit()
            local = local_commit()
            self._upd_q.put((self._upd_done, (remote, local)))

        threading.Thread(target=work, daemon=True).start()

    def _upd_done(self, remote, local):
        if not remote:
            self._upd_say("连不上 GitHub（或不是 git 安装）。检查网络后重试。",
                          EF_DANGER)
            self._upd_button("重试", self._upd_check)
            return
        if remote == local:
            self._upd_say("已是最新 %s。" % local)
            self._upd_button("检查更新", self._upd_check)
            return
        self._upd_say("有新版本：%s → %s。点右侧按钮拉取，"
                      "重启后生效。" % (local, remote), EF_SIGNAL)
        self._upd_button("立即更新", self._upd_apply)

    def _upd_apply(self):
        """真的拉。拉之前把设置存了——更新要重启，没保存的设置会丢。"""
        try:
            self._collect()
            save_settings(self.cfg)
        except (ValueError, tk.TclError):
            pass
        if repo_dirty():
            self._upd_say("项目里有未提交的改动，先提交或 stash 再更新。",
                          EF_DANGER)
            return
        self._upd_say("正在拉取…")
        self._upd_button("更新中…", lambda: None)

        def work():
            ok, msg = do_update()
            self._upd_q.put((self._upd_finish, (ok, msg)))

        threading.Thread(target=work, daemon=True).start()

    def _upd_finish(self, ok, msg):
        if ok:
            self._upd_say(msg)
            self._upd_button("重启生效", self._upd_restart)
        else:
            self._upd_say(msg, EF_DANGER)
            self._upd_button("重试", self._upd_apply)

    def _upd_restart(self):
        if relaunch():
            self.bar.quit()
        else:
            self._upd_say("起不来新实例，手动重启一下悬浮条。", EF_DANGER)

    def _collect(self):
        c = self.cfg
        c["anchor"] = self.v_anchor.get()
        c["scope"] = self.v_scope.get()
        c["offset_x"] = int(self.v_ox.get())
        c["offset_y"] = int(self.v_oy.get())
        c["opacity"] = round(float(self.v_op.get()), 2)
        c["font_px"] = int(self.v_fs.get())
        c["labels"] = bool(self.v_lab.get())
        c["always_on_top"] = bool(self.v_top.get())
        c["hide_when_inactive"] = bool(self.v_hide.get())
        c["panel_opacity"] = round(float(self.v_pop.get()), 2)
        c["auto_update"] = bool(self.v_auto.get())
        for k, v in self.v_show.items():
            c["show"][k] = bool(v.get())
        c["pinned"] = bool(self.v_pin.get())

    def apply(self):
        # 位置类设置有没有动过：动了且处于钉住状态，就要按新锚点重新钉
        pos_keys = ("anchor", "scope", "offset_x", "offset_y")
        before = {k: self.cfg.get(k) for k in pos_keys}
        was_pinned = bool(self.cfg.get("pinned"))
        self._collect()
        repin = any(self.cfg[k] != before[k] for k in pos_keys)
        if self.cfg["pinned"] and not was_pinned:
            # 面板里勾上钉住：和点条面左端图钉同一套语义——
            # 钉住当前会话 + 冻结在当下位置；没有会话数据就钉不上
            sid = ((self.bar.data or {}).get("session") or {}).get("sid") or ""
            if sid:
                self.cfg["pinned_sid"] = sid
                self.cfg["pin_x"] = self.bar.root.winfo_x()
                self.cfg["pin_y"] = self.bar.root.winfo_y()
            else:
                self.cfg["pinned"] = False
                self.v_pin.set(False)
        elif not self.cfg["pinned"]:
            self.cfg["pinned_sid"] = ""
            self.cfg["pin_x"] = self.cfg["pin_y"] = -1
        save_settings(self.cfg)
        self.bar.apply_settings(repin=repin)
        if self._wp is not None:
            patch = {"wallpaperVisible": bool(self.v_wp_vis.get()),
                     "dim": round(float(self.v_wp_dim.get())),
                     "blur": round(float(self.v_wp_blur.get()))}
            if _wp_write(patch):
                self._wp.update(patch)
                self._wp = _wp_read() or self._wp
                if not _wp_refresh(self._wp.get("port")):
                    log("壁纸配置已写入，但未能立刻重注入")
        # 应用 = 确认：落盘即收，不留着一个已经没事可做的面板
        self.close()

    def reset(self):
        self.cfg.update(json.loads(json.dumps(DEFAULTS)))
        save_settings(self.cfg)
        self.bar.apply_settings()
        self.close()
        self.bar.open_settings()

    def close(self):
        self._closed = True
        self._unregister_own_window()
        job = getattr(self, "_upd_job", None)
        if job:
            try:
                self.top.after_cancel(job)
            except (tk.TclError, ValueError):
                pass
        try:
            self.top.destroy()
        except tk.TclError:
            pass
        if getattr(self.bar, "_dlg", None) is self:
            self.bar._dlg = None


# ---------------------------------------------------------------- 更新
# 用户自己决定要不要更：默认只在启动后静默问一次 GitHub，有新版就在设置面板
# 「06 更新」里摆出来，点不点由用户定。绝不在后台偷偷 pull——那会在用户
# 编辑 hoverbar.py 的时候把他的改动冲掉。
def _git(args, timeout=20):
    """在项目目录跑 git。返回 (rc, stdout)。stderr 合进 stdout，调用方自己判。"""
    try:
        r = subprocess.run(["git"] + args, cwd=str(HERE), capture_output=True,
                           text=True, encoding="utf-8", errors="replace",
                           timeout=timeout,
                           creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        return r.returncode, (r.stdout or "") + (r.stderr or "")
    except (OSError, subprocess.SubprocessError) as e:
        return 1, str(e)


def git_available() -> bool:
    rc, out = _git(["--version"])
    return rc == 0


def local_commit() -> str:
    """本地 HEAD 短 hash；不在 git 仓库里就返回空串。"""
    rc, out = _git(["rev-parse", "--short", "HEAD"])
    return out.strip() if rc == 0 else ""


def repo_dirty() -> bool:
    """有未提交改动就不许自动更新——pull 会冲掉用户自己改的东西。"""
    rc, out = _git(["status", "--porcelain"])
    return rc == 0 and bool(out.strip())


def remote_commit() -> str:
    """远端 HEAD 短 hash。只 fetch 那一个 ref，不拉全历史。"""
    rc, out = _git(["ls-remote", "origin", "HEAD"], timeout=25)
    if rc != 0:
        return ""
    line = (out.strip().splitlines() or [""])[0]
    return line.split("\t")[0][:12] if line else ""


def behind_count() -> int:
    """本地落后远端几个提交。没有 upstream 或 fetch 失败返回 -1（未知）。"""
    _git(["fetch", "origin", "HEAD"], timeout=40)
    rc, out = _git(["rev-list", "--count", "HEAD..FETCH_HEAD"])
    if rc != 0:
        return -1
    try:
        return int(out.strip())
    except ValueError:
        return -1


def do_update() -> tuple:
    """拉到最新。返回 (ok, 人话说明)。

    只在工作区干净时真拉；有本地改动时说明情况并放弃——宁可让用户自己决定
    要不要先提交，也不能替他覆盖掉。
    """
    if repo_dirty():
        return False, ("项目里有未提交的改动，先提交或 stash 再更新，"
                       "免得把你的修改冲掉。")
    _git(["fetch", "origin", "HEAD"], timeout=40)
    rc, out = _git(["merge", "--ff-only", "FETCH_HEAD"], timeout=60)
    if rc != 0:
        head = (out.strip().splitlines() or [""])[0]
        return False, "拉不下来（%s）。" % head if head else "拉不下来，看日志。"
    rc, out = _git(["rev-parse", "--short", "HEAD"])
    return True, "已更新到 %s，重启悬浮条后生效。" % (out.strip()[:8] or "新版")


def relaunch() -> bool:
    """起一个新实例然后退出当前这个。Windows 上先起后杀，中间留 800ms
    让新进程把锁文件写进去，否则旧实例退出时会把新实例的锁一起删掉。"""
    try:
        subprocess.Popen(
            [sys.executable, str(HERE / "hoverbar.py")],
            cwd=str(HERE),
            creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
            | getattr(subprocess, "DETACHED_PROCESS", 0)
            | getattr(subprocess, "CREATE_NO_WINDOW", 0))
    except OSError as e:
        log("重启失败", e)
        return False
    return True


def cli_check_update() -> int:
    """--check-update：只问，不动。返回码 0=已最新，1=有新版本，2=查不了。"""
    if not git_available():
        print("找不到 git。更新功能需要 git；也可以直接重新下载解压覆盖。")
        return 2
    local = local_commit()
    if not local:
        print("这个目录不在 git 版本控制里（解压使用），无法比对版本。")
        return 2
    print("本地版本 :", local)
    print("正在连接 GitHub…")
    remote = remote_commit()
    if not remote:
        print("连不上 GitHub。检查网络后重试。")
        return 2
    print("远端版本 :", remote)
    if remote == local:
        print("已是最新。")
        return 0
    print("有新版本：%s → %s" % (local, remote))
    print("执行 python hoverbar.py --update 拉取，或重开悬浮条在设置面板「06 更新」里点。")
    return 1


def cli_update() -> int:
    """--update：拉取。有本地改动就拒绝，不覆盖。"""
    if not git_available():
        print("找不到 git，无法自动更新。")
        return 2
    ok, msg = do_update()
    print(msg)
    return 0 if ok else 1


# ---------------------------------------------------------------- CLI
def alive(pid: int) -> bool:
    try:
        out = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/NH"],
                             capture_output=True, text=True,
                             encoding="mbcs", errors="replace").stdout
        return str(pid) in out
    except Exception:
        return False


def running_pid() -> int | None:
    try:
        pid = int(LOCK.read_text().strip())
    except (OSError, ValueError):
        return None
    if alive(pid):
        return pid
    try:
        LOCK.unlink()
    except OSError:
        pass
    return None


def send(msg: str) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", IPC_PORT), timeout=2) as s:
            s.sendall(msg.encode("utf-8"))
        return True
    except OSError:
        return False


def once():
    """排查用：直接跑一次 json。"""
    try:
        p = subprocess.run([sys.executable, str(ZUSAGE), "json"],
                           capture_output=True, timeout=20,
                           creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        d = json.loads(p.stdout.decode("utf-8", "replace"))
    except Exception as e:
        print("查询失败：", e)
        return 1
    s, lt, last, today = (d.get("session") or {}, d.get("last_turn") or {},
                          d.get("last") or {}, d.get("today") or {})
    print("会话总量 :", fmt(s.get("total")))
    print("本轮     :", fmt(lt.get("total")))
    print("速度     : %.1f t/s" % (last.get("tps") or 0))
    print("上下文   : %.1f%%" % (s.get("ctx", 0) / max(1, d.get("context_window") or 1) * 100))
    print("今日     :", fmt(today.get("total")))
    print("DPI 缩放 : %.3f" % SCALE)
    return 0


def main(argv):
    global DEBUG
    DEBUG = "--debug" in argv
    if "--once" in argv:
        return once()
    if "--quit" in argv:
        if send("quit"):
            print("已通知退出")
            return 0
        pid = running_pid()
        if pid:
            try:
                os.kill(pid, 9)
                print("已关闭", pid)
                return 0
            except OSError as e:
                print("关闭失败", e)
                return 1
        print("没有正在运行的悬浮条")
        return 0
    if "--settings" in argv:
        if send("settings"):
            return 0
        print("悬浮条未运行，先启动：python hoverbar.py")
        return 1
    if "--check-update" in argv:
        return cli_check_update()
    if "--update" in argv:
        return cli_update()

    if running_pid():
        if not send("settings"):
            print("悬浮条已在运行（右键条面可关）")
        return 0
    if not ZUSAGE.is_file():
        print("找不到 zusage.py：", ZUSAGE)
        return 1
    HoverBar(load_settings()).run()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
