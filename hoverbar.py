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
    python hoverbar.py --wait-zcode # 待命直到 ZCode 出现再开条（自启用）
    python hoverbar.py --debug      # 开窗并打印定位/取数过程

打开设置（三种等价入口）：点条面最右端的方框按钮（悬停反白）、双击条面、
右键菜单「设置…」，或命令行 --settings。界面用 Codex 风格：近黑中性底、
全等宽字体、1px 细边、直角、无阴影无渐变；"当前/选中"靠反白表示，
绿/琥珀/红只表示数据健康度。

设置项：位置 / 跟随窗口还是整块屏幕 / 偏移 / 透明度 / 字号 / 显示哪些项 /
ZCode 关掉时是否收起 / 是否置顶 / 是否启动时检查更新。拖动条面可直接改位置。

更新：默认**不自动安装**。勾上「启动时自动检查」后，每次启动 5 秒悄悄问一次
GitHub，有新版本只在设置面板「06 更新」和右键菜单里提示；拉不拉、什么时候拉
由用户点。手动入口是设置面板里的「检查更新 / 立即更新」或上面的命令行两个开关。
"""
from __future__ import annotations

import json
import math
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

HERE = Path(__file__).parent.resolve()
ZUSAGE = HERE / "zusage.py"
CONFIG = HERE / "hoverbar.json"
LOCK = Path(os.environ.get("TEMP", ".")) / "zusage-hoverbar.lock"
DUMP = Path(os.environ.get("TEMP", ".")) / "zusage-hoverbar-dump.json"
IPC_PORT = 47615          # --settings 唤起已在运行实例的控制端口

# Codex 风格配色：近黑中性底、灰阶为主、只在状态上用色。
# 没有高饱和强调色——"当前 / 选中"靠**反白**表达（浅底黑字），
# 跟 Codex CLI 的选中态一致；绿/琥珀/红只表示数据健康度。
BG = "#0d0d0d"
KEY = "#010203"           # -transparentcolor 键色：条面外的直角区域
BORDER = "#2e2e2e"
FG = "#e6e6e6"
DIM = "#8a8a8a"
LABEL = "#6e6e6e"
OK = "#3fb950"
WARM = "#d29922"
HOT = "#f85149"
BADGE = "#f85149"
TRACK = "#242424"         # 进度条 / 加载弧线的暗色轨道

# 设置面板：同一套 Codex 语言——直角、1px 细线、等宽字体、无阴影无渐变。
# EF_SIGNAL 是"反白色块"：选中/当前 = 浅底黑字，不用高饱和强调色。
EF_PAPER = "#0d0d0d"
EF_PANEL = "#161616"
EF_INK = "#e6e6e6"
EF_INK2 = "#8a8a8a"
EF_RULE = "#2e2e2e"
EF_CHIP = "#1f1f1f"
EF_SIGNAL = "#e6e6e6"
EF_ON_SIGNAL = "#0d0d0d"
EF_DANGER = "#f85149"
EF_CMD = "#111111"        # 顶部命令栏

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
    "always_on_top": False,   # 默认普通工具窗口；勾上走 HWND_TOPMOST
    "hide_when_inactive": False,  # 只在 ZCode 窗口真的不存在/最小化时收起
    "show": {"speed": True, "ctx": True, "turn": True,
             "session": True, "today": True, "tools": True, "cache": True},
    "labels": True,
    "poll_ms": 700,
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


def zcode_available(hwnd: int) -> bool:
    """ZCode 主窗口现在还在、且没最小化吗？

    以前 hide_when_inactive 直接拿 foreground_is() 判断"ZCode 在不在前台"。
    但 EnumWindows / GetForegroundWindow 在非交互会话（开机自启、MCP 面板
    拉起的进程）里看不到桌面窗口，前台是 QQ 或别的程序时 zcode_hwnd()
    干脆返回 0 —— 条面就永远判"该藏"，miss 一路涨到十几。用户体感就是
    "条又丢了"。

    改成只看事实：ZCode 窗口存在且有可用矩形就显示。切到资源管理器、
    切到别的应用都不再把它收起来（那本来就是置顶条该有的行为）。
    """
    if not hwnd:
        return False
    return win_state(hwnd) is not None


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


# ---------------------------------------------------------------- 动效
# 曲线与时长照 Emil Kowalski 的 animate 技能定的一套来：入场一律 ease-out、
# 时长压在 300ms 内、动位移和透明度（tkinter 里对应 geometry/alpha），
# 系统关了动画就全部直接落位——gentler, not zero。
_MOTION_OK = None


def motion_ok() -> bool:
    """Windows「显示动画」辅助功能开关（SPI_GETCLIENTAREAANIMATION）。
    关掉时跳过所有入场动画，直接落位。探测一次缓存。"""
    global _MOTION_OK
    if _MOTION_OK is None:
        try:
            import ctypes
            from ctypes import wintypes
            v = wintypes.BOOL()
            ok = ctypes.windll.user32.SystemParametersInfoW(
                0x1042, 0, ctypes.byref(v), 0)      # SPI_GETCLIENTAREAANIMATION
            _MOTION_OK = bool(ok) and bool(v.value)
        except Exception:
            _MOTION_OK = True       # 查不到就当开着，别因为排查失败砍掉动效
    return _MOTION_OK


def ease_out(t: float) -> float:
    """三次方 ease-out：起步快、收尾缓。近似 cubic-bezier(0.23, 1, 0.32, 1)。"""
    t = min(1.0, max(0.0, t))
    return 1.0 - (1.0 - t) ** 3


def _rgb(c: str) -> tuple:
    c = c.lstrip("#")
    return int(c[0:2], 16), int(c[2:4], 16), int(c[4:6], 16)


def mix(c1: str, c2: str, t: float) -> str:
    """两色线性插值。悬停过渡用——直接换色是硬切，插值才连得上。"""
    t = min(1.0, max(0.0, t))
    a, b = _rgb(c1), _rgb(c2)
    return "#%02x%02x%02x" % tuple(round(a[i] + (b[i] - a[i]) * t)
                                  for i in range(3))


def tween(widget, ms: float, frame, done=None, step_ms: int = 16):
    """时间基补间循环。frame(p) 每帧拿到 0→1 的缓动后进度。

    时间基（不是固定帧数）的好处：掉帧时终点照样准。回调里抛 TclError
    （控件已销毁）就静默收场。返回 {"job": after_id} 供取消。
    """
    t0 = time.perf_counter()
    span = max(0.001, ms / 1000.0)
    state = {"job": None}

    def step():
        state["job"] = None
        try:
            t = (time.perf_counter() - t0) / span
            if t >= 1.0:
                frame(1.0)
                if done:
                    done()
                return
            frame(ease_out(t))
            state["job"] = widget.after(step_ms, step)
        except tk.TclError:
            return

    step()
    return state


def _autostart_command() -> str:
    """自启命令串的唯一来源，统一走 autostart.py。

    两边各拼一份的后果是真实的：autostart.py 那份带 --wait-zcode（随 ZCode
    启动），面板里曾经另拼一份不带 flag 的，于是「命令行 on」和「面板勾选」
    写出两种行为，用户在面板里勾一下就把待命模式改回了开机直开。
    autostart.py 不在（用户只拷了 hoverbar.py）时降级成等价的内联命令。
    """
    try:
        import autostart
        return autostart.command()
    except Exception:
        exe = Path(sys.executable)
        pw = exe.with_name("pythonw.exe")
        py = pw if pw.is_file() else exe
        return '"%s" "%s" --wait-zcode' % (py, HERE / "hoverbar.py")


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
        # 加载动画：等待首包时旋转的亮弧线。帧更新走 itemconfig 只改角度，
        # 不重建图元也不进签名门控——动画期间既不闪也不被门控拦住
        self._loading = False
        self._spin_id = None       # 弧线的 canvas item id
        self._spin_track = None    # 暗色整圆轨道的 item id
        self._spin_job = None      # after 定时任务；None = 没在转
        self._spin_angle = 0
        self._spin_phase = 0       # 弧长呼吸的相位

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
        # 条面本身就是拖放目标：图片往条上一丢就换壁纸，不用点开任何菜单。
        # DragAcceptFiles 认顶层 HWND，此时 root 还没映射但句柄已存在。
        self._drop_ok = False
        self.root.after(120, self._install_bar_drop)
        self.root.after(60, self._tick_data)
        self.root.after(80, self._tick_anchor)
        # 启动 5 秒后才问，避开开机那一阵的网络拥塞；没勾自动检查就什么都不做
        if self.cfg.get("auto_update"):
            self.root.after(5000, self._upd_silent)

    # -- 拖放换壁纸 --
    def _drop_toast_howto(self):
        self._drop_toast("图片直接拖到条面上就能换壁纸（jpg / png / webp / bmp）",
                         EF_INK)

    def _install_bar_drop(self):
        """注册条面拖放。只看句柄在不在，不看映射——条面可能因为
        hide_when_inactive 暂时 withdraw，那时 HWND 还在，DragAcceptFiles
        照样能挂；等窗口重新映射时拖放就能用，不需要等下一次安装。"""
        try:
            if not getattr(self, "_drop_ok", False):
                self._drop_ok = _drop_install(self.root, self._on_bar_files)
        except Exception as e:
            log("条面安装拖放失败", e)

    def _on_bar_files(self, files: list[str]):
        """图片拖到条面上 → 直接换壁纸。多文件只取第一张图片。"""
        for f in files:
            p = f.replace("\\", "/")
            if not p.lower().endswith(WP_EXTS) or not os.path.isfile(p):
                continue
            ok, detail = _wp_pick_file(p)
            if not ok:
                self._drop_toast(detail, EF_DANGER)
                return
            p = detail
            if not _wp_write({"wallpaperPath": p, "wallpaperVisible": True}):
                self._drop_toast("写不进插件配置，壁纸没换。", EF_DANGER)
                return
            self._drop_toast("壁纸换成 %s，正在注入…" % _wp_short(p), OK)
            port = (_wp_read() or {}).get("port")

            def work(pp=port):
                good = _wp_refresh(pp)
                self._upd_q.put((self._drop_done, (good, _wp_short(p))))

            threading.Thread(target=work, daemon=True).start()
            return
        self._drop_toast("拖进来的不是图片（jpg / png / webp / bmp）",
                         EF_DANGER)

    def _drop_done(self, good: bool, name: str):
        # HoverBar 没有设置面板那条 200ms 泵，这里直接在主线程回调里改 UI
        if good:
            self._drop_toast("壁纸已生效：%s" % name, OK)
        else:
            self._drop_toast("注入失败：CDP 端口没开？点「立即刷新」再试",
                             EF_DANGER)

    def _drop_toast(self, text: str, color: str):
        """条面上方一行短提示，2.6 秒自己收。不弹系统消息框。"""
        t = getattr(self, "_drop_tip", None)
        if t is None or not t.winfo_exists():
            t = tk.Toplevel(self.root)
            t.overrideredirect(True)
            try:
                t.attributes("-topmost", True)
            except tk.TclError:
                pass
            t.configure(bg=BORDER)
            inner = tk.Frame(t, bg=BG)
            inner.pack(fill="both", expand=True, padx=1, pady=1)
            tk.Frame(inner, bg=EF_SIGNAL, height=2).pack(fill="x")
            lbl = tk.Label(inner, text=text, bg=BG, fg=color,
                           font=_ef_font(EF_UI, 9), padx=12, pady=7)
            lbl.pack()
            self._drop_tip = t
            self._drop_lbl = lbl
            register_own_window(t)
        else:
            try:
                self._drop_lbl.configure(text=text, fg=color)
            except tk.TclError:
                return
        try:
            t.update_idletasks()
            w, h = t.winfo_reqwidth(), t.winfo_reqheight()
            bx, by = self.root.winfo_rootx(), self.root.winfo_rooty()
            bw = self.root.winfo_width()
            sw, sh = t.winfo_screenwidth(), t.winfo_screenheight()
            x = min(max(8, bx + (bw - w) // 2), max(8, sw - w - 8))
            y = by - h - 6 if by - h - 6 >= 8 else by + self.root.winfo_height() + 6
            t.geometry("%dx%d+%d+%d" % (w, h, x, min(y, max(8, sh - h - 8))))
            t.deiconify()
        except tk.TclError:
            return
        job = getattr(self, "_drop_job", None)
        if job:
            try:
                t.after_cancel(job)
            except (tk.TclError, ValueError):
                pass
        self._drop_job = t.after(2600, self._drop_toast_hide)

    def _drop_toast_hide(self):
        t = getattr(self, "_drop_tip", None)
        if t is not None and t.winfo_exists():
            unregister_own_window(t)
            t.destroy()
        self._drop_tip = None
        self._drop_job = None

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
        """把后台结果收进主线程。数据轮询同一个节拍上顺便做，不另起定时器。

        队列里两种东西：版本检查的 (remote, local) 字符串对，和拖放换壁纸
        的 (回调, 参数元组)。后者要真的调回去，不能当成版本比较。"""
        import queue as _q
        while True:
            try:
                item = self._upd_q.get_nowait()
            except _q.Empty:
                return
            if callable(item[0]):
                fn, args = item
                try:
                    fn(*args)
                except Exception as e:
                    log("后台回调失败", e)
                continue
            remote, local = item
            self._upd_new = bool(remote) and remote != local
            if self._upd_new:
                self._upd_ver = "%s → %s" % (local, remote)

    # -- 窗口属性 --
    def _apply_winprops(self):
        self.root.attributes("-alpha", min(1.0, max(0.25, float(self.cfg["opacity"]))))
        self._apply_topmost()

    def _apply_topmost(self):
        """置顶勾一改就立刻生效，不用等重启。

        Tk 的 attributes("-topmost") 只在启动时设一次，运行期改配置不会
        重新调用；而且它跟属主绑定打架时表现不稳定。这里直接调 Win32
        SetWindowPos(HWND_TOPMOST / HWND_NOTOPMOST)，带 SWP_NOACTIVATE |
        SWP_NOMOVE | SWP_NOSIZE，不抢焦点也不动位置。"""
        self.root.attributes("-topmost", bool(self.cfg.get("always_on_top")))
        if not self._bar_hwnd:
            return
        try:
            import ctypes
            from ctypes import wintypes
            u32 = ctypes.WinDLL("user32", use_last_error=True)
            u32.SetWindowPos.argtypes = [wintypes.HWND, wintypes.HWND,
                                         ctypes.c_int, ctypes.c_int,
                                         ctypes.c_int, ctypes.c_int,
                                         ctypes.c_uint]
            after = wintypes.HWND(-1 if self.cfg.get("always_on_top") else -2)
            u32.SetWindowPos(self._bar_hwnd, after, 0, 0, 0, 0,
                             0x0010 | 0x0002 | 0x0001)   # NOACTIVATE|NOMOVE|NOSIZE
        except Exception as e:
            log("设置层级失败", e)

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
        loading = not items
        if loading:
            # 等待首包：一个旋转的加载弧线 + 提示文字，同一个块里不画分隔线
            items = [[("spin",), ("t", "等待数据…", DIM)]]

        pad, gap = 10, 9
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
                elif tok[0] == "spin":
                    ops.append(("spin",))
                    w += self._spin_size() + 2
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
        # 悬停态**不进签名**：悬停只改两个方块的填色，走 _tween_affordance
        # 就地补间。以前悬停会触发整条 delete+重建，既慢又容易闪一帧。
        sig = (total, h, self._px, bool(self.cfg["pinned"]),
               tuple(tuple(tuple(o) for o in ops) for _, ops in blocks))
        if sig == self._draw_key:
            self._last_draw = (total, h)
            self._place()
            return
        self._draw_key = sig

        self.canvas.configure(width=total, height=h)
        self.canvas.delete("all")
        cv = self.canvas

        # Codex 没有圆角：整条是一个 1px 细边的直角方框
        cv.create_rectangle(0.5, 0.5, total - 1.0, h - 1.0,
                            fill=BG, outline=BORDER, width=1)

        cy = h / 2.0
        # 左端图钉方块：内容从它右侧起。钉住时整块反白——
        # "反白=当前状态"，读作"这根条被钉住了"
        self._draw_pin(pad, cy, h)
        x = pad + pin_s + pin_gap
        for i, (w, ops) in enumerate(blocks):
            for op in ops:
                if op[0] == "t":
                    _, txt, col = op
                    cv.create_text(x, cy, text=txt, fill=col,
                                   font=self.font, anchor="w")
                    x += twidth(txt) + GAP
                elif op[0] == "spin":
                    d = self._spin_size()
                    # 暗色整圆轨道 + 亮弧：转圈指示的常见构图，
                    # 轨道给出"这里有个圈"的预期，弧线在上面转才读得顺
                    self._spin_track = cv.create_oval(
                        x + 1, cy - d / 2.0, x + 1 + d, cy + d / 2.0,
                        outline=TRACK, width=2)
                    # 前景弧线：亮灰在暗轨道上转，读作"正在取数"。
                    # start/extent 由 _spin_tick 驱动
                    self._spin_id = cv.create_arc(
                        x + 1, cy - d / 2.0, x + 1 + d, cy + d / 2.0,
                        start=self._spin_angle, extent=270, style="arc",
                        outline=EF_SIGNAL, width=2)
                    x += d + 2 + GAP
                else:
                    _, pct, col = op
                    bh = 5
                    bw = BAR_W_HINT
                    by = cy - bh / 2.0
                    cv.create_polygon(
                        [x + 2.5, by, x + bw - 2.5, by, x + bw, by + bh / 2,
                         x + bw - 2.5, by + bh, x + 2.5, by + bh, x, by + bh / 2],
                        smooth=False, fill=TRACK, outline="")
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
                cv.create_line(x, cy - 7, x, cy + 7, fill=BORDER)
                x += sep_w + gap

        # 右端 ⚙：整条最后一项，直角方框。悬停反白，点击开设置面板。
        # 方块右边距与左边 pad 对齐，视觉上才是"紧贴末端"而不是悬在中间。
        self._draw_gear(total - pad - gear_s, cy, h)

        # 加载动画的启停。数据到位后停表——delete("all") 已经把弧线清掉了，
        # 不停的话定时器会对着死 id 转，白耗电还可能在重画瞬间闪一帧错位
        if loading:
            self._loading = True
            if not self._spin_job:
                self._spin_job = self.root.after(110, self._spin_tick)
        else:
            self._loading = False
            self._spin_id = None
            self._spin_track = None
            if self._spin_job:
                try:
                    self.root.after_cancel(self._spin_job)
                except Exception:
                    pass
                self._spin_job = None

        self._last_draw = (total, h)
        self._place()

    def _spin_size(self):
        """加载弧线直径：跟字号走，比文字略矮，读作附属指示而不是主角。"""
        return max(10, int(self._px * 0.7))

    def _spin_tick(self):
        """转一格。只 itemconfig 改角度和弧长——不重建图元、不动几何、
        不经过签名门控，所以动画期间条面既不闪也不会被"没变就不重画"拦住。

        丝滑的关键是"追逐"式构图（Material 转圈那套）：弧长随相位呼吸
        （100°↔260°），叠加匀速旋转，视觉上弧头在追弧尾，比一根 270° 的
        硬弧匀速转顺滑得多。30fps（33ms）——tkinter 事件循环下再快反而抖。"""
        self._spin_job = None
        if not self._loading or not self._spin_id:
            return
        try:
            self._spin_angle = (self._spin_angle + 6.0) % 360
            self._spin_phase += 0.09
            ext = 100 + 80 * (1 + math.sin(self._spin_phase))   # 100°~260°
            self.canvas.itemconfigure(self._spin_id, start=self._spin_angle,
                                      extent=ext)
            self._spin_job = self.root.after(33, self._spin_tick)
        except tk.TclError:
            pass              # 画布没了（退出中），静默收场

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
        # Codex 的按钮就是直角方框 + 1px 边线：悬停靠"填色反白"表示可点
        hover = isinstance(self._hover, str) and self._hover == "gear"
        cv = self.canvas
        self.gear_bg = cv.create_rectangle(
            x, y1, x2, y2,
            fill=EF_SIGNAL if hover else EF_CHIP,
            outline=EF_RULE if hover else "")
        gcol = EF_ON_SIGNAL if hover else EF_INK2
        c = s / 2.0
        r = max(3.0, s * 0.30)
        cv.create_oval(
            x + c - r, cy - r, x + c + r, cy + r,
            outline=gcol, width=1.2, fill="", tags="gear_glyph")
        # 齿：四根短线，绕中心 45° 交叉，够小仍能读成齿轮
        for dx, dy in ((0, -1), (0, 1), (-1, 0), (1, 0)):
            cv.create_line(
                x + c + dx * r * 0.72, cy + dy * r * 0.72,
                x + c + dx * (r + 1.8), cy + dy * (r + 1.8),
                fill=gcol, width=1.2, tags="gear_glyph")
        self._gear_box = (x, y1, x2, y2)

    def _draw_pin(self, x, cy, h):
        """左端图钉方块：直角方框，反白=已钉住。图钉用三笔凑——
        横杠帽 + 针身 + 针尖，14px 内也读得出是"钉"。"""
        s = self._gear_size()
        y1 = cy - s / 2.0
        y2 = cy + s / 2.0
        x2 = x + s
        # 直角方框，与右端齿轮同一套：钉住时反白（浅底黑字）表示"当前状态"
        pinned = bool(self.cfg["pinned"])
        hover = isinstance(self._hover, str) and self._hover == "pin"
        hot = pinned or hover
        cv = self.canvas
        self.pin_bg = cv.create_rectangle(
            x, y1, x2, y2,
            fill=EF_SIGNAL if hot else EF_CHIP,
            outline=EF_RULE if hot else "")
        gcol = EF_ON_SIGNAL if hot else EF_INK2
        c = x + s / 2.0
        cap_y = y1 + s * 0.22
        cap_h = s * 0.16
        cv.create_rectangle(c - s * 0.24, cap_y, c + s * 0.24,
                            cap_y + cap_h, fill=gcol, outline="",
                            tags="pin_glyph")
        cv.create_line(c, cap_y + cap_h, c, y2 - s * 0.18,
                       fill=gcol, width=max(1.6, s * 0.12),
                       tags="pin_glyph")
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
                          and not zcode_available(self._hwnd)):
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
        if not wr or (cfg["hide_when_inactive"] and not zcode_available(self._hwnd)):
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
        # 条面比 ZCode 窗口还宽时，右贴靠会算出负 x（比如窗口 800 宽、
        # 条面 1000 宽 → x = -216），整条跑到屏幕左边外面去。
        # 夹进所在显示器工作区：宁可偏离锚点，也不能让它不可见。
        wx, wy, ww, wh = monitor_work(self._hwnd)
        x = min(max(wx, x), max(wx, wx + ww - bw))
        y = min(max(wy, y), max(wy, wy + wh - bh))
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
        # 顺序至关重要：未映射的窗口必须先 geometry 再 deiconify。
        # 反过来的话 deiconify 会先把它按当前位置（新建窗口就是屏幕左上角
        # 0,0）显示出来，下一句 geometry 才挪到目标——启动时那一下左上角
        # 闪屏就是这么来的。已映射的窗口 geometry 本来就能直接生效，
        # 不需要 deiconify，但多发一次也无害（对映射窗口是 no-op）。
        try:
            if not self.root.winfo_ismapped():
                self.root.geometry("%dx%d+%d+%d" % (bw, bh, x, y))
                self.root.deiconify()
            else:
                self.root.geometry("%dx%d+%d+%d" % (bw, bh, x, y))
        except tk.TclError:
            return
        self._bind_zorder()
        self._follow_panel()

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
        """确保窗口处于映射状态。已经映射就什么都不做——问 Tk 比记标志可靠，
        重复调用天然幂等。与 _hide 成对，两者只管可见性不管位置。"""
        try:
            if not self.root.winfo_ismapped():
                self.root.deiconify()
        except tk.TclError:
            pass

    def _hide(self):
        """收起。已经收起就不用再动。

        条面收了面板必须一起收：面板是贴在条面上的一块，
        条面没了还留着它就是一块没有归属的浮窗。
        """
        try:
            if self.root.winfo_ismapped():
                self.root.withdraw()
        except tk.TclError:
            pass
        dlg = getattr(self, "_dlg", None)
        if dlg is not None:
            try:
                dlg.close()
            except Exception:
                pass

    def _follow_panel(self):
        """条面位置变了 → 面板跟着重贴（两者是同一块界面）。"""
        dlg = getattr(self, "_dlg", None)
        if dlg is None:
            return
        try:
            dlg._place_off_bar()
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
        """只维护工具窗口属性和置顶层级，**不再把条面设成 ZCode 的属主窗口**。

        以前用 SetWindowLongPtrW(GWLP_HWNDPARENT) 把条绑在 ZCode 上，
        Windows 会强制"属主窗口恒浮在属主之上"——结果是切到资源管理器、
        或 ZCode 自己切换子窗口时层级被系统来回重排，用户看到的就是条面
        跟文件管理器/ZCode 打架。改成普通工具窗口，层级只由 -topmost /
        SetWindowPos 管，谁在前台都不影响它。"""
        if not self._bar_hwnd:
            u32 = _u32()
            self._bar_hwnd = u32.GetAncestor(self.root.winfo_id(), 2)   # GA_ROOT
            if not self._bar_hwnd:
                return
            _OWN_HWNDS["bar"] = self._bar_hwnd
            ex = u32.GetWindowLongW(self._bar_hwnd, -20)               # GWL_EXSTYLE
            u32.SetWindowLongW(self._bar_hwnd, -20,
                               ex | 0x00000080 | 0x08000000)           # TOOLWINDOW|NOACTIVATE
            self._apply_topmost()

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
        """悬停切换：只把两个方块的填色补间过去，不重建整条。

        目标色沿用原语义——齿轮悬停反白、图钉悬停或已钉住时反白。
        90ms ease-out：比硬切跟手，又不会拖慢指针反馈。
        """
        if getattr(self, "_hover", None) == what:
            return
        self._hover = what
        self._tween_affordance()

    def _tween_affordance(self):
        pin_hot = bool(self.cfg["pinned"]) or self._hover == "pin"
        gear_hot = self._hover == "gear"
        pairs = []
        box = getattr(self, "pin_bg", None)
        if box is not None:
            pairs.append((box, EF_SIGNAL if pin_hot else EF_CHIP))
        box = getattr(self, "gear_bg", None)
        if box is not None:
            pairs.append((box, EF_SIGNAL if gear_hot else EF_CHIP))
        if not pairs:
            return
        # 图形（齿轮环线 / 图钉针身）跟着方块一起换色：
        # 反白底上留深灰图形会糊，必须同步到墨色
        for tag, hot in (("pin_glyph", pin_hot), ("gear_glyph", gear_hot)):
            target = EF_ON_SIGNAL if hot else EF_INK2
            try:
                ids = self.canvas.find_withtag(tag)
            except tk.TclError:
                ids = ()
            for item in ids:
                pairs.append((item, target))
        start = {}
        for item, target in pairs:
            try:
                field = "outline" if self.canvas.type(item) == "oval" else "fill"
                start[item] = (field, self.canvas.itemcget(item, field), target)
            except tk.TclError:
                return
        job = getattr(self, "_aff_job", None)
        if job and job.get("job"):
            try:
                self.root.after_cancel(job["job"])
            except Exception:
                pass

        def frame(p):
            for item, (field, c0, c1) in start.items():
                try:
                    self.canvas.itemconfigure(item, **{field: mix(c0, c1, p)})
                except tk.TclError:
                    return

        self._aff_job = tween(self.root, 90, frame)

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
            # "(推断)" = 该模型的窗口目录里查不到，按已观测用量推的标准档，
            # 会随会话增长自动上档——数字诚实但不是官方值，标注出来
            mark = "  (推断)" if d.get("context_inferred") else ""
            add("上下文", "%s / %s  %s%s" % (fmt(ctx), fmt(cw), pct(ctx, cw),
                                             mark))

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
        # 顶部 2px 亮线：跟条面同一处强调色，读起来是"这条的展开"
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
        # 建好先不显示：_tip_show 会摆好位置再 deiconify。
        # 否则 Toplevel 一创建就在左上角露一帧，跟条面启动闪屏同一个毛病。
        t.withdraw()
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
        bh = self.root.winfo_height()
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
        # 行缓存跟着窗口一起作废：留着旧的行数/标签会让下一次 _tip_refresh
        # 误判成"行数变了"而白重建一个新窗口
        self._tip_rows = None
        self._tip_row_labels = None


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
        m.add_command(label="把图片拖到条上可直接换壁纸",
                      command=self._drop_toast_howto)
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
            "loading": self._loading,
            "hwnd": self._hwnd,
            "font_px": self._px,
            "data_none": self.data is None,
            "session": ((self.data or {}).get("session") or {}).get("sid", "")[-8:],
            "anchor": self.cfg.get("anchor"),
            "scope": self.cfg.get("scope"),
            # 诊断「条面被藏 / 层级冲突」：把判断链上的每个中间量都摊开
            "hide_when_inactive": bool(self.cfg.get("hide_when_inactive")),
            "always_on_top": bool(self.cfg.get("always_on_top")),
            "topmost": bool(self.root.attributes("-topmost")),
            "bar_hwnd": self._bar_hwnd,
            "owner": getattr(self, "_owner", 0),
            "own_hwnds": {"bar": _OWN_HWNDS.get("bar", 0),
                          "extra": sorted(_OWN_HWNDS.get("extra", ()))},
            "fg_now": _u32().GetForegroundWindow(),
            "foreground_is": foreground_is(self._hwnd),
            "win_state_now": win_state(self._hwnd),
            "zcode_available": zcode_available(self._hwnd),
            "drop_ok": getattr(self, "_drop_ok", False),
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
        _drop_uninstall(self.root)
        self._drop_toast_hide()
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
# 风格：Codex（OpenAI Codex 桌面/CLI 的视觉语言）
#   近黑中性底（#0d0d0d）+ 灰阶文字；全等宽字体（含中文回落到等宽中文字体）；
#   直角、1px 细边（#2e2e2e）、无圆角无阴影无渐变；
#   "当前 / 选中"用**反白**（浅底黑字）表达，不用高饱和强调色；
#   绿 #3fb950 / 琥珀 #d29922 / 红 #f85149 只表示数据健康度。
# Codex 界面全等宽：连界面标签也是 mono，中文回落到系统等宽中文字体。
EF_UI = ("JetBrains Mono", "Cascadia Mono", "Consolas",
         "Sarasa Mono SC", "Noto Sans Mono CJK SC",
         "Microsoft YaHei UI", "Microsoft YaHei")
EF_MONO = ("Cascadia Mono", "JetBrains Mono", "Consolas")


def _ef_font(fams, size, bold=False):
    have = set(tkfont.families())
    fam = next((f for f in fams if f in have), "Microsoft YaHei UI")
    return (fam, size, "bold") if bold else (fam, size)


class _EfButton(tk.Canvas):
    """Codex 按钮：直角方框 + 1px 边线。主按钮反白（浅底黑字），
    次按钮只有边线，悬停填 EF_CHIP。没有切角、没有圆角。"""

    def __init__(self, master, text, command, primary=False, width=None):
        self.text = text
        self.command = command
        self.primary = primary
        self._hover = False
        f = _ef_font(EF_UI, 9, bold=primary)
        probe = tkfont.Font(root=master, font=f)
        h, cut = 26, 0
        w = width or (probe.measure(text) + 30)
        super().__init__(master, width=w, height=h, bg=EF_PANEL,
                         highlightthickness=0, bd=0, cursor="hand2")
        self._font = f
        self._cw, self._ch, self._cut = w, h, cut
        self.bind("<Enter>", lambda e: self._set_hover(True))
        self.bind("<Leave>", lambda e: self._set_hover(False))
        self.bind("<Button-1>", lambda e: self.command())
        self._paint()

    def _set_hover(self, v):
        if self._hover == v:
            return
        self._hover = v
        w, h = self._cw, self._ch
        c0, c1 = self._colors(not v)
        t1, t2 = self._text_colors(not v)
        job = getattr(self, "_job", None)
        if job and job.get("job"):
            try:
                self.after_cancel(job["job"])
            except Exception:
                pass

        def frame(p):
            fill = mix(c0, c1, p)
            self.itemconfigure(self._box, fill=fill,
                               outline=fill if self.primary else EF_RULE)
            self.itemconfigure(self._label, fill=mix(t1, t2, p))

        self._job = tween(self, 90, frame)

    def _colors(self, hover):
        if self.primary:
            return (EF_SIGNAL, "#ffffff") if not hover else ("#ffffff", "#ffffff")
        return (EF_CHIP, EF_PANEL) if hover else (EF_PANEL, EF_CHIP)

    def _text_colors(self, hover):
        if self.primary:
            return EF_ON_SIGNAL, EF_ON_SIGNAL
        return ((EF_INK, EF_INK2) if not hover else (EF_INK2, EF_INK))

    def _paint(self):
        self.delete("all")
        w, h = self._cw, self._ch
        if self.primary:
            fill = "#ffffff" if self._hover else EF_SIGNAL
            self._box = self.create_rectangle(
                0.5, 0.5, w - 0.5, h - 0.5, fill=fill, outline=fill, width=1)
            fg = EF_ON_SIGNAL
        else:
            fill = EF_CHIP if self._hover else EF_PANEL
            self._box = self.create_rectangle(
                0.5, 0.5, w - 0.5, h - 0.5, fill=fill, outline=EF_RULE,
                width=1)
            fg = EF_INK if self._hover else EF_INK2
        self._label = self.create_text(w / 2.0, h / 2.0, text=self.text,
                                       fill=fg, font=self._font)


class _EfCheck(tk.Canvas):
    """勾选项。未选中是 1px 空方框，选中是实心反白方块 + 墨色 ✓；
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
    """1px 细轨 + 实心方块把手（直角，没有圆点滑块）。"""

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
    """单选 chip 组：直角小方框，选中 = 反白填充。"""

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

            def target(c, v):
                """当前应显示的颜色 (底, 文字)。"""
                if self.var.get() == v:
                    return EF_SIGNAL, EF_ON_SIGNAL
                if c._hover:
                    return EF_CHIP, EF_INK
                return bg, EF_INK2

            def paint(c=cv, v=val, t=txt):
                c.delete("all")
                w, h = int(c["width"]), 22
                fill, fg = target(c, v)
                c._box = c.create_rectangle(
                    0.5, 0.5, w - 0.5, h - 0.5, fill=fill,
                    outline=fill if fill == EF_SIGNAL else EF_RULE)
                c._label = c.create_text(w / 2.0, h / 2.0, text=t, fill=fg,
                                         font=_ef_font(EF_UI, 8))

            def hover(c=cv, v=val, on=True):
                """悬停只补间颜色，不重建；选中态切换才走 paint。"""
                if c._hover == on:
                    return
                c._hover = on
                if self.var.get() == v:
                    return                    # 选中项不随悬停变色
                f0 = c.itemcget(c._box, "fill")
                g0 = c.itemcget(c._label, "fill")
                f1, g1 = target(c, v)
                job = getattr(c, "_job", None)
                if job and job.get("job"):
                    try:
                        c.after_cancel(job["job"])
                    except Exception:
                        pass

                def frame(p):
                    c.itemconfigure(c._box, fill=mix(f0, f1, p))
                    c.itemconfigure(c._label, fill=mix(g0, g1, p))

                c._job = tween(c, 90, frame)

            cv.bind("<Enter>", lambda e, h=hover: h(on=True))
            cv.bind("<Leave>", lambda e, h=hover: h(on=False))
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


