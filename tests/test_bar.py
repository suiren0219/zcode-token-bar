"""悬浮条回归测试。一个文件跑完所有场景，不依赖外部框架。

用法：
    python tests/test_bar.py            # 全部
    python tests/test_bar.py pin pages  # 只跑指定组

为什么放在仓库里而不是临时目录：这些脚本以前放在 %TEMP%，被系统清理过
三次，每次都得重写。测试是代码的一部分，跟代码一起版本控制。

设计约束：
- 每个用例都在打桩环境里跑（假数据源、假窗口句柄、假注册表），
  不碰真实 ZCode、不碰真实配置、不弹真窗口抢焦点。
- 共用一套 GUI 环境：tkinter 起一次，各用例之间重置状态。
"""
from __future__ import annotations

import ctypes
import json
import os
import queue
import sys
import tempfile
import time
from pathlib import Path

try:
    ctypes.windll.user32.SetProcessDpiAwarenessContext(-4)
except Exception:
    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(2)
    except Exception:
        pass

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.chdir(ROOT)

import hoverbar as hb

TMP = Path(tempfile.mkdtemp(prefix="bar_test_"))
hb.CONFIG = TMP / "hoverbar.json"
hb._wp_read = lambda: None          # 壁纸段整体跳过

DEMO = {
    "session": {"sid": "sess_demo_0001", "title": "demo-project",
                "active": True, "input": 412000, "output": 18600,
                "cache_read": 268000, "cache_write": 0, "total": 430600,
                "turns": 7, "requests": 42, "ctx": 412000,
                "sub": {}, "last_turn": {}, "last": {}},
    "last_turn": {"total": 84200, "requests": 5, "input": 76000,
                  "cache_read": 51000, "output": 1900, "duration_ms": 41000},
    "last": {"model": "demo-model", "tps": 158.2, "ttft_ms": 1200},
    "today": {"total": 2860000, "requests": 318},
    "tools": {"total": 96, "errors": 2,
              "list": [{"name": "Bash", "count": 54, "errors": 1},
                       {"name": "Edit", "count": 21, "errors": 0}]},
    "code": {"add": 412, "del": 88, "files": 9},
    "context_window": 1000000, "remote": False, "remote_error": None,
    "not_local": [], "pinned": "", "pin_lost": False,
}

# ---------------------------------------------------------------- 打桩环境
ZC = 198836
OWN = 555001


class FakeSrc:
    """假快照源：不起子进程，队列由测试自己塞。"""

    def __init__(self, interval_ms, pin_fn=None):
        self.pin_fn = pin_fn or (lambda: "")
        self.q = queue.Queue()
        self._alive = True

    def start(self):
        pass

    def stop(self):
        self._alive = False

    def alive(self):
        return self._alive


ENV = {"win": (100, 100, 900, 700), "work": (0, 0, 1920, 1040),
       "hwnd": ZC, "fg": ZC}


def install_stubs():
    hb.SnapshotSource = FakeSrc
    hb.zcode_hwnd = lambda: ENV["hwnd"]
    # hwnd=0（找不到窗口）时要像真的一样返回 None，否则测试测不到隐藏分支
    hb.win_state = lambda h: (ENV["win"] if h else None)
    hb.monitor_work = lambda h: ENV["work"]
    hb.foreground_is = lambda h: ENV["fg"] == h
    hb.lbutton_down = lambda: False


install_stubs()

ok = True
FAILED: list[str] = []


def check(name, cond):
    global ok
    if cond:
        print("  PASS  " + name)
    else:
        print("  FAIL  " + name)
        ok = False
        FAILED.append(name)


def new_bar(**over):
    """建一个干净的条面实例。每组用例都从默认配置起步——
    上一组可能把配置写进磁盘（钉住开关就会 save_settings），
    串下去会让后面的位置断言莫名其妙地失败。"""
    try:
        hb.CONFIG.unlink()
    except OSError:
        pass
    cfg = hb.load_settings()
    cfg.update(over)
    bar = hb.HoverBar(cfg)
    bar.data = dict(DEMO)
    bar._draw()
    bar.root.update_idletasks()
    bar.root.update()
    return bar


def kill(bar):
    try:
        bar.src.stop()
        bar.root.destroy()
    except Exception:
        pass


# ---------------------------------------------------------------- 用例
def t_pin():
    """图钉：开关语义、位置冻结、pin_lost 自动解钉。"""
    bar = new_bar()
    bar._place(); bar.root.update()
    px1, py1, px2, py2 = bar._pin_rect()
    check("pin 热区在条面左端", px2 > px1 and px1 <= 24)
    check("未钉时底色是 EF_CHIP",
          bar.canvas.itemcget(bar.pin_bg, "fill") == hb.EF_CHIP)

    bar._toggle_pin()
    saved = json.loads(hb.CONFIG.read_text(encoding="utf-8"))
    check("钉住后 pinned=True", bar.cfg["pinned"] is True)
    check("钉住记录了当前会话", saved["pinned_sid"] == "sess_demo_0001")
    check("位置已冻结", saved["pin_x"] >= 0)
    check("底色变 acid yellow",
          bar.canvas.itemcget(bar.pin_bg, "fill") == hb.EF_SIGNAL)

    before = (bar.root.winfo_x(), bar.root.winfo_y())

    class E:
        x, y = 100, 10
        x_root, y_root = before[0] + 130, before[1] + 10

    bar._drag_start(E())
    check("钉住时不进入拖动", bar._drag is None)

    bar._toggle_pin()
    saved = json.loads(hb.CONFIG.read_text(encoding="utf-8"))
    check("解除后字段清空", saved["pinned"] is False
          and saved["pinned_sid"] == "" and saved["pin_x"] == -1)
    check("解除后底色回 EF_CHIP",
          bar.canvas.itemcget(bar.pin_bg, "fill") == hb.EF_CHIP)

    bar._toggle_pin()
    bar.data = dict(DEMO, pin_lost=True)
    bar._check_pin_lost()
    check("pin_lost 自动解钉", bar.cfg["pinned"] is False)

    bar.data = {}
    bar._toggle_pin()
    check("无会话数据时钉不上", bar.cfg["pinned"] is False)
    kill(bar)


