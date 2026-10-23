#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""悬浮条开机自启管理（只写 HKCU，不需要管理员权限）

用法：
    python autostart.py on       # 开机自启
    python autostart.py off      # 取消
    python autostart.py status   # 看当前状态

为什么不放进「启动」文件夹：那里躺着 .lnk，快捷方式目标一旦被移动
（换 Python、搬目录）就静默失效，而且不好排查。注册表 Run 项里是明文
命令行，`status` 能直接把将要执行的命令打出来，坏了看得见。

用 pythonw.exe 而不是 python.exe：后者会闪一个控制台窗口。找不到
pythonw.exe 时退回 python.exe（功能一样，只是有窗）。
"""
from __future__ import annotations

import subprocess
import sys
import winreg
from pathlib import Path

RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"
NAME = "ZCodeTokenBar"
HERE = Path(__file__).parent.resolve()
BAR = HERE / "hoverbar.py"


def _pythonw() -> Path:
    """优先 pythonw.exe；没有就跟当前解释器同目录找，再不行退回 python.exe。"""
    exe = Path(sys.executable)
    cand = exe.with_name("pythonw.exe")
    return cand if cand.is_file() else exe


def command() -> str:
    """将要写入 Run 项的命令行（带引号，路径含空格也能跑）。"""
    return '"%s" "%s"' % (_pythonw(), BAR)


def read() -> str | None:
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY) as k:
            return winreg.QueryValueEx(k, NAME)[0]
    except FileNotFoundError:
        return None
    except OSError:
        return None


def enable() -> int:
    if not BAR.is_file():
        print("找不到 hoverbar.py：", BAR)
        return 1
    cmd = command()
    with winreg.CreateKey(winreg.HKEY_CURRENT_USER, RUN_KEY) as k:
        winreg.SetValueEx(k, NAME, 0, winreg.REG_SZ, cmd)
    print("已开启开机自启")
    print("  ", cmd)
    if "pythonw" not in cmd.lower():
        print("   注意：没找到 pythonw.exe，用的是 python.exe，启动时会闪一下控制台。")
    return 0


def disable() -> int:
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY, 0,
                            winreg.KEY_SET_VALUE) as k:
            winreg.DeleteValue(k, NAME)
        print("已取消开机自启")
    except FileNotFoundError:
        print("本来就没开")
    except OSError as e:
        print("取消失败：", e)
        return 1
    return 0


def status() -> int:
    cmd = read()
    if cmd:
        print("开机自启：开")
        print("  ", cmd)
        # 路径可能已经失效（Python 换了版本、目录搬了），顺手验一下
        try:
            target = Path(cmd.split('"')[1])
            ok = target.is_file()
        except (IndexError, OSError):
            ok = False
        if not ok:
            print("   警告：上面这个解释器路径已不存在，自启会静默失败。重跑 on 即可修好。")
            return 1
        return 0
    print("开机自启：关")
    return 0


def main(argv) -> int:
    cmd = (argv[0] if argv else "status").lower()
    if cmd == "on":
        return enable()
    if cmd == "off":
        return disable()
    if cmd == "status":
        return status()
    print(__doc__.strip())
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