def _wp_pick_file(path: str) -> tuple[bool, str]:
    """校验一张壁纸。返回 (通过, 归一化路径 或 错误说明)。

    只做判定**不写盘**：条面直接拖放和面板选择器共用这份规则，但各自
    有各自的写盘/刷新流程，合在一起会写两次。不弹系统消息框（消息框
    也借 Explorer）。"""
    p = (path or "").strip().strip('"').replace("\\", "/")
    if not p.lower().endswith(WP_EXTS) or not os.path.isfile(p):
        return False, "不是图片或文件不存在：%s" % Path(p).name
    return True, p


# ---------------------------------------------------------------- 文件拖拽（WM_DROPFILES）
# Tk 8.6 不带 tkdnd，也不想为一张图装第三方包。Windows 自己的
# DragAcceptFiles + WM_DROPFILES 就够了：把顶层窗口注册成拖放目标，
# 系统拖进来时给一条 HDROP，用 DragQueryFile/DragFinish 读出路径。
# 全程不借 Explorer 的文件对话框，也就绕开了那个 0xc0000374 堆损坏。
_WM_DROPFILES = 0x0233
_GWL_WNDPROC = -4


def _drop_install(win, on_files) -> bool:
    """给一个 Tk 顶层窗口装拖放接收。成功返回 True，失败静默返回 False
    （拖拽是加法，装不上还得能照常用目录列表）。"""
    try:
        import ctypes
        from ctypes import wintypes
        u32 = ctypes.WinDLL("user32", use_last_error=True)
        sh = ctypes.WinDLL("shell32", use_last_error=True)
        WNDPROC = ctypes.WINFUNCTYPE(ctypes.c_ssize_t, wintypes.HWND,
                                     ctypes.c_uint, wintypes.WPARAM,
                                     wintypes.LPARAM)
        u32.GetAncestor.argtypes = [wintypes.HWND, ctypes.c_uint]
        u32.GetAncestor.restype = wintypes.HWND
        u32.SetWindowLongPtrW.argtypes = [wintypes.HWND, ctypes.c_int, WNDPROC]
        u32.SetWindowLongPtrW.restype = WNDPROC
        u32.CallWindowProcW.argtypes = [WNDPROC, wintypes.HWND, ctypes.c_uint,
                                        wintypes.WPARAM, wintypes.LPARAM]
        u32.CallWindowProcW.restype = ctypes.c_ssize_t
        sh.DragAcceptFiles.argtypes = [wintypes.HWND, wintypes.BOOL]
        sh.DragQueryFileW.argtypes = [wintypes.HANDLE, ctypes.c_uint,
                                      wintypes.LPWSTR, ctypes.c_uint]
        sh.DragQueryFileW.restype = ctypes.c_uint
        sh.DragFinish.argtypes = [wintypes.HANDLE]

        hwnd = u32.GetAncestor(win.winfo_id(), 2)   # GA_ROOT
        if not hwnd:
            return False

        def _read_drop(hdrop):
            n = sh.DragQueryFileW(hdrop, 0xFFFFFFFF, None, 0)
            out = []
            for i in range(min(n, 32)):
                size = sh.DragQueryFileW(hdrop, i, None, 0)
                buf = ctypes.create_unicode_buffer(size + 1)
                sh.DragQueryFileW(hdrop, i, buf, size + 1)
                if buf.value:
                    out.append(buf.value)
            sh.DragFinish(hdrop)
            return out

        @WNDPROC
        def proc(h, msg, wp, lp):
            if msg == _WM_DROPFILES:
                # 回调必须保持对 WNDPROC 对象的引用，别让 GC 收走
                try:
                    files = _read_drop(wp)
                    if files:
                        win.after(0, lambda f=files: on_files(f))
                except Exception as e:
                    log("读取拖放文件失败", e)
                return 0
            return u32.CallWindowProcW(old, h, msg, wp, lp)

        old = u32.SetWindowLongPtrW(hwnd, _GWL_WNDPROC, proc)
        if not old:
            return False
        sh.DragAcceptFiles(hwnd, True)
        win._drop_state = (hwnd, old, proc, u32, sh)   # 防 GC + 恢复用
        return True
    except Exception as e:
        log("安装拖放失败", e)
        return False