def t_flicker():
    """闪屏：内容没变就一次都不重画、不动窗口。"""
    bar = new_bar()
    bar._place(); bar.root.update()
    COUNT = {"geo": 0, "clear": 0}
    rg, rdel = bar.root.geometry, bar.canvas.delete
    bar.root.geometry = lambda *a, **k: (
        COUNT.__setitem__("geo", COUNT["geo"] + 1), rg(*a, **k))[1]

    def cdel(*a, **k):
        if a and a[0] == "all":
            COUNT["clear"] += 1
        return rdel(*a, **k)

    bar.canvas.delete = cdel
    for _ in range(10):
        bar._draw(); bar._place(); bar.root.update()
    check("10 拍空转：geometry 0 次", COUNT["geo"] == 0)
    check("10 拍空转：delete(all) 0 次", COUNT["clear"] == 0)

    COUNT["clear"] = 0
    bar.data = dict(DEMO)
    bar.data["last"] = dict(DEMO["last"], tps=321.9)
    bar._draw(); bar.root.update()
    check("数字变了：恰好重画 1 次", COUNT["clear"] == 1)
    COUNT["clear"] = 0
    for _ in range(3):
        bar._draw()
    check("同一份数据再画 3 次：0 次", COUNT["clear"] == 0)

    COUNT["geo"] = 0
    bar.cfg["anchor"] = "top-right"
    bar._place(); bar.root.update()
    check("换锚点：geometry 1 次", COUNT["geo"] == 1)
    COUNT["geo"] = 0
    for _ in range(5):
        bar._place()
    check("新锚点稳定后：geometry 0 次", COUNT["geo"] == 0)
    bar.root.geometry = rg
    bar.canvas.delete = rdel
    kill(bar)


def t_fs():
    """全屏适配：F11 / 最大化下六向贴靠都在可见区。"""
    CASES = [
        ("F11·任务栏自动隐藏", (0, 0, 1920, 1080), (0, 0, 1920, 1080)),
        ("F11·任务栏可见", (0, 0, 1920, 1080), (0, 0, 1920, 1032)),
        ("最大化·隐形边框", (-9, -9, 1938, 1098), (0, 0, 1920, 1080)),
        ("最大化·任务栏可见", (-9, -9, 1938, 1098), (0, 0, 1920, 1032)),
        ("普通窗口", (100, 100, 800, 600), (0, 0, 1920, 1032)),
    ]
    ANCHORS = ["bottom-left", "bottom-center", "bottom-right",
               "top-left", "top-center", "top-right"]
    bar = new_bar()
    for desc, wr, wa in CASES:
        ENV["win"], ENV["work"] = wr, wa
        bad = []
        for a in ANCHORS:
            bar.cfg["anchor"] = a
            bar.cfg["scope"] = "window"
            bar._place()
            bar.root.update_idletasks()
            x, y = bar.root.winfo_x(), bar.root.winfo_y()
            w, h = bar.root.winfo_width(), bar.root.winfo_height()
            if not (wa[0] <= x and wa[1] <= y
                    and x + w <= wa[0] + wa[2] and y + h <= wa[1] + wa[3]):
                bad.append(a)
        check("%s：六向全在界内" % desc, not bad)

    ENV["win"], ENV["work"] = (100, 100, 800, 600), (0, 0, 1920, 1032)
    bar.cfg.update({"anchor": "bottom-left", "scope": "window",
                    "offset_x": 16, "offset_y": 6})
    bar._place(); bar.root.update_idletasks()
    bw, bh = bar._last_draw
    check("普通窗口坐标与公式一致",
          (bar.root.winfo_x(), bar.root.winfo_y())
          == (116, 100 + 600 - bh - 6))

    ENV["win"] = (5000, 5000, 50, 50)
    bar.cfg["anchor"] = "bottom-left"
    bar._placed = None
    try:
        bar._place()
        check("极端窗口不崩溃", True)
    except Exception as e:
        check("极端窗口不崩溃（%s）" % e, False)
    ENV["win"] = (100, 100, 900, 700)
    kill(bar)


def t_status():
    """状态页四行：各自独立判断、颜色对、自启读写注册表。"""
    REG = {}

    class FakeKey:
        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

    class FakeWinReg:
        HKEY_CURRENT_USER = object()
        KEY_SET_VALUE = 2
        REG_SZ = 1

        @staticmethod
        def OpenKey(root, path, reserved=0, access=0):
            return FakeKey()

        @staticmethod
        def CreateKey(root, path):
            return FakeKey()

        @staticmethod
        def QueryValueEx(k, name):
            if name not in REG:
                raise FileNotFoundError(name)
            return REG[name], 1

        @staticmethod
        def SetValueEx(k, name, reserved, typ, value):
            REG[name] = value

        @staticmethod
        def DeleteValue(k, name):
            if name not in REG:
                raise FileNotFoundError(name)
            del REG[name]

    real = sys.modules.get("winreg")
    sys.modules["winreg"] = FakeWinReg
    REG.clear()
    try:
        bar = new_bar()
        dlg = hb.SettingsDialog(bar)
        dlg.top.update_idletasks(); dlg.top.update()
        check("四行状态齐全",
              set(dlg._st_labels) == {"run", "data", "zcode", "auto"})
        dlg._st_refresh()
        check("进程行给出 PID",
              "运行中" in str(dlg._st_labels["run"]["text"])
              and str(os.getpid()) in str(dlg._st_labels["run"]["text"]))
        check("数据链路正常", "正常" in str(dlg._st_labels["data"]["text"]))

        bar.src._alive = False
        dlg._st_refresh()
        check("serve 挂了标红",
              "断开" in str(dlg._st_labels["data"]["text"])
              and str(dlg._st_labels["data"]["fg"]) == hb.HOT)
        bar.src._alive = True

        ENV["hwnd"] = 0
        dlg._st_refresh()
        check("找不到窗口标红",
              "没找到" in str(dlg._st_labels["zcode"]["text"]))
        ENV["hwnd"] = ZC
        dlg._st_refresh()

        REG.clear()
        check("注册表空 → 未开", dlg._st_autostart_on() is False)
        dlg.v_autostart.set(True)
        check("勾上写注册表", "ZCodeTokenBar" in REG)
        check("命令带 --wait-zcode", "--wait-zcode" in REG["ZCodeTokenBar"])
        dlg.v_autostart.set(False)
        check("取消后清空", "ZCodeTokenBar" not in REG)
        dlg.close()
        kill(bar)
    finally:
        if real is not None:
            sys.modules["winreg"] = real
        else:
            sys.modules.pop("winreg", None)


def t_ghost():
    """丢失防护：拖动卡死自愈、单拍噪声不藏、位置偏离自愈。"""
    bar = new_bar()
    bar._place(); bar.root.update()

    HB = hb.HoverBar

    class Root:
        def after(self, *a):
            pass

    class FakeBar:
        def __init__(self):
            self.root = Root()
            self.cfg = bar.cfg
            self._drag = (5, 5)
            self._drag_t0 = time.time()
            self._pin_armed = self._gear_armed = False
            self.n = 0
            self._tick_anchor = HB._tick_anchor

        def _place(self):
            self.n += 1

    hb.lbutton_down = lambda: False
    f = FakeBar()
    HB._tick_anchor(f)
    check("松开但事件丢失：自动松手并恢复定位", f._drag is None and f.n == 1)

    hb.lbutton_down = lambda: True
    f = FakeBar()
    f._drag_t0 = time.time() - 40
    HB._tick_anchor(f)
    check("按住超时：强制松手", f._drag is None)

    f = FakeBar()
    HB._tick_anchor(f)
    check("正常拖动中不定位", f._drag is not None and f.n == 0)
    hb.lbutton_down = lambda: False

    bar._show(); bar._place(); bar.root.update()
    bar._miss = 0
    ENV["hwnd"] = 0
    bar._place(); bar.root.update()
    check("单拍找不到窗口：还不藏", bar._visible is True)
    bar._place(); bar.root.update()
    check("连续两拍才藏", bar._visible is False)
    ENV["hwnd"] = ZC
    bar._place(); bar.root.update()
    check("窗口回来恢复显示", bar._visible is True)

    target = (bar.root.winfo_x(), bar.root.winfo_y())
    bar.root.geometry("+%d+%d" % (target[0] + 200, target[1] + 150))
    bar.root.update()
    bar._place(); bar.root.update()
    check("被外部挪走：下一拍纠正",
          (bar.root.winfo_x(), bar.root.winfo_y()) == target)

    bar.root.withdraw(); bar.root.update()
    bar._place(); bar.root.update()
    check("被外部藏起来：下一拍重新出现", bar._visible is True)
    kill(bar)


def t_truth():
    """窗口状态以事实为准，不存副本。"""
    bar = new_bar()
    bar._place(); bar.root.update()
    check("_visible 是 property",
          isinstance(type(bar).__dict__["_visible"], property))
    src = (ROOT / "hoverbar.py").read_text(encoding="utf-8")
    check("无 _placed 赋值", "self._placed = " not in src)
    check("无 _verify_t", "_verify_t" not in src)
    check("无 _visible 赋值", "self._visible = " not in src)

    bar.root.withdraw()
    settled = False
    for _ in range(40):            # unmap 事件偶尔要一两拍才到，别当时序断言
        bar.root.update()
        if bar._visible is False:
            settled = True
            break
        time.sleep(0.02)
    check("外部 withdraw 后读到 False（等 unmap 事件）", settled)
    bar._place(); bar.root.update()
    check("下一拍自动恢复", bar._visible is True)

    for desc, mangle in (("挪走", lambda: bar.root.geometry("+900+700")),
                         ("改尺寸", lambda: bar.root.geometry("300x10")),
                         ("挪到屏外", lambda: bar.root.geometry("+-2000+-2000"))):
        bar._place(); bar.root.update()
        tgt = (bar.root.winfo_x(), bar.root.winfo_y())
        mangle(); bar.root.update()
        bar._place(); bar.root.update()
        check("%s → 下一拍纠正" % desc,
              (bar.root.winfo_x(), bar.root.winfo_y()) == tgt)

    N = 300
    t0 = time.perf_counter()
    for _ in range(N):
        bar._real_state()
    dt = (time.perf_counter() - t0) / N * 1000
    check("_real_state 单次 < 0.05ms（实测 %.4fms）" % dt, dt < 0.05)
    kill(bar)


def t_spin():
    """加载动画：弧线出现、真的在转、不触发重绘、数据到位自动停。"""
    bar = new_bar()
    bar.data = None
    bar._draw(); bar.root.update()
    check("loading 置位", bar._loading is True)
    check("弧线存在且酸黄",
          bar._spin_id is not None
          and bar.canvas.itemcget(bar._spin_id, "outline") == hb.EF_SIGNAL)
    check("暗色轨道存在",
          bar._spin_track is not None
          and bar.canvas.itemcget(bar._spin_track, "outline") == hb.TRACK)

    a0 = float(bar.canvas.itemcget(bar._spin_id, "start"))
    n0 = len(bar.canvas.find_all())
    bar._spin_tick()
    a1 = float(bar.canvas.itemcget(bar._spin_id, "start"))
    check("真的在转 (%.0f° → %.0f°)" % (a0, a1), a1 != a0)
    check("转一格不新增图元", len(bar.canvas.find_all()) == n0)

    exts = []
    for _ in range(40):
        bar._spin_tick()
        exts.append(float(bar.canvas.itemcget(bar._spin_id, "extent")))
    check("弧长在呼吸（幅度 > 60°）", max(exts) - min(exts) > 60)
    check("弧长在 100~260° 内", min(exts) >= 99.9 and max(exts) <= 260.1)

    if bar._spin_job:
        bar.root.after_cancel(bar._spin_job)
        bar._spin_job = None
    bar.data = dict(DEMO)
    bar._draw(); bar.root.update()
    check("数据到位自动停", bar._loading is False and bar._spin_job is None)
    check("弧线清掉",
          not any(bar.canvas.type(i) == "arc" for i in bar.canvas.find_all()))
    kill(bar)