def _drop_uninstall(win):
    st = getattr(win, "_drop_state", None)
    if not st:
        return
    hwnd, old, proc, u32, sh = st
    try:
        sh.DragAcceptFiles(hwnd, False)
        u32.SetWindowLongPtrW(hwnd, _GWL_WNDPROC, old)
    except Exception:
        pass
    win._drop_state = None


WP_EXTS = (".jpg", ".jpeg", ".png", ".webp", ".bmp")


def _wp_image_files(folder: str) -> list[str]:
    """列一个目录里的图片。只给自绘选择器用，不碰系统文件对话框。"""
    try:
        return sorted((n for n in os.listdir(folder)
                       if n.lower().endswith(WP_EXTS)
                       and os.path.isfile(os.path.join(folder, n))),
                      key=str.lower)
    except OSError:
        return []


class WallpaperPicker(tk.Toplevel):
    """自绘壁纸选择器。

    Windows 的原生文件对话框由 Explorer.EXE 承载，装了第三方 Shell 扩展
    （WPS/夸克/PrivaZer…）时它会 0xc0000374 堆损坏顺带把调用方带走。
    这里只读目录、画个列表，还接 Win32 WM_DROPFILES 直接拖文件进来；
    全程不借 Explorer 的文件对话框，也不弹 messagebox。
    """

    def __init__(self, dlg):
        super().__init__(dlg.top)
        self.dlg = dlg
        self.picked: str | None = None
        self._hwnd = 0
        self.title("选择壁纸")
        self.configure(bg=EF_PAPER)
        self.overrideredirect(True)
        self.attributes("-topmost", True)
        self.resizable(False, False)
        self.protocol("WM_DELETE_WINDOW", self._cancel)
        self.bind("<Escape>", lambda e: self._cancel())

        hdr = tk.Frame(self, bg=EF_CMD)
        hdr.pack(fill="x")
        tk.Frame(hdr, bg=EF_RULE, height=1).pack(side="bottom", fill="x")
        tk.Label(hdr, text="▌ 选择壁纸", bg=EF_CMD, fg=EF_INK,
                 font=_ef_font(EF_MONO, 9, bold=True)).pack(
                     side="left", padx=(14, 6), pady=9)
        _EfButton(hdr, "×", self._cancel, width=26).pack(side="right", padx=6)

        # 路径行：可编辑，直接粘路径也行（完全不需要弹对话框）
        pr = tk.Frame(self, bg=EF_PAPER)
        pr.pack(fill="x", padx=14, pady=(10, 4))
        tk.Label(pr, text="目录", bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_UI, 8)).pack(side="left")
        self.v_dir = tk.StringVar()
        e = tk.Entry(pr, textvariable=self.v_dir, bg=EF_CHIP, fg=EF_INK,
                     insertbackground=EF_INK, relief="flat", bd=0,
                     font=_ef_font(EF_UI, 9), highlightthickness=1,
                     highlightbackground=EF_RULE, highlightcolor=EF_RULE)
        e.pack(side="left", fill="x", expand=True, padx=(8, 6), ipady=4)
        e.bind("<Return>", lambda ev: self._load())
        _EfButton(pr, "读取", self._load).pack(side="left")

        # 拖拽落区：把图片从资源管理器直接拖到这条上就能用。
        # 窗口级接收（DragAcceptFiles 认的是顶层 HWND），所以落区本身只是提示。
        self._dropzone = tk.Frame(self, bg=EF_PANEL, height=46,
                                  highlightthickness=1,
                                  highlightbackground=EF_RULE,
                                  highlightcolor=EF_RULE)
        self._dropzone.pack(fill="x", padx=14, pady=(8, 2))
        self._dropzone.pack_propagate(False)
        self._drop_lbl = tk.Label(
            self._dropzone, bg=EF_PANEL, fg=EF_INK2,
            text="把图片拖到这里  ·  或粘贴完整路径后回车",
            font=_ef_font(EF_UI, 9))
        self._drop_lbl.pack(expand=True)
        for w in (self._dropzone, self._drop_lbl):
            w.bind("<Button-1>", lambda e: self._drop_lbl.focus_set())

        # 单文件路径：粘一行绝对路径按回车，等于拖进来
        fr = tk.Frame(self, bg=EF_PAPER)
        fr.pack(fill="x", padx=14, pady=(2, 4))
        tk.Label(fr, text="文件", bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_UI, 8)).pack(side="left")
        self.v_file = tk.StringVar()
        fe = tk.Entry(fr, textvariable=self.v_file, bg=EF_CHIP, fg=EF_INK,
                      insertbackground=EF_INK, relief="flat", bd=0,
                      font=_ef_font(EF_UI, 9), highlightthickness=1,
                      highlightbackground=EF_RULE, highlightcolor=EF_RULE)
        fe.pack(side="left", fill="x", expand=True, padx=(8, 6), ipady=4)
        fe.bind("<Return>", lambda ev: self._use_path(self.v_file.get()))
        _EfButton(fr, "用这个", lambda: self._use_path(
            self.v_file.get())).pack(side="left")

        body = tk.Frame(self, bg=EF_PAPER)
        body.pack(fill="both", expand=True, padx=14, pady=(4, 2))
        self._list = tk.Listbox(body, bg=EF_CHIP, fg=EF_INK,
                                selectbackground=EF_SIGNAL,
                                selectforeground=EF_ON_SIGNAL,
                                relief="flat", bd=0, highlightthickness=1,
                                highlightbackground=EF_RULE,
                                font=_ef_font(EF_UI, 9),
                                activestyle="none", height=12, width=42)
        sb = tk.Scrollbar(body, bg=EF_PAPER, troughcolor=EF_PAPER,
                          relief="flat", bd=0, width=8)
        self._list.configure(yscrollcommand=sb.set)
        sb.configure(command=self._list.yview)
        sb.pack(side="right", fill="y")
        self._list.pack(side="left", fill="both", expand=True)
        self._list.bind("<Double-Button-1>", lambda e: self._confirm())
        self._list.bind("<Return>", lambda e: self._confirm())

        self._msg = tk.Label(self, text="", bg=EF_PAPER, fg=EF_INK2,
                             font=_ef_font(EF_UI, 8), anchor="w")
        self._msg.pack(fill="x", padx=14)

        foot = tk.Frame(self, bg=EF_PAPER)
        foot.pack(fill="x", padx=14, pady=(4, 12))
        _EfButton(foot, "取消", self._cancel).pack(side="right")
        _EfButton(foot, "用这张", self._confirm, primary=True).pack(
            side="right", padx=(0, 8))

        # 起始目录：当前壁纸所在目录，没有就退到桌面/家目录
        cur = (dlg._wp or {}).get("wallpaperPath", "") or ""
        start = os.path.dirname(cur) if os.path.isfile(cur) else ""
        if not start or not os.path.isdir(start):
            for d in (Path.home() / "Desktop", Path.home(),
                      Path(os.environ.get("USERPROFILE", "")) / "Desktop"):
                if d.is_dir():
                    start = str(d)
                    break
        self.v_dir.set(start)
        self._load()

        # 摆在设置面板中间偏上，别跑到屏幕外
        self.update_idletasks()
        w, h = self.winfo_reqwidth(), self.winfo_reqheight()
        px, py = dlg.top.winfo_rootx(), dlg.top.winfo_rooty()
        pw, ph = dlg.top.winfo_width(), dlg.top.winfo_height()
        sw, sh = self.winfo_screenwidth(), self.winfo_screenheight()
        x = min(max(8, px + (pw - w) // 2), max(8, sw - w - 8))
        y = min(max(8, py + max(0, (ph - h) // 3)), max(8, sh - h - 8))
        self.geometry("%dx%d+%d+%d" % (w, h, x, y))
        self._hwnd = register_own_window(self)   # 视线在浮层上别把条面藏了
        self._drop_ok = _drop_install(self, self._on_files)
        if self._drop_ok:
            self._drop_lbl.configure(
                text="把图片拖到这里  ·  或粘贴完整路径后回车",
                fg=EF_INK)
        self._list.focus_set()

    def _use_path(self, raw: str):
        """拖进来的文件 / 粘进来的路径，统一走这里。"""
        if not (raw or "").strip():
            self._msg.configure(text="先给个文件路径", fg=EF_DANGER)
            return
        ok, detail = _wp_pick_file(raw)
        if not ok:
            self._msg.configure(text=detail, fg=EF_DANGER)
            return
        # 校验通过但写盘交给 dlg._wp_apply 统一做（还要更新标签、起后台注入），
        # 这里只把归一化路径传下去。
        self.picked = detail
        self._finish()

    def _on_files(self, files: list[str]):
        """拖放回调（已在主线程）。多文件只取第一张图片。"""
        for f in files:
            p = f.replace("\\", "/")
            if p.lower().endswith(WP_EXTS) and os.path.isfile(p):
                self.v_file.set(p)
                self._drop_lbl.configure(text="已接到：%s" % _wp_short(p),
                                         fg=OK)
                self._use_path(p)
                return
        self._drop_lbl.configure(text="拖进来的不是图片", fg=EF_DANGER)
        self._msg.configure(text="只收 jpg / png / webp / bmp", fg=EF_DANGER)

    def _load(self):
        folder = self.v_dir.get().strip()
        self._list.delete(0, "end")
        if not os.path.isdir(folder):
            self._msg.configure(text="目录不存在", fg=EF_DANGER)
            return
        files = _wp_image_files(folder)
        for n in files:
            self._list.insert("end", n)
        self._msg.configure(
            text="%d 张图片 · 双击或回车选中" % len(files), fg=EF_INK2)
        if files:
            self._list.selection_set(0)
            self._list.activate(0)

    def _confirm(self):
        folder = self.v_dir.get().strip()
        sel = self._list.curselection()
        if not sel:
            self._msg.configure(text="先选一张图片", fg=EF_DANGER)
            return
        name = self._list.get(sel[0])
        full = os.path.join(folder, name).replace("\\", "/")
        if not os.path.isfile(full):
            self._msg.configure(text="文件不见了，点「读取」刷新", fg=EF_DANGER)
            self._load()
            return
        self.picked = full
        self._finish()

    def _cancel(self):
        self.picked = None
        self._finish()

    def _finish(self):
        if getattr(self, "_finished", False):
            return
        self._finished = True
        picked = self.picked
        dlg = self.dlg
        dlg._wp_picker = None       # 让下次点按钮能重新打开
        unregister_own_window(self._hwnd)
        _drop_uninstall(self)
        try:
            self.grab_release()
        except tk.TclError:
            pass
        self.destroy()
        if picked:
            dlg._wp_apply(picked)


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
        # 登记到条面上：条面移动时靠它把面板带着走、收起时一起关。
        # open_settings 也会设，但直接构造（测试、--settings 期间）也得有。
        bar._dlg = self
        # 先藏起来：Toplevel 一创建就按默认位置（屏幕左上）显示，等下面
        # 一堆控件建完再摆位，中间会看到它在左上角闪一下。建完、
        # 摆好位置之后才 deiconify（见 __init__ 末尾）。
        top.withdraw()
        top.title("悬浮条设置")
        top.configure(bg=EF_PAPER)
        # 不要系统白标题栏：它跟这套暗色完全不是一路。改成无边框 + 自绘标题行，
        # 拖动靠下面 bind 的 <B1-Motion>，关闭靠右侧的小方框。
        top.overrideredirect(True)
        top.attributes("-topmost", True)
        # 不要 resizable(False, False)：它把 min=max 锁成初始化那一刻的尺寸，
        # 之后切页改内容高度窗口不会重算（短页下方留白、长页被裁）。
        # overrideredirect 窗口没有边框可拖，本来也不需要这句。
        top.protocol("WM_DELETE_WINDOW", self.close)
        # 面板自己的透明度：下次打开沿用上次的值
        try:
            top.attributes("-alpha", min(1.0, max(
                0.4, float(bar.cfg.get("panel_opacity", 1.0)))))
        except tk.TclError:
            pass

        # 顶部标题行：Codex 是「▌ 标题  · 面包屑」，没有三色条那类装饰。
        bar_hdr = tk.Frame(top, bg=EF_CMD)
        bar_hdr.pack(fill="x")
        tk.Frame(bar_hdr, bg=EF_RULE, height=1).pack(side="bottom", fill="x")

        cmd = tk.Frame(bar_hdr, bg=EF_CMD)
        cmd.pack(fill="x")
        # "▌" 光标块：Codex TUI 的招牌符号，等宽字体下正好一格宽
        mark = tk.Label(cmd, text="▌", bg=EF_CMD, fg=EF_SIGNAL,
                        font=_ef_font(EF_MONO, 9))
        mark.pack(side="left", padx=(14, 6), pady=9)
        tk.Label(cmd, text="ZCODE TOKEN", bg=EF_CMD, fg=EF_INK,
                 font=_ef_font(EF_MONO, 9, bold=True)).pack(side="left")
        crumb = tk.Frame(cmd, bg=EF_CMD)
        crumb.pack(side="left", padx=(10, 0))
        tk.Label(crumb, text="·", bg=EF_CMD, fg=EF_INK2,
                 font=_ef_font(EF_MONO, 9)).pack(side="left")
        tk.Label(crumb, text="settings", bg=EF_CMD, fg=EF_INK2,
                 font=_ef_font(EF_MONO, 9)).pack(side="left", padx=(8, 0))

        # 关闭：直角小方框，悬停反白。系统标题栏去掉后这是唯一关闭入口。
        cl = tk.Canvas(cmd, width=20, height=20, bg=EF_CMD,
                       highlightthickness=0, bd=0, cursor="hand2")
        cl.pack(side="right", padx=(0, 12), pady=7)
        cl_box = cl.create_rectangle(1.5, 1.5, 18.5, 18.5,
                                     fill=EF_CHIP, outline=EF_RULE)
        cl.create_line(6.5, 6.5, 13.5, 13.5, fill=EF_INK2, width=1.3)
        cl.create_line(13.5, 6.5, 6.5, 13.5, fill=EF_INK2, width=1.3)

        def cl_hover(on_):
            f0 = cl.itemcget(cl_box, "fill")
            f1 = EF_SIGNAL if on_ else EF_CHIP
            g0 = cl.itemcget(cl.find_all()[1], "fill")
            g1 = EF_ON_SIGNAL if on_ else EF_INK2
            job = getattr(cl, "_job", None)
            if job and job.get("job"):
                try:
                    cl.after_cancel(job["job"])
                except Exception:
                    pass

            def frame(p):
                try:
                    cl.itemconfigure(cl_box, fill=mix(f0, f1, p))
                    for i in cl.find_all()[1:]:
                        cl.itemconfigure(i, fill=mix(g0, g1, p))
                except tk.TclError:
                    pass

            cl._job = tween(cl, 90, frame)

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

        # 内容区：七个分区改成标签页。以前全堆在一个 body 里，
        # 125% 缩放下 900 逻辑像素高、几乎顶满屏幕，把对话区挡掉了。
        # 现在一页只装一个分区，面板高度取最高那页，切页带滑入动画。
        outer = tk.Frame(top, bg=EF_PAPER)
        outer.pack(fill="both", expand=True, padx=(20, 0), pady=(12, 0))
        self._scroll_cv = tk.Canvas(outer, bg=EF_PAPER, highlightthickness=0,
                                    bd=0)
        # holder 必须挂在 canvas 自己下面：create_window 只把「canvas 的子控件」
        # 真正合成进画布。挂到 outer 的话它是一个独立的同级 HWND，会被 canvas
        # 盖住——控件、几何全对，整块正文却是一片空白。
        self._holder = tk.Frame(self._scroll_cv, bg=EF_PAPER)
        self._scroll_cv.pack(side="left", fill="both", expand=True)
        # height=1 是必须的：Canvas 不设 -height 时默认 7c（125% 缩放下
        # 约 331px），会把它所在的外层请求高度顶到 331——于是无论内容多矮
        # 窗口都缩不下来，短页下方留一截空白。pack(fill="y") 会把它拉满，
        # 请求高度只需给一个不影响布局的最小值。
        self._scrollbar = tk.Canvas(outer, width=3, height=1, bg=EF_PAPER,
                                    highlightthickness=0, bd=0)
        self._scrollbar.pack(side="right", fill="y", padx=(6, 8))
        self._body_id = self._scroll_cv.create_window(
            (0, 0), window=self._holder, anchor="nw")
        self._holder.bind("<Configure>",
                          lambda e: self._sync_scroll(self._body_id))
        self._scroll_cv.bind("<Configure>",
                             lambda e: self._sync_scroll(self._body_id))
        for w in (self._scroll_cv, self._holder, self._scrollbar):
            w.bind("<MouseWheel>", self._on_wheel)

        # ---- 标签栏 ----
        tabs = tk.Frame(top, bg=EF_PAPER)
        tabs.pack(fill="x", padx=(20, 14), pady=(10, 0))
        tk.Frame(top, bg=EF_RULE, height=1).pack(fill="x", padx=(20, 14),
                                                 pady=(8, 0))
        self._tabs_bar = tabs
        self._tab_cvs = {}
        self._cur_page = None
        self._pages = {}
        self._slide_job = None
        # 每页一个构建方法（原来全塞在 __init__ 里，424 行）：
        # 位置 → 外观 → 显示 → 壁纸 → 行为 → 更新 → 状态
        for key, cn, build in (
                ("pos", "位置", self._page_pos),
                ("look", "外观", self._page_look),
                ("items", "显示", self._page_items),
                ("panel", "壁纸", self._page_panel),
                ("behav", "行为", self._page_behav),
                ("upd", "更新", self._page_upd),
                ("stat", "状态", self._page_stat)):
            self._tab_chip(key, cn)
            page = tk.Frame(self._holder, bg=EF_PAPER)
            self._pages[key] = page
            build(page)

        # ---- 底部按钮 ----
        tk.Frame(top, bg=EF_RULE, height=1).pack(fill="x", pady=(10, 0))
        foot = tk.Frame(top, bg=EF_PANEL)
        foot.pack(fill="x")
        inner = tk.Frame(foot, bg=EF_PANEL)
        inner.pack(fill="x", padx=20, pady=10)
        _EfButton(inner, "恢复默认", self.reset).pack(side="left")
        _EfButton(inner, "应用", self.apply, primary=True).pack(side="right")
        _EfButton(inner, "关闭", self.close).pack(side="right", padx=(0, 8))

        # 透明度/字号拖动即生效：先在条上看效果，再决定要不要"应用"
        for v in (self.v_op, self.v_fs):
            v.trace_add("write", lambda *_: self._preview())

        # Tk 的 <MouseWheel> 不向上冒泡：鼠标停在标签、滑块、勾选框上时，
        # 事件只发给那个控件。所以每页的整棵子树都要绑一遍。
        try:
            self._holder.bind("<MouseWheel>", self._on_wheel)
        except tk.TclError:
            pass
        for f in self._pages.values():
            self._bind_wheel(f)

        top.update_idletasks()
        # 各页尺寸互不相同：宽度取最宽那页，高度取最高那页（再封顶 62% 屏高）。
        # 这样切页时面板不会跳宽，只会按当前页缩短——比原来固定 900px 紧凑得多。
        pw = max(f.winfo_reqwidth() for f in self._pages.values())
        self._page_w = pw
        self._page_h = {k: f.winfo_reqheight() for k, f in self._pages.items()}
        self._max_h = max(self._page_h.values())
        cap = max(240, int(top.winfo_screenheight() * 0.62))
        self._cap_h = cap
        # v73：所有页共用**同一个**视口高度（最高页，再封顶），切页不再
        # 忽大忽小。短页内容顶部对齐，下面留出的空处是稳定的，比每页
        # 换一次窗口尺寸舒服得多；面板窗口也因此只算一次尺寸。
        self._fixed_h = min(self._max_h, cap)
        self._scroll_cv.configure(width=pw, height=self._fixed_h)
        self._holder.configure(width=pw, height=self._fixed_h)
        self._body_id = self._scroll_cv.create_window(
            (0, 0), window=self._holder, anchor="nw", width=pw,
            height=self._fixed_h)
        top.update_idletasks()
        self._show_page("pos", animate=False)
        self._place_off_bar()
        # 摆好位置、内容齐了才显示——顺序反了就是"每次打开面板都在左上角闪一下"
        self._animate_open()
        self._st_refresh()          # 状态区先填一次，别让用户看到四行 —
        # 面板开着时用户就是在操作悬浮条，别让 hide_when_inactive 把条藏了
        self._own_hwnd = register_own_window(top)
        # 后台 git 调用的结果回主线程：起泵，跟着面板同生共死
        self._upd_pump()

    def _animate_open(self):
        """面板入场：160ms ease-out，从下方 12px 落定 + 淡入。

        第一帧就画在偏移位置且透明（先 geometry/alpha 再 deiconify），
        所以不会闪。系统关了动画就直接落位。终点透明度是用户设的
        panel_opacity——动画只负责"到那里"，不覆盖设置。
        """
        top = self.top
        # 必须用 _place_off_bar 记下的坐标：Toplevel 还没映射时 winfo_x/y 读回 0，
        # 用它当动画终点会把面板从 (0,0) 一路滑到左上角
        x, y = getattr(self, "_anchor", (top.winfo_x(), top.winfo_y()))
        target = min(1.0, max(0.4, float(self.cfg.get("panel_opacity", 1.0))))
        # 从贴条那一侧滑出：贴在条面下方就从上方 10px 落下，贴在上方就往上收
        dy = 10 if getattr(self, "_drop", "down") == "down" else -10
        self._open_dy = dy
        try:
            if not motion_ok():
                top.attributes("-alpha", target)
                top.deiconify()
                return
            # 入场起点：位移要小（10px），不是从天而降
            top.geometry("+%d+%d" % (x, y - dy))
            top.attributes("-alpha", 0.0)
            top.deiconify()
        except tk.TclError:
            return
        t0 = time.perf_counter()

        def step():
            t = (time.perf_counter() - t0) / 0.16
            e = ease_out(t)
            try:
                if t >= 1.0 or getattr(self, "_open_job", None) is None:
                    top.geometry("+%d+%d" % (x, y))
                    top.attributes("-alpha", target)
                    self._open_job = None
                    return
                off = getattr(self, "_open_dy", 10)
                top.geometry("+%d+%d" % (
                    x, y - round(off * (1 - e))))
                top.attributes("-alpha", round(target * e, 3))
                self._open_job = top.after(16, step)
            except tk.TclError:
                pass

        self._open_job = top.after(0, step)

    def _page_pos(self, p):
        """01 位置。"""
        self._section(p, 1, "位置", "position")
        self.v_anchor = tk.StringVar(value=self.cfg["anchor"])

        _EfChips(self._row(p, "贴靠", "相对 ZCode 窗口或整块屏幕"), self.v_anchor,

                 [(a, ANCHOR_TXT[a]) for a in ANCHORS if a != "free"]

                 ).pack(anchor="e")



        self.v_scope = tk.StringVar(value=self.cfg["scope"])

        _EfChips(self._row(p, "范围", "跟随窗口 = 只贴 ZCode；整块屏幕 = 贴显示器"),

                 self.v_scope,

                 [("window", "跟随窗口"), ("screen", "整块屏幕")]

                 ).pack(anchor="e")



        self.v_ox = tk.IntVar(value=int(self.cfg["offset_x"]))

        self.v_oy = tk.IntVar(value=int(self.cfg["offset_y"]))

        r = self._row(p, "边距", "左右 / 上下，单位 px")

        for v, tip in ((self.v_ox, "左右"), (self.v_oy, "上下")):

            tk.Spinbox(r, from_=0, to=600, textvariable=v, width=5,

                       bg=EF_CHIP, fg=EF_INK, insertbackground=EF_SIGNAL,

                       buttonbackground=EF_CHIP, relief="flat", bd=0,

                       highlightthickness=1,

                       highlightbackground=EF_RULE,

                       highlightcolor=EF_SIGNAL,

                       font=_ef_font(EF_MONO, 9)

                       ).pack(side="left", padx=(0, 6))

            tk.Label(r, text=tip, bg=EF_PAPER, fg=EF_INK2,

                     font=_ef_font(EF_UI, 8)).pack(side="left", padx=(0, 10))



        # 钉住：与条面左端图钉是同一个状态。

        self.v_pin = tk.BooleanVar(value=bool(self.cfg["pinned"]))

        self._row(p, "钉住", "锁住当前位置；显示停在被钉住的那一轮对话上")

        _EfCheck(p, "钉住条面（等于条面左端的图钉）", self.v_pin,

                 big=True).pack(anchor="w", pady=(0, 2))


    def _page_look(self, p):
        """02 外观。"""
        self._section(p, 2, "外观", "appearance")
        self.v_op = tk.DoubleVar(value=float(self.cfg["opacity"]))

        _EfScale(self._row(p, "透明度", "拖动即时预览，点「应用」才落盘"),

                 self.v_op, 0.30, 1.0, fmt="%.2f").pack(anchor="e")



        self.v_fs = tk.IntVar(value=int(self.cfg["font_px"]))

        _EfScale(self._row(p, "字号", "px；上下文微进度条会一起缩放"),

                 self.v_fs, 10, 22, fmt="%d", suffix="px").pack(anchor="e")



        self.v_lab = tk.BooleanVar(value=bool(self.cfg["labels"]))

        _EfCheck(p, "显示文字标签（速度 / 上下文 / 本轮 …）", self.v_lab,

                 big=True).pack(anchor="w", pady=(8, 2))


    def _page_items(self, p):
        """03 显示项。"""
        self._section(p, 3, "显示项", "elements")
        self.v_show = {}

        grid = tk.Frame(p, bg=EF_PAPER)

        grid.pack(fill="x", pady=(0, 2))

        names = [("speed", "速度"), ("ctx", "上下文"), ("turn", "本轮"),

                 ("session", "会话"), ("today", "今日"), ("tools", "工具"),

                 ("cache", "缓存率")]

        for i, (k, txt) in enumerate(names):

            v = tk.BooleanVar(value=bool(self.cfg["show"].get(k, True)))

            self.v_show[k] = v

            _EfCheck(grid, txt, v).grid(row=i // 3, column=i % 3,

                                        sticky="w", padx=(0, 14), pady=3)


    def _page_panel(self, p):
        """04 面板与壁纸。"""
        self._section(p, 4, "面板与壁纸", "panel")
        self.v_pop = tk.DoubleVar(value=float(self.cfg.get("panel_opacity", 1.0)))

        _EfScale(self._row(p, "面板透明度", "这个设置面板自己；1.00 = 纯不透明"),

                 self.v_pop, 0.55, 1.0, fmt="%.2f").pack(anchor="e")

        self.v_pop.trace_add("write", lambda *_: self._preview_panel())



        # 壁纸：读 zcode-beautify 插件的数据目录，跟它共用同一份配置，

        # 不另存一份以免两边打架。插件没装就整段换成一行提示。

        self._wp = _wp_read()

        if self._wp is not None:

            self.v_wp_vis = tk.BooleanVar(

                value=bool(self._wp.get("wallpaperVisible", True)))

            _EfCheck(p, "显示壁纸（关掉后 ZCode 回到纯色背景）", self.v_wp_vis,

                     big=True).pack(anchor="w", pady=(8, 4))

            self.v_wp_dim = tk.DoubleVar(value=float(self._wp.get("dim", 25)))

            _EfScale(self._row(p, "壁纸压暗", "越高壁纸越沉，字越清楚"),

                     self.v_wp_dim, 0, 100, fmt="%.0f").pack(anchor="e")

            self.v_wp_blur = tk.DoubleVar(value=float(self._wp.get("blur", 0)))

            _EfScale(self._row(p, "壁纸模糊", "px；界面会更糊但更安静"),

                     self.v_wp_blur, 0, 40, fmt="%.0f").pack(anchor="e")
            # v73：压暗/模糊/显示开关拖动即时生效，不必点「应用」。
            # debounce 600ms 是因为 _wp_refresh 要起 subprocess 走 CDP，
            # 鼠标每动一格都跑一次会把面板拖垮；松手后一拍写盘 + 注入。
            self._wp_job = None
            for v in (self.v_wp_vis, self.v_wp_dim, self.v_wp_blur):
                v.trace_add("write", lambda *_: self._wp_changed())

            r_pick = self._row(p, "换壁纸", "从目录里挑，选完立刻生效")

            _EfButton(r_pick, "从目录里挑…", self._open_wallpaper_picker).pack(side="left")

            cur = tk.Label(r_pick, text=_wp_short(self._wp.get("wallpaperPath", "")),

                           bg=EF_PAPER, fg=EF_INK2,

                           font=_ef_font(EF_UI, 8))

            cur.pack(side="left", padx=(10, 0))

            self._wp_label = cur
            # 刷新壁纸：不换图，只让插件把已存的壁纸/配色重新注入一次。
            # 典型场景是"图设过了但界面没变"（CDP 断过、ZCode 重启过）。
            r_re = self._row(p, "刷新壁纸",
                               "图没换但界面没生效时，手动重新注入一次")
            _EfButton(r_re, "立即刷新", self._wp_reinject).pack(side="left")
            self._wp_msg = tk.Label(p, text="", bg=EF_PAPER, fg=EF_INK2,
                                    font=_ef_font(EF_UI, 8))
            self._wp_msg.pack(anchor="w", pady=(2, 4))

            self._note(p, "壁纸由 zcode-beautify 插件接管；这里用自绘目录选择，不弹系统文件对话框。")

        else:

            self._note(p, "没找到 zcode-beautify 插件的数据目录，壁纸设置不可用。")


    def _page_behav(self, p):
        """05 行为。"""
        self._section(p, 5, "行为", "behaviour")
        self.v_top = tk.BooleanVar(value=bool(self.cfg["always_on_top"]))

        _EfCheck(p, "总是置顶（盖住其它程序）", self.v_top,

                 big=True).pack(anchor="w", pady=2)

        self._note(p, "勾上立刻生效，不用重启。条面不再绑成 ZCode 的属主窗口，切换窗口时层级不会被系统重排。")



        self.v_hide = tk.BooleanVar(value=bool(self.cfg["hide_when_inactive"]))

        _EfCheck(p, "ZCode 最小化/关闭时收起", self.v_hide,

                 big=True).pack(anchor="w", pady=(6, 2))

        self._note(p, "只在 ZCode 窗口真的没了才收；切到资源管理器或别的程序不藏，不会再「条丢了」。")


    def _page_upd(self, p):
        """06 更新。"""
        self._section(p, 6, "更新", "update")
        # 当前版本号直接读 git：没有 tag 体系，短 hash 就是最好的版本标识

        self._upd_local = local_commit() or "（非 git 安装）"

        r = self._row(p, "当前版本", "git 短 hash；没有就是解压使用时不在版本控制里")

        tk.Label(r, text=self._upd_local, bg=EF_PAPER, fg=EF_INK,

                 font=_ef_font(EF_MONO, 9, bold=True)).pack(side="left",

                                                            padx=(0, 10))

        self._upd_btn = _EfButton(r, "检查更新", self._upd_check)

        self._upd_btn.pack(side="left")

        self._upd_msg = tk.Label(p, text="", bg=EF_PAPER, fg=EF_INK2,

                                 font=_ef_font(EF_UI, 8))

        self._upd_msg.pack(anchor="w", pady=(2, 6))



        self.v_auto = tk.BooleanVar(

            value=bool(self.cfg.get("auto_update", False)))

        _EfCheck(p, "启动时自动检查（只提示，不自动安装）", self.v_auto,

                 big=True).pack(anchor="w", pady=2)

        self._note(p, "勾上后每次启动悄悄问一次 GitHub：有新版本就在这里和右键菜单提示，"

                "点不点由你定。默认不勾。")


    def _page_stat(self, p):
        """07 状态。"""
        self._section(p, 7, "状态", "status")
        st = tk.Frame(p, bg=EF_PAPER)

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



        r_st = tk.Frame(p, bg=EF_PAPER)

        r_st.pack(fill="x", pady=(6, 2))

        _EfButton(r_st, "刷新", self._st_refresh).pack(side="left")

        _EfButton(r_st, "重启悬浮条", self._st_restart).pack(side="left",

                                                              padx=(8, 0))

        self._st_msg = tk.Label(p, text="", bg=EF_PAPER, fg=EF_INK2,

                                font=_ef_font(EF_UI, 8))

        self._st_msg.pack(anchor="w", pady=(2, 4))



        # 自启开关即时生效：写的是注册表 Run 项，属于系统级状态，

        # 不该等「应用」——而且取消自启这种操作，用户期望点了就走。

        self.v_autostart = tk.BooleanVar(value=self._st_autostart_on())

        _EfCheck(p, "开机自动启动悬浮条（写当前用户 Run 项）",

                 self.v_autostart, big=True).pack(anchor="w", pady=(6, 2))

        self.v_autostart.trace_add("write", lambda *_: self._st_set_autostart())

        self._note(p, "只用当前用户权限，不需要管理员。路径写在注册表里，"

                "换了 Python 或搬了目录会失效——上面那行会标红提醒。")


    # -- 面板内的通用构件（原来都是 __init__ 里的闭包） --
    def _tab_chip(self, key, cn):
        """标签栏上的一个切角小方块。存档信息供 _tab_paint 重绘。"""
        f = _ef_font(EF_MONO, 9)
        w = tkfont.Font(root=self.top, font=f).measure(cn) + 22
        c = tk.Canvas(self._tabs_bar, width=w, height=24, bg=EF_PAPER,
                      highlightthickness=0, bd=0, cursor="hand2")
        c.pack(side="left", padx=(0, 5))
        c._hover = False
        self._tab_cvs[key] = (c, cn, f, w)
        c.bind("<Enter>", lambda e: self._tab_hover(key, True))
        c.bind("<Leave>", lambda e: self._tab_hover(key, False))
        c.bind("<Button-1>", lambda e: self._show_page(key))
        return c

    def _tab_paint(self, key):
        c, cn, f, w = self._tab_cvs[key]
        c.delete("all")
        on = (key == self._cur_page)
        h, cut = 26, 6
        pts = [0.5, 0.5, w - cut, 0.5, w - 0.5, cut + 0.5,
               w - 0.5, h - 0.5, 0.5, h - 0.5]
        if on:
            c.create_polygon(pts, smooth=False, fill=EF_SIGNAL,
                             outline=EF_SIGNAL)
            fg = EF_ON_SIGNAL
        else:
            c.create_polygon(pts, smooth=False,
                             fill=EF_CHIP if c._hover else EF_PAPER,
                             outline=EF_RULE)
            fg = EF_INK if c._hover else EF_INK2
        c.create_text(w / 2.0, h / 2.0, text=cn, fill=fg, font=f)

    def _tab_hover(self, key, v):
        c = self._tab_cvs[key][0]
        if c._hover == v:
            return
        c._hover = v
        if key == self._cur_page:
            return                       # 当前页保持反白，不随悬停变
        try:
            f0 = c.itemcget(c._box, "fill")
            g0 = c.itemcget(c._label, "fill")
            f1 = EF_CHIP if v else EF_PAPER
            g1 = EF_INK if v else EF_INK2
        except tk.TclError:
            self._tab_paint(key)
            return
        job = getattr(c, "_job", None)
        if job and job.get("job"):
            try:
                c.after_cancel(job["job"])
            except Exception:
                pass

        def frame(p):
            try:
                c.itemconfigure(c._box, fill=mix(f0, f1, p))
                c.itemconfigure(c._label, fill=mix(g0, g1, p))
            except tk.TclError:
                pass

        c._job = tween(c, 90, frame)

    @staticmethod
    def _section(parent, no, cn, en):
        """分区标题（Codex 式）：dim 编号 + 中文 + 紧凑小写英文，无字距拉伸。"""
        head = tk.Frame(parent, bg=EF_PAPER)
        head.pack(fill="x")
        tk.Label(head, text="%02d" % no, bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_MONO, 9)).pack(side="left", padx=(0, 9))
        tk.Label(head, text=cn, bg=EF_PAPER, fg=EF_INK,
                 font=_ef_font(EF_UI, 10, bold=True)).pack(side="left")
        tk.Label(head, text=en.lower(), bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_MONO, 8)).pack(side="right")
        tk.Frame(parent, bg=EF_RULE, height=1).pack(fill="x", pady=(5, 9))

    @staticmethod
    def _row(parent, cn, note=None):
        """一行：左边说明（中文 + 小字注释），右边控件区。返回右侧容器。"""
        r = tk.Frame(parent, bg=EF_PAPER)
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

    @staticmethod
    def _note(parent, text):
        tk.Label(parent, text=text, bg=EF_PAPER, fg=EF_INK2,
                 font=_ef_font(EF_UI, 8)).pack(anchor="w", pady=(2, 4))

    def _set_page_height(self, h):
        """把内容视口和 holder 一起改成 h 高。

        复用一个 canvas window item——重复 create_window 会堆同名 item，
        bbox/滚动区域都会算错；只改宽高并把坐标归零。
        """
        if getattr(self, "_body_id", None) is None:
            return
        self._scroll_cv.configure(height=h)
        self._holder.configure(width=self._page_w, height=h)
        self._scroll_cv.itemconfigure(self._body_id, width=self._page_w,
                                      height=h)
        self._scroll_cv.coords(self._body_id, 0, 0)

    def _animate_page_height(self, target, animate=True):
        """切页**不改视口高度**：所有页共用 _fixed_h（v73）。

        以前按页改高度，忽大忽小不说，上翻定位还会被旧高度算错；
        补间版本更糟——每帧触发一次窗口 resize + 整窗重绘，一次切页
        260ms 才跑完、一帧没画出来。现在面积恒定，只由页面滑入给反馈。
        """
        self._sync_scroll()

    def _show_page(self, key, animate=True):
        """切到某一页：换页 + 按该页高度收放画布 + 滑入动画。

        动画只动 place 的 y 偏移（14px 落到底），不碰窗口几何，所以不会闪；
        快速连点用 _slide_job 打断上一轮。

        holder 的高度必须显式设：place 进来的子控件不参与几何传播，
        父容器会被 create_window 压成 1px 高，页面内容整块被裁掉
        （表现为"面板里一片空白，只有标签和页脚"）。
        """
        if key == self._cur_page:
            return
        self._cur_page = key
        for k, f in self._pages.items():
            if k != key:
                f.place_forget()
        page = self._pages[key]
        h = min(self._page_h.get(key, self._max_h), self._cap_h)
        self._animate_page_height(h, animate)
        for k in self._tab_cvs:
            self._tab_paint(k)
        if not animate or not motion_ok():
            page.place(x=0, y=0, relwidth=1)
            self._sync_scroll()
            return
        if self._slide_job:
            try:
                self.top.after_cancel(self._slide_job)
            except Exception:
                pass
            self._slide_job = None
        # 150ms ease-out（emil 规则：下拉类 150-250ms，入场 ease-out，
        # 每帧只动位移不碰布局）。时间基而不是固定帧序列，掉帧时终点
        # 位置照样准。
        t0 = time.perf_counter()

        def step():
            t = (time.perf_counter() - t0) / 0.15
            if t >= 1.0:
                self._slide_job = None
                try:
                    page.place(x=0, y=0, relwidth=1)
                    self._sync_scroll()
                except tk.TclError:
                    pass
                return
            try:
                page.place(x=0, y=max(0, round(14 * (1 - ease_out(t)))),
                           relwidth=1)
                self._slide_job = self.top.after(16, step)
            except tk.TclError:
                self._slide_job = None

        step()

    def _unregister_own_window(self):
        unregister_own_window(getattr(self, "_own_hwnd", 0))
        self._own_hwnd = 0

    def _sync_scroll(self, body_id=None):
        """内容比视口高时，画出可滚动的细线指示条。

        分页之后高度直接取自当前页的 reqheight：页面是 place 进去的，
        canvas 的 bbox 里不包含它，靠 bbox 会算出空区域。
        """
        cv, sb = self._scroll_cv, self._scrollbar
        if cv is None:
            return
        try:
            page = self._pages.get(self._cur_page)
            th = page.winfo_reqheight() if page is not None else 0
            h = cv.winfo_height()
            w = cv.winfo_width()
            cv.configure(scrollregion=(0, 0, max(1, w), max(1, th)))
            sb.delete("all")
            if th <= h + 1:
                return
            frac = h / float(th)
            span = float(max(1, th - h))
            top_y = max(0.0, min(1.0, -cv.canvasy(0) / span))
            sh = sb.winfo_height() or h   # 滚动条 fill="y" 撑开，高度取实际值
            y1 = top_y * sh
            y2 = y1 + max(18.0, sh * frac)
            sb.create_rectangle(1, y1, 2, y2, fill=EF_SIGNAL, outline=EF_SIGNAL)
        except tk.TclError:
            pass

    def _bind_wheel(self, w):
        """把滚轮事件绑到整棵子树。用 bind 而不是 bind_all，避免误伤兄弟控件；
        同一控件重复绑是幂等的（后一次覆盖前一次，回调相同）。"""
        try:
            w.bind("<MouseWheel>", self._on_wheel)
        except tk.TclError:
            return
        for c in w.winfo_children():
            self._bind_wheel(c)

    def _on_wheel(self, e):
        cv = self._scroll_cv
        if cv is None:
            return
        page = self._pages.get(self._cur_page)
        if page is None or page.winfo_reqheight() <= cv.winfo_height() + 1:
            return
        cv.yview_scroll(int(-1 * (e.delta / 120)) * 3, "units")
        self.top.after_idle(self._sync_scroll)

    def _place_off_bar(self):
        """把面板贴在条面上：正下方或正上方，零间隙、左边缘对齐。

        以前是按鼠标位置随便飘，跟条面看不出关系。现在它是"从条面长出来"
        的一块：紧贴条面下沿（下面放不下就翻到上沿并**底边**贴住），
        右边缘对齐齿轮、夹在屏内。上下都不够时压缩内容视口到可用高度，
        标签栏和页脚永远可点。条面不可见时（--settings 冷启动）退回指针附近。
        """
        top = self.top
        # 先落定布局再量：切页刚改过 canvas 高度，reqheight 还是旧值，
        # 拿旧高度算贴边会留下一大截空隙
        top.update_idletasks()
        w, h = top.winfo_reqwidth(), top.winfo_reqheight()
        sw, sh = top.winfo_screenwidth(), top.winfo_screenheight()
        bar = self.bar
        bx, by, bw, bh, shown = 0, 0, 0, 0, False
        try:
            shown = bool(bar._visible) and bar.root.winfo_width() > 4
            if shown:
                bx, by = bar.root.winfo_x(), bar.root.winfo_y()
                bw, bh = bar.root.winfo_width(), bar.root.winfo_height()
        except tk.TclError:
            shown = False
        if not shown:
            px, py = top.winfo_pointerx(), top.winfo_pointery()
            x = min(max(8, px - w // 2), max(8, sw - w - 8))
            y = min(max(8, py - 40), max(8, sh - h - 48))
            self._drop = "down"
            self._anchor = (x, y)
            top.geometry("+%d+%d" % (x, y))
            return x, y
        # 触发点是条面最右端的齿轮，所以面板右边缘对齐条面右边缘
        # （"从触发处展开"）；右边放不下才退回左对齐并夹在屏内。
        x = bx + bw - w
        if x < 8:
            x = max(8, bx)
        x = min(x, max(8, sw - w - 8))
        below = sh - (by + bh) - 8               # 条面下方的可用高度
        above = by - 8                           # 条面上方的可用高度
        if h <= below:
            y, self._drop = by + bh, "down"      # 贴条面下沿
        elif h <= above:
            # 上翻必须是**底边**贴条面顶沿。以前用 by-h（顶对齐）在
            # 面板比可用空间高时，底部标签栏会伸到条面下面被盖住——
            # 用户点不到标签就是"无法切换"。
            y, self._drop = by - h, "up"
        else:
            # 上下都不够：选较宽的一侧，并把内容视口压缩到那一侧的高度。
            # 面板整体高度 = 视口 + chrome，压缩后一定放得下，绝不遮挡。
            side = "down" if below >= above else "up"
            chrome = max(0, h - self._scroll_cv.winfo_reqheight())
            avail = max(180, (below if side == "down" else above) - chrome)
            self._set_page_height(min(getattr(self, "_fixed_h", avail), avail))
            top.update_idletasks()
            h = top.winfo_reqheight()
            y = by + bh if side == "down" else by - h
            self._drop = side
        y = min(max(8, y), max(8, sh - h - 8))
        self._anchor = (x, y)
        top.geometry("+%d+%d" % (x, y))
        return x, y

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
        """面板自己的透明度也做实时预览，边拖边看。
        拖滑块 = 用户在接管透明度，入场动画立刻让位。"""
        job = getattr(self, "_open_job", None)
        if job:
            try:
                self.top.after_cancel(job)
            except Exception:
                pass
            self._open_job = None
        try:
            a = min(1.0, max(0.4, float(self.v_pop.get())))
            self.top.attributes("-alpha", a)
        except (tk.TclError, ValueError):
            pass

    def _open_wallpaper_picker(self):
        """打开自绘选择器。不碰 Windows 原生文件对话框——它的宿主
        Explorer.EXE 在第三方 Shell 扩展下会堆损坏（0xc0000374），
        把整个调用链一起带走。"""
        if getattr(self, "_wp_picker", None) is not None:
            try:
                if self._wp_picker.winfo_exists():
                    self._wp_picker.lift()
                    return
            except tk.TclError:
                pass
        try:
            self._wp_picker = WallpaperPicker(self)
        except Exception as e:
            log("打开壁纸选择器失败", e)
            self._wp_picker = None
            msg = getattr(self, "_wp_msg", None)
            if msg is not None:
                self._say(msg, "选择器打不开：%s" % e, EF_DANGER)

    def _wp_apply(self, p: str):
        """把选好的图写进 beautify 配置并后台重注入。

        全程只改我们自己面板上的状态行，不弹 messagebox——消息框也走
        系统窗口，没必要再借一次 Explorer。"""
        self._wp_picker = None
        if not _wp_write({"wallpaperPath": p, "wallpaperVisible": True}):
            msg = getattr(self, "_wp_msg", None)
            if msg is not None:
                self._say(msg, "写不进插件配置，壁纸没换。", EF_DANGER)
            return
        try:
            self._wp_label.configure(text=_wp_short(p))
        except tk.TclError:
            pass
        self._wp = _wp_read() or self._wp
        msg = getattr(self, "_wp_msg", None)
        if msg is not None:
            self._say(msg, "已保存，正在注入…")
        port = self._wp.get("port")

        def work():
            ok = _wp_refresh(port)
            self._upd_q.put((self._wp_apply_done, (ok,)))

        threading.Thread(target=work, daemon=True).start()

    def _wp_apply_done(self, ok: bool):
        msg = getattr(self, "_wp_msg", None)
        if msg is None:
            return
        if ok:
            self._say(msg, "已重新注入，壁纸生效。", OK)
        else:
            self._say(msg, "注入失败：CDP 端口没开？点「立即刷新」再试。",
                      EF_DANGER)

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
        不能让它显示"已开"而注册表里其实没有。

        命令串统一由 autostart.py 提供——那份带 --wait-zcode（随 ZCode 启动）。
        以前这里自己拼了一份不带 flag 的，于是「命令行 on」和「面板里勾选」
        写出两种不同行为，用户在面板里勾一下就把待命模式悄悄改回了开机直开。
        """
        want = bool(self.v_autostart.get())
        if want == self._st_autostart_on():
            return
        try:
            import winreg
            key = r"Software\Microsoft\Windows\CurrentVersion\Run"
            if want:
                cmd = _autostart_command()
                with winreg.CreateKey(winreg.HKEY_CURRENT_USER, key) as k:
                    winreg.SetValueEx(k, "ZCodeTokenBar", 0, winreg.REG_SZ, cmd)
                self._st_say("已开启开机自启（随 ZCode 启动）。")
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

    def _say(self, label, text, fg=None):
        """给某个状态 Label 改一行字。面板可能已经关了，所以吞掉 TclError。"""
        try:
            label.configure(text=text, fg=fg or EF_INK2)
        except tk.TclError:
            pass

    def _st_say(self, text, fg=None):
        self._say(self._st_msg, text, fg)

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
        #    还要区分"窗口在但条面被故意藏了"——那是收起逻辑生效，
        #    不是丢了；不写清楚用户会以为是 bug。
        try:
            hwnd = zcode_hwnd()
        except Exception:
            hwnd = 0
        hidden = not self.bar._visible
        if hwnd and hidden and self.bar.cfg.get("hide_when_inactive"):
            self._st_set("zcode",
                         "已找到，但条面因窗口不可见而收起（见「05 行为」）", WARM)
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
                # 命令里带 --wait-zcode 说明是待命模式：ZCode 出现才开条
                self._st_set("auto", "已开（随 ZCode 启动）" if "--wait-zcode" in cmd
                             else "已开", OK)
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
        self._say(self._upd_msg, text, fg)

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

    def _wp_changed(self):
        """壁纸控件变了：600ms 内合并成一次写盘 + 后台重注入。"""
        if self._wp is None or getattr(self, "_wp_msg", None) is None:
            return
        job = getattr(self, "_wp_job", None)
        if job:
            try:
                self.top.after_cancel(job)
            except Exception:
                pass

        def flush():
            self._wp_job = None
            patch = {"wallpaperVisible": bool(self.v_wp_vis.get()),
                     "dim": round(float(self.v_wp_dim.get())),
                     "blur": round(float(self.v_wp_blur.get()))}
            if not _wp_write(patch):
                self._say(self._wp_msg, "配置写失败，改动未生效。", EF_DANGER)
                return
            self._wp.update(patch)
            self._wp = _wp_read() or self._wp
            self._say(self._wp_msg, "已保存，正在注入…")
            port = self._wp.get("port")

            def work():
                ok = _wp_refresh(port)
                self._upd_q.put((self._wp_reinject_done, (ok,)))

            threading.Thread(target=work, daemon=True).start()

        self._wp_job = self.top.after(600, flush)

    def _wp_reinject(self):
        """不换图，只重注入：后台线程跑插件 CLI，结果回主线程显示。

        _wp_refresh 会起 subprocess 走 CDP，最长 30s——放主线程会把面板
        冻住，所以丢后台；结果仍走对话框那条 200ms 泵回主线程。
        """
        msg = getattr(self, "_wp_msg", None)
        if msg is None or self._wp is None:
            return
        self._say(msg, "正在注入…")
        port = self._wp.get("port")

        def work():
            ok = _wp_refresh(port)
            self._upd_q.put((self._wp_reinject_done, (ok,)))

        threading.Thread(target=work, daemon=True).start()

    def _wp_reinject_done(self, ok):
        msg = getattr(self, "_wp_msg", None)
        if msg is None:
            return
        if ok:
            self._say(msg, "已重新注入，ZCode 应该已经换上了。", OK)
        else:
            self._say(msg, "注入失败：ZCode 的调试端口（CDP）没开。"
                           "重启一次 ZCode（从快捷方式启动）再试。", EF_DANGER)

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
    return _git(["--version"])[0] == 0


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


ZCODE_WAIT_POLL = 1.0      # 待命模式的看窗口间隔（秒）


def wait_zcode() -> int:
    """--wait-zcode：待命直到 ZCode 主窗口出现，然后原地变成悬浮条。

    自启用这个模式，实现的是"跟 ZCode 一起启动"：开机只有几 MB 的
    pythonw 在每秒看一眼（实测单次枚举 0.26ms，几乎不占 CPU），ZCode
    没开就什么都不画；ZCode 一来，同一个进程直接开条——不起第二个
    进程，也没有先后竞争。之后 ZCode 关了再开，条面自己会藏/现，
    不依赖这个待命逻辑。

    已经有一条在跑就安静退出：否则开机会把设置面板叫到前台。这个检查要
    放在轮询**之前**——已有一条时根本不该进入待命循环，否则 ZCode 没开的
    机器上它会永远空转。
    """
    if running_pid():
        return 0
    while not zcode_hwnd():
        time.sleep(ZCODE_WAIT_POLL)
    if running_pid():                  # 待命期间用户手动开了一条
        return 0
    HoverBar(load_settings()).run()
    return 0


# ---------------------------------------------------------------- CLI
def alive(pid: int) -> bool:
    """PID 活着**且是我们自己的条面进程**。

    以前只查 tasklist 里有没有这个 PID——Windows 会复用 PID，条面退出后
    锁文件里残留的 3892 被别的进程拿到，--wait-zcode 启动时就误判成
    "已经在跑"直接退出，条面永远起不来（用户报的"悬浮条没启动"）。
    所以还要核对命令行里确实有 hoverbar.py。查不到命令行（权限/时序）
    时退回旧的 PID 存在性判断。"""
    try:
        out = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/NH"],
                             capture_output=True, text=True,
                             encoding="mbcs", errors="replace").stdout
        if str(pid) not in out:
            return False
    except Exception:
        return True
    try:
        out = subprocess.run(
            ["powershell", "-NoProfile", "-Command",
             "(Get-CimInstance Win32_Process -Filter "
             f"'ProcessId={pid}').CommandLine"],
            capture_output=True, text=True, timeout=15,
            encoding="utf-8", errors="replace").stdout or ""
        return "hoverbar" in out.lower()
    except Exception:
        return True      # 命令行查不到就按旧的"活着"算，宁可保守


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


def install_plugin() -> int:
    """把 zcode-plugin/ 装成本地 MCP App 插件（复制 + 写注册表）。"""
    import datetime
    import json
    import shutil
    home = os.path.expanduser("~")
    here = os.path.dirname(os.path.abspath(__file__))
    src = os.path.join(here, "zcode-plugin")
    if not os.path.isdir(src):
        print("zcode-plugin/ 不存在，这个副本没有附带插件")
        return 1
    reg = os.path.join(home, ".zcode", "cli", "plugins")
    lm = os.path.join(home, ".zcode", "local-marketplaces")
    dst = os.path.join(lm, "zcode-token-bar-control")
    if os.path.isdir(dst):
        shutil.rmtree(dst)
    os.makedirs(dst, exist_ok=True)
    for name in (".zcode-plugin", "dist"):
        shutil.copytree(os.path.join(src, name), os.path.join(dst, name))
    example = os.path.join(src, "marketplace.example.json")
    if os.path.isfile(example):
        shutil.copy2(example, os.path.join(lm, "marketplace.json"))
    cache_dir = os.path.join(reg, "cache", "local",
                             "zcode-token-bar-control", "0.1.0")
    if os.path.isdir(cache_dir):
        shutil.rmtree(cache_dir)
    shutil.copytree(dst, cache_dir)

    stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
    now_iso = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%S.000Z")

    kmp = os.path.join(reg, "known_marketplaces.json")
    try:
        data = json.load(open(kmp, encoding="utf-8"))
    except Exception:
        data = {"version": 1, "marketplaces": []}
    shutil.copy2(kmp, kmp + ".bak-" + stamp)
    data["marketplaces"] = [m for m in data.get("marketplaces", [])
                            if m.get("id") != "local"]
    data["marketplaces"].append({
        "id": "local",
        "source": {"source": "directory", "path": lm},
        "name": "local",
        "description": "本机插件（本地目录）",
        "addedAt": now_iso,
        "pluginCount": 1,
        "lastUpdated": now_iso,
    })
    open(kmp, "w", encoding="utf-8").write(
        json.dumps(data, ensure_ascii=False, indent=2))

    ip = os.path.join(reg, "installed_plugins.json")
    try:
        data = json.load(open(ip, encoding="utf-8"))
    except Exception:
        data = {"version": 1, "plugins": []}
    shutil.copy2(ip, ip + ".bak-" + stamp)
    data["plugins"] = [x for x in data.get("plugins", [])
                       if x.get("id") != "zcode-token-bar-control@local"]
    data["plugins"].append({
        "id": "zcode-token-bar-control@local",
        "name": "zcode-token-bar-control",
        "marketplace": "local",
        "version": "0.1.0",
        "installPath": cache_dir,
        "installedAt": now_iso,
        "updatedAt": now_iso,
        "scope": "user",
        "source": "./zcode-token-bar-control",
    })
    open(ip, "w", encoding="utf-8").write(
        json.dumps(data, ensure_ascii=False, indent=2))
    print("插件已安装：重启 ZCode，点右上角「切换面板」→ Token Bar")
    return 0


def main(argv):
    global DEBUG
    DEBUG = "--debug" in argv
    if "--once" in argv:
        return once()
    if "--install-plugin" in argv:
        return install_plugin()
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
    if "--wait-zcode" in argv:
        return wait_zcode()

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