def t_wait():
    """--wait-zcode：待命、原地开条、已有实例则安静退出。"""
    CREATED = []

    class FakeBar:
        def __init__(self, cfg):
            CREATED.append("bar")

        def run(self):
            CREATED.append("run")

    real_bar, real_ls = hb.HoverBar, hb.load_settings
    real_pid, real_hwnd, real_sleep = (hb.running_pid, hb.zcode_hwnd,
                                       hb.time.sleep)
    hb.HoverBar = FakeBar
    hb.load_settings = lambda: {}
    hb.time.sleep = lambda s: None
    try:
        hb.running_pid = lambda: 4321
        check("已有实例：直接返回不起第二条",
              hb.wait_zcode() == 0 and not CREATED)

        CREATED.clear()
        hb.running_pid = lambda: None
        seq = iter([0, 0, ZC])
        hb.zcode_hwnd = lambda: next(seq)
        check("ZCode 出现后原地开条",
              hb.wait_zcode() == 0 and CREATED == ["bar", "run"])

        CREATED.clear()
        seq = iter([0, ZC])
        hb.zcode_hwnd = lambda: next(seq)
        hb.running_pid = lambda: 9999
        check("待命期间已有实例：不重复开",
              hb.wait_zcode() == 0 and not CREATED)
    finally:
        hb.HoverBar, hb.load_settings = real_bar, real_ls
        hb.running_pid, hb.zcode_hwnd, hb.time.sleep = (real_pid, real_hwnd,
                                                        real_sleep)

    import autostart
    check("自启命令带 --wait-zcode", "--wait-zcode" in autostart.command())


def t_pages():
    """分页面板：七页、只显示当前页、高度随页收放、宽度固定。"""
    bar = new_bar()
    dlg = hb.SettingsDialog(bar)
    dlg.top.update_idletasks(); dlg.top.update()
    KEYS = ["pos", "look", "items", "panel", "behav", "upd", "stat"]
    check("七个页面齐全", list(dlg._pages) == KEYS)
    check("七个标签齐全", list(dlg._tab_cvs) == KEYS)
    check("默认第一页", dlg._cur_page == "pos")

    top_h = dlg.top.winfo_reqheight()
    scr_h = dlg.top.winfo_screenheight()
    check("面板高度 < 屏幕 72%%（实测 %d/%d）" % (top_h, scr_h),
          top_h < scr_h * 0.72)

    heights, widths = {}, set()
    for k in KEYS:
        dlg._show_page(k, animate=False)
        dlg.top.update_idletasks(); dlg.top.update()
        heights[k] = dlg._scroll_cv.winfo_reqheight()
        widths.add(dlg.top.winfo_reqwidth())
        shown = [kk for kk, f in dlg._pages.items() if f.winfo_ismapped()]
        check("切到 %s：只有它在显示" % k, shown == [k])
    check("各页视口高度一致（统一面积，不再忽大忽小）",
          len(set(heights.values())) == 1)
    check("统一高度等于最高页（封顶内）",
          all(h == min(dlg._max_h, dlg._cap_h) for h in heights.values()))
    check("七页宽度一致（不左右跳）", len(widths) == 1)

    dlg._show_page("look", animate=True)
    # 第一帧是同步发出的：y 立刻在偏移位，定时器已排好
    y0 = int(dlg._pages["look"].place_info().get("y", 0))
    check("切页动画已排队且首帧有位移（y=%d）" % y0,
          dlg._slide_job is not None and y0 > 0)
    for _ in range(60):
        dlg.top.update(); time.sleep(0.01)
        if dlg._slide_job is None:
            break
    check("动画落定在 y=0",
          int(dlg._pages["look"].place_info().get("y", -1)) == 0)
    # 切页完全不改高度（v73 统一面积）
    cv = dlg._scroll_cv
    fixed = getattr(dlg, "_fixed_h", None)
    dlg._show_page("items", animate=True)
    check("切页高度恒定（统一面积）",
          fixed is not None and int(cv.cget("height")) == fixed)
    hj = getattr(dlg, "_h_job", None)
    check("没有悬挂的高度任务", not (hj and hj.get("job")))

    dlg._show_page("items", animate=False)
    dlg.v_show["speed"].set(False)
    dlg.v_lab.set(False)
    dlg.apply()
    saved = json.loads(hb.CONFIG.read_text(encoding="utf-8"))
    check("跨页的值都能写盘",
          saved["labels"] is False and saved["show"]["speed"] is False)
    check("应用后面板关闭", getattr(bar, "_dlg", None) is None)
    kill(bar)


def t_tip():
    """浮层：先藏后显、不透明、就地刷新不重建、行数变了才重建。"""
    bar = new_bar()
    t = bar._tip_build()
    check("build 后不可见", t.winfo_ismapped() == 0)
    bar._tip_position(t)
    bar.root.update()
    check("定位后仍不可见（不闪）", t.winfo_ismapped() == 0)
    t.deiconify(); bar.root.update()
    check("deiconify 后才出现", t.winfo_ismapped() == 1)
    try:
        alpha = float(t.attributes("-alpha"))
    except Exception:
        alpha = 1.0
    check("浮层完全不透明", alpha == 1.0)
    bar._tip_hide()

    bar._hover = None
    bar._tip_show()
    bar.root.update()
    t1 = bar._tip
    check("浮层已显示", t1 is not None and t1.winfo_exists())
    bar.data = dict(DEMO)
    bar.data["last"] = dict(DEMO["last"], tps=321.9)
    bar._tip_refresh()
    bar.root.update()
    check("只改值不重建窗口", bar._tip is t1)
    texts = [str(v["text"]) for (k, v) in bar._tip_row_labels]
    check("值已就地更新", any("321.9" in x for x in texts))

    bar.data = dict(DEMO)
    bar.data["tools"] = {}
    bar.data["code"] = {}
    bar._tip_refresh()
    bar.root.update()
    check("行数变了才重建", bar._tip is not t1)
    bar._tip_hide()
    check("_tip_hide 清掉行缓存",
          bar._tip is None and bar._tip_rows is None
          and bar._tip_row_labels is None)
    kill(bar)


def t_follow():
    """跟会话走：顶层快照跟「用户正在打的会话」，不被后台完成拽走。"""
    import sqlite3
    import zusage as zu

    def ins(mem, table, **vals):
        """按真实 schema 插行：没给的 NOT NULL 列按类型填空串/0。
        ZCode 的表约束一大把（project_id/slug/directory/logical_request_id…），
        逐列手补是打地鼠，让 pragma 说话。"""
        info = list(mem.execute(f"pragma table_info({table})"))
        use = dict(vals)
        for _cid, name, ctype, notnull, dflt, pk in info:
            if name in use or (notnull and (dflt is not None or pk)):
                continue
            if notnull:
                use[name] = "" if "text" in (ctype or "").lower() else 0
        q = ",".join(use)
        mem.execute(f"insert into {table}({q}) values({','.join('?' * len(use))})",
                    list(use.values()))

    real = sqlite3.connect(
        f"file:{Path.home() / '.zcode/cli/db/db.sqlite'}?mode=ro", uri=True)
    mem = sqlite3.connect(":memory:")
    mem.row_factory = sqlite3.Row       # _session_snapshot 按列名取值（真实行为）
    for (sql,) in real.execute(
            "select sql from sqlite_master where type='table' and sql is not null"):
        if "sqlite_" in sql:
            continue
        mem.execute(sql)

    now = int(time.time() * 1000)
    ins(mem, "session", id="sess_aaa", project_id="proj", slug="a",
        directory="C:/demo/aaa", title="会话A（正在打字）",
        time_created=now - 600000, time_updated=now - 600000)
    ins(mem, "session", id="sess_bbb", project_id="proj", slug="b",
        directory="C:/demo/bbb", title="会话B（后台完成）",
        time_created=now - 1200000, time_updated=now - 1200000)
    # B 的请求完成得比 A 晚：旧逻辑会把条面拽向 B
    ins(mem, "model_usage", session_id="sess_bbb", completed_at=now - 30000,
        status="completed", input_tokens=1000, computed_total_tokens=1000)
    ins(mem, "model_usage", session_id="sess_aaa", completed_at=now - 300000,
        status="completed", input_tokens=2000, computed_total_tokens=2000)
    # A 的用户输入最新：这才是「正在看的会话」
    ins(mem, "input_history", session_id="sess_aaa", time_created=now - 10000,
        kind="prompt")
    mem.commit()

    d = zu._snapshot_with(mem)
    check("打字会话优先于最近完成（A 而非 B）", d["session"]["sid"] == "sess_aaa")

    # 新会话：发了一条消息但还没有任何 usage 行 → 也跟，而不是留在旧会话
    ins(mem, "session", id="sess_ccc", project_id="proj", slug="c",
        directory="C:/demo/ccc", title="会话C（刚发出第一条）",
        time_created=now - 5000, time_updated=now - 5000)
    ins(mem, "input_history", session_id="sess_ccc", time_created=now - 4000,
        kind="prompt")
    # 发消息后、首请求落库前：最新 message 是用户说的 → 生成中而不是空闲
    ins(mem, "message", session_id="sess_ccc", time_created=now - 3000,
        sequence=1, data=json.dumps({"role": "user"}))
    mem.commit()
    d = zu._snapshot_with(mem)
    check("新会话零请求也立刻跟上", d["session"]["sid"] == "sess_ccc"
          and d["session"]["requests"] == 0)
    check("这个窗口期显示生成中", d["session"]["active"] is True)

    # 会话被删 → 它的 user 消息也一并出局（join 排除），回到最近的 aaa
    mem.execute("update session set time_deleted=? where id=?",
                (now, "sess_ccc"))
    mem.commit()
    d = zu._snapshot_with(mem)
    check("已删除会话的用户消息被忽略",
          d["session"]["sid"] in ("sess_aaa", "sess_bbb"))

    # message/input_history 全空 → 回退旧逻辑（最近完成 = B）
    mem.execute("delete from input_history")
    mem.execute("delete from message")
    mem.commit()
    d = zu._snapshot_with(mem)
    check("无用户消息回退最近完成", d["session"]["sid"] == "sess_bbb")

    # helper 自身的边界
    check("_latest_typed_sid 空表返回 None",
          zu._latest_typed_sid(mem) is None)
    mem.close()


def t_attach():
    """面板贴在条面上：零间隙、右边缘对齐齿轮、下面放不下翻上面、跟着条面走。"""
    bar = new_bar()
    bar._place(); bar.root.update()
    bx, by = bar.root.winfo_x(), bar.root.winfo_y()
    bw, bh = bar.root.winfo_width(), bar.root.winfo_height()
    dlg = hb.SettingsDialog(bar)
    dlg.top.update(); time.sleep(0.25); dlg.top.update()
    x, y = dlg.top.winfo_x(), dlg.top.winfo_y()
    w, h = dlg.top.winfo_width(), dlg.top.winfo_height()
    sh = dlg.top.winfo_screenheight()

    below = by + bh + h <= sh - 8
    if below:
        check("贴在条面下沿（间距 0，实测 %d）" % (y - (by + bh)),
              y == by + bh and dlg._drop == "down")
    else:
        check("下面放不下 → 贴上沿（间距 0，实测 %d）" % (by - (y + h)),
              y + h == by and dlg._drop == "up")
    check("右边缘对齐齿轮（差 %d）" % ((bx + bw) - (x + w)),
          (bx + bw) - (x + w) == 0)
    check("面板夹在屏内", 8 <= x and x + w <= dlg.top.winfo_screenwidth() - 8)

    # 条面移动 → 面板跟着重贴（_follow_panel 由 _settle 调用）
    ENV["win"] = (400, 300, 900, 700)
    bar._place(); bar.root.update(); time.sleep(0.1)
    bx2, by2 = bar.root.winfo_x(), bar.root.winfo_y()
    if (bx2, by2) != (bx, by):
        check("条面挪了，面板跟着挪", dlg.top.winfo_x() != x
              or dlg.top.winfo_y() != y)

    # 条面收起 → 面板一起关
    bar._hide(); bar.root.update()
    check("条面收起时面板一起关闭", getattr(bar, "_dlg", None) is None)
    ENV["win"] = (100, 100, 900, 700)
    kill(bar)


def t_flash():
    """启动闪屏：一律先定位再显示。"""
    src = (ROOT / "hoverbar.py").read_text(encoding="utf-8")
    i = src.index("def _settle(")
    j = src.index("def _real_state(", i)
    body = src[i:j]
    gi, di = body.find("self.root.geometry("), body.find("self.root.deiconify(")
    check("_settle 里 geometry 在 deiconify 之前", 0 <= gi < di)
    check("_settle 对未映射窗口先定位", "winfo_ismapped()" in body)

    i = src.index("top = tk.Toplevel(bar.root)")
    j = src.index("top.overrideredirect(True)", i)
    check("面板创建后立刻 withdraw", "top.withdraw()" in src[i:j])
    k = src.index("self._place_off_bar()", j)
    seg2 = src[k:k + 200]
    check("面板摆位之后才走入场动画", "self._animate_open()" in seg2)
    ai = src.index("def _animate_open(")
    aseg = src[ai:ai + 1600]
    # 只查动画分支：geometry(y+12) 之后的第一个 deiconify 才是入场用的；
    # reduced-motion 分支里直接 deiconify 是对的（无位移，无需先定位）
    gi = aseg.find('top.geometry("+%d+%d" % (x, y - dy))')
    check("入场动画内部也是先定位再 deiconify",
          gi >= 0 and aseg.find("top.deiconify()", gi) > gi)
    check("入场动画尊重系统动效开关", "motion_ok()" in aseg)

    i = src.index("def _tip_build(")
    j = src.index("def _tip_show(", i)
    check("浮层建好之后 withdraw", "t.withdraw()" in src[i:j])
    i = src.index("def _tip_show(")
    j = src.index("def _tip_position(", i)
    body2 = src[i:j]
    check("浮层先定位再 deiconify",
          body2.find("self._tip_position(t)") < body2.find("t.deiconify()"))

    bar = new_bar()
    bar._place(); bar.root.update()
    pos = (bar.root.winfo_x(), bar.root.winfo_y())
    check("条面首次出现不在 (0,0)（实测 %s）" % (pos,), pos != (0, 0))
    bar._hide(); bar.root.update()
    bar._place(); bar.root.update()
    check("隐藏再出现位置一致",
          (bar.root.winfo_x(), bar.root.winfo_y()) == pos)
    kill(bar)





def t_ctxwin():
    """上下文窗口：v2/config 新数据源 + 用量推断（>100% 的根治）。"""
    import zusage as zu

    # 纯函数：档位推断
    check("用量 250.5K、已知 200K → 推断 256K",
          zu._infer_window(250500, 200000) == 256000)
    check("无已知值 → 装得下用量的最小档",
          zu._infer_window(250500, None) == 256000)
    check("已知 1M 更大时不缩小",
          zu._infer_window(250500, 1000000) == 1000000)
    check("用量 3M → 向上取整到 3M",
          zu._infer_window(3_000_000, None) == 3_000_000)
    check("用量没超窗 → 不改", zu._infer_window(100000, 200000) == 200000)
    check("用量 0 → 不改", zu._infer_window(0, 200000) == 200000)

    # 查找链：v2/config 数据源参与（打桩避免依赖本机文件内容）。
    # hy4free 会被运行期 rules 的通配规则命中 200K——那正是 bug 现场，
    # 用量推断（下面的端到端）才是它的解；v2 数据源服务 rules 没覆盖的模型。
    real_v2 = zu._v2_config_windows
    zu._CTX_WINDOW_CACHE.clear()
    try:
        check("rules 通配命中 hy4free = 200K（bug 现场记录在案）",
              zu.lookup_context_window("hy4free") == 200000)
        zu._v2_config_windows = lambda: {"mystery-model": 400000}
        zu._CTX_WINDOW_CACHE.clear()
        check("rules 没覆盖时走 v2/config",
              zu.lookup_context_window("mystery-model") == 400000)
        zu._v2_config_windows = lambda: {}
        zu._CTX_WINDOW_CACHE.clear()
        check("v2/config 也没有 → 回退旧目录/None",
              zu.lookup_context_window("mystery-model") in (None,)
              or isinstance(zu.lookup_context_window("mystery-model"), int))
    finally:
        zu._v2_config_windows = real_v2
        zu._CTX_WINDOW_CACHE.clear()

    # 端到端：目录查不到的模型，用量超过窗口 → 推断 + 标记
    # _AGG_CACHE 是模块级 2s 缓存，前面的组可能用别的库填过 "scan"；
    # 不清的话 latest_sid 会指向上一条库的会话，ctx=0，用例偶发失败。
    zu._AGG_CACHE.clear()
    zu._TASK_CACHE.clear()
    real_lookup = zu.lookup_context_window
    import sqlite3
    real = sqlite3.connect(
        f"file:{Path.home() / '.zcode/cli/db/db.sqlite'}?mode=ro", uri=True)
    mem = sqlite3.connect(":memory:")
    mem.row_factory = sqlite3.Row
    for (sql,) in real.execute(
            "select sql from sqlite_master where type='table' and sql is not null"):
        if "sqlite_" in sql:
            continue
        mem.execute(sql)
    now = int(time.time() * 1000)

    def ins(mem_, table, **vals):
        info = list(mem_.execute(f"pragma table_info({table})"))
        use = dict(vals)
        for _c, name, ctype, notnull, dflt, pk in info:
            if name in use or (notnull and (dflt is not None or pk)):
                continue
            if notnull:
                use[name] = "" if "text" in (ctype or "").lower() else 0
        q = ",".join(use)
        mem_.execute(f"insert into {table}({q}) values({','.join('?' * len(use))})",
                     list(use.values()))

    ins(mem, "session", id="sess_ctx", project_id="p", slug="c",
        directory="C:/demo", title="ctx 测试", time_created=now,
        time_updated=now)
    ins(mem, "model_usage", session_id="sess_ctx", completed_at=now - 10000,
        status="completed", model_id="mystery-model", input_tokens=250500,
        computed_total_tokens=250500)
    mem.commit()
    try:
        zu._CTX_WINDOW_CACHE.clear()
        zu.lookup_context_window = lambda mid: None   # 目录查不到
        d = zu._snapshot_with(mem)
        check("查不到的模型 + 用量 250.5K → 窗口推断 256K",
              d["context_window"] == 256000)
        check("标记为推断", d.get("context_inferred") is True
              and d.get("context_auto") is True)
        check("百分比不再超过 100",
              d["session"]["ctx"] / d["context_window"] <= 1.0)
    finally:
        zu.lookup_context_window = real_lookup
        zu._CTX_WINDOW_CACHE.clear()
        zu._AGG_CACHE.clear()
        mem.close()


def t_wallpaper():
    """壁纸页的「刷新壁纸」：不换图、后台注入、结果回主线程着色。"""
    bar = new_bar()
    real_read = hb._wp_read
    real_refresh = hb._wp_refresh
    hb._wp_read = lambda: {"wallpaperPath": "C:/demo/w.png",
                           "wallpaperVisible": True, "dim": 25, "blur": 0,
                           "monet": True, "port": 9222}
    try:
        dlg = hb.SettingsDialog(bar)
        dlg.top.update()
        dlg._show_page("panel", animate=False)
        dlg.top.update()
        check("壁纸页有刷新状态行", hasattr(dlg, "_wp_msg"))

        hb._wp_refresh = lambda port: True
        dlg._wp_reinject()
        for _ in range(50):
            dlg.top.update()
            time.sleep(0.02)
            if "已重新注入" in dlg._wp_msg.cget("text"):
                break
        check("成功时提示已注入", "已重新注入" in dlg._wp_msg.cget("text"))
        check("成功用绿色", str(dlg._wp_msg.cget("fg")) == hb.OK)

        hb._wp_refresh = lambda port: False
        dlg._wp_reinject()
        for _ in range(50):
            dlg.top.update()
            time.sleep(0.02)
            if "注入失败" in dlg._wp_msg.cget("text"):
                break
        check("失败时说明端口没开", "注入失败" in dlg._wp_msg.cget("text"))
        check("失败用红色", str(dlg._wp_msg.cget("fg")) == hb.EF_DANGER)
        dlg.close()
    finally:
        hb._wp_read = real_read
        hb._wp_refresh = real_refresh
        kill(bar)





def t_layout():
    """统一面积 + 任意条面位置都不遮挡 + 压暗即时生效。"""
    # 1) 三种条面位置 × 七页：面板既不遮挡条面也不出屏，视口高度恒定
    for desc, wr in (("顶部", (100, 40, 1100, 300)),
                     ("中部", (100, 400, 1100, 400)),
                     ("底部", (100, 700, 1100, 330))):
        ENV["win"] = wr
        bar = new_bar()
        bar._place(); bar.root.update()
        by, bh = bar.root.winfo_y(), bar.root.winfo_height()
        dlg = hb.SettingsDialog(bar)
        dlg.top.update(); time.sleep(0.15); dlg.top.update()
        fixed, heights = dlg._fixed_h, set()
        for k in ("pos", "look", "items", "panel", "behav", "upd", "stat"):
            dlg._show_page(k, animate=False)
            dlg._place_off_bar()
            dlg.top.update()
            heights.add(dlg._scroll_cv.winfo_height())
            y, ph = dlg.top.winfo_y(), dlg.top.winfo_height()
            sh = dlg.top.winfo_screenheight()
            overlap = not (y + ph <= by or y >= by + bh)
            check("%s/%s：不遮挡、不出屏" % (desc, k),
                  not overlap and 0 <= y and y + ph <= sh)
        check("%s：七页面积统一" % desc, len(heights) == 1
              and fixed is not None)
        dlg.close(); kill(bar)
    ENV["win"] = (100, 100, 900, 700)

    # 2) 压暗即时生效（debounce 写盘 + 后台重注入），不点「应用」
    wp = Path(tempfile.mkdtemp(prefix="wp_")) / "data"
    wp.mkdir()
    (wp / "config.json").write_text(json.dumps({
        "port": 9222, "dim": 25, "blur": 0, "wallpaperVisible": True,
        "monet": True, "fit": "cover", "wallpaperPath": "C:/demo.png"}),
        encoding="utf-8")
    real_dir, real_refresh, real_read = (hb._wp_dir, hb._wp_refresh,
                                         hb._wp_read)
    hb._wp_dir, hb._wp_refresh = lambda: wp, lambda port: True
    hb._wp_read = lambda: json.loads((wp / "config.json").read_text(
        encoding="utf-8"))
    try:
        bar = new_bar()
        dlg = hb.SettingsDialog(bar)
        dlg.top.update()
        dlg._show_page("panel", animate=False)
        dlg.top.update()
        dlg.v_wp_dim.set(72)
        for _ in range(80):
            dlg.top.update(); time.sleep(0.015)
            if "已重新注入" in dlg._wp_msg.cget("text"):
                break
        cfg = json.loads((wp / "config.json").read_text(encoding="utf-8"))
        check("拖压暗滑块立即写盘（25 → 72）", cfg["dim"] == 72)
        check("并自动重注入", "已重新注入" in dlg._wp_msg.cget("text"))
        dlg.close(); kill(bar)
    finally:
        hb._wp_dir, hb._wp_refresh = real_dir, real_refresh
        hb._wp_read = real_read



def t_picker():
    """壁纸选择器：不弹系统文件对话框、列目录、选图、写配置、后台注入。"""
    import tkinter as tk
    bar = new_bar()
    real_read = hb._wp_read
    real_write = hb._wp_write
    real_refresh = hb._wp_refresh

    folder = TMP / "wp_pics"
    folder.mkdir(exist_ok=True)
    for n in ("b_wall.png", "a_wall.jpg", "note.txt", "c_wall.webp", "d_wall.bmp"):
        (folder / n).write_text("x", encoding="utf-8")

    wrote = []
    hb._wp_read = lambda: {
        "wallpaperPath": str(folder / "b_wall.png").replace("\\", "/"),
        "wallpaperVisible": True, "dim": 25, "blur": 0, "monet": True,
        "port": 9222}
    hb._wp_write = lambda patch: (wrote.append(patch), True)[1]
    hb._wp_refresh = lambda port: True

    # 关键断言：真要是有人偷偷改回 filedialog/messagebox，立刻抓出来
    called = []
    class _Boom:
        def __getattr__(self, name):
            def boom(*a, **k):
                called.append(name)
                raise AssertionError("碰了系统对话框: " + name)
            return boom
    real_filedialog = getattr(hb.tk, "filedialog", None)
    real_messagebox = getattr(hb.tk, "messagebox", None)
    hb.tk.filedialog = _Boom()
    hb.tk.messagebox = _Boom()

    try:
        dlg = hb.SettingsDialog(bar)
        dlg.top.update()
        dlg._show_page("panel", animate=False)
        dlg.top.update()
        check("壁纸页不再有「选择图片」按钮",
              not any(getattr(w, "text", "") == "选择图片…"
                      for w in _all_widgets(dlg.top)
                      if isinstance(w, hb._EfButton)))

        dlg._open_wallpaper_picker()
        dlg.top.update()
        pk = dlg._wp_picker
        check("选择器是 Toplevel 且已弹出", isinstance(pk, tk.Toplevel)
              and bool(pk.winfo_ismapped()))
        check("起始目录 = 当前壁纸所在目录",
              pk.v_dir.get().replace("\\", "/").lower()
              == str(folder).replace("\\", "/").lower())
        names = list(pk._list.get(0, "end"))
        check("只列图片且按名字排序",
              names == ["a_wall.jpg", "b_wall.png", "c_wall.webp", "d_wall.bmp"])
        check("没有碰系统对话框", not called)

        # 选第三张并确认
        pk._list.selection_clear(0, "end")
        pk._list.selection_set(2)
        pk._confirm()
        dlg.top.update()
        want = str(folder / "c_wall.webp").replace("\\", "/")
        check("选中路径写进配置",
              bool(wrote) and wrote[-1]["wallpaperPath"].lower() == want.lower()
              and wrote[-1]["wallpaperVisible"] is True)
        check("选择器已关闭", not pk.winfo_exists())

        # 后台注入结果回主线程
        for _ in range(50):
            dlg.top.update()
            time.sleep(0.02)
            if "已重新注入" in dlg._wp_msg.cget("text"):
                break
        check("注入结果回显在状态行",
              "已重新注入" in dlg._wp_msg.cget("text"))

        # 目录不存在时给提示，不崩
        dlg._open_wallpaper_picker()
        dlg.top.update()
        pk2 = dlg._wp_picker
        pk2.v_dir.set(str(TMP / "no_such_dir"))
        pk2._load()
        dlg.top.update()
        check("目录不存在有提示且不崩",
              "目录不存在" in pk2._msg.cget("text"))
        pk2._cancel()
        dlg.top.update()
        # --- 拖放：模拟 WM_DROPFILES 回调，验证直接选图不靠目录列表 ---
        dlg._open_wallpaper_picker()
        dlg.top.update()
        pk3 = dlg._wp_picker
        pic = folder / "a_wall.jpg"
        pk3._on_files([str(pic).replace("\\", "/")])
        dlg.top.update()
        want = str(pic).replace("\\", "/")
        check("拖入图片直接写配置",
              bool(wrote) and wrote[-1]["wallpaperPath"].lower() == want.lower())
        check("拖入后选择器已关闭", not pk3.winfo_exists())

        # 非图片被拒
        dlg._open_wallpaper_picker()
        dlg.top.update()
        pk4 = dlg._wp_picker
        pk4._on_files([str(folder / "note.txt")])
        dlg.top.update()
        check("拖入非图片不写入也不关闭",
              pk4.winfo_exists() and "不是图片" in pk4._drop_lbl.cget("text"))

        # 粘路径（带引号、反斜杠）也能用
        pk4.v_file.set('"%s"' % str(folder / "d_wall.bmp").replace("/", "\\"))
        pk4._use_path(pk4.v_file.get())
        dlg.top.update()
        check("粘贴带引号反斜杠路径可选图",
              wrote[-1]["wallpaperPath"].lower()
              == str(folder / "d_wall.bmp").replace("\\", "/").lower())
        check("全程仍未碰系统对话框", not called)
        dlg.close()

    finally:
        hb._wp_read, hb._wp_write, hb._wp_refresh = real_read, real_write, real_refresh
        hb.tk.filedialog = real_filedialog
        hb.tk.messagebox = real_messagebox
        kill(bar)


def _all_widgets(w):
    """递归列出子控件，给按钮文字断言用。"""
    out = []
    try:
        kids = w.winfo_children()
    except Exception:
        return out
    for k in kids:
        out.append(k)
        out.extend(_all_widgets(k))
    return out


GROUPS = {
    "pin": t_pin, "flicker": t_flicker, "fs": t_fs, "status": t_status,
    "ghost": t_ghost, "truth": t_truth, "spin": t_spin, "wait": t_wait,
    "pages": t_pages, "tip": t_tip, "flash": t_flash, "follow": t_follow,
    "wallpaper": t_wallpaper, "attach": t_attach, "ctxwin": t_ctxwin,
    "layout": t_layout, "picker": t_picker,
}


def main(argv):
    names = [a for a in argv if a in GROUPS] or list(GROUPS)
    for n in names:
        print("\n=== %s ===" % n)
        try:
            GROUPS[n]()
        except Exception as e:
            import traceback
            traceback.print_exc()
            check("%s 抛异常：%s" % (n, e), False)
    print()
    if ok:
        print("=== 全部通过（%d 组）===" % len(names))
        return 0
    print("=== 有失败项：%s ===" % ", ".join(FAILED))
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
