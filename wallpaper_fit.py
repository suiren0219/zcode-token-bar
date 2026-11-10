"""壁纸铺法覆盖层：让 zcode-beautify 的壁纸真正跟着窗口缩放。

背景（现场诊断，2026-11-10）：

    getComputedStyle(#zcode-beautify-wallpaper).backgroundSize == "contain"
    图片 1216x832（1.462），窗口 1564x851（1.838）→ 左右大片留黑
    插件源码里 addEventListener('resize') / ResizeObserver 均为 0
    fit 合法值仅 cover / contain / smart，没有「拉伸铺满」

插件自己的 CSS 是 `position:fixed; inset:0` + `background-size`，理论上
容器跟随视口，但 contain 会留边、cover 会裁掉一截，而且插件在窗口大小
变化后不重算焦点/位置。更重要的是它没有 stretch（100% 100%）这个选项。

做法：通过 CDP 在渲染进程里挂一个我们自己的 <style>（id 固定），用
!important 覆盖 background-size / background-position，并装一个 resize
监听：窗口一变就按当前窗口比例重算，让「自适应」真的自适应。实测插件的
`colors` 重注入只改它自己的 #zcode-beautify-style，不会删我们的节点，
所以覆盖是持久的；ZCode 重启后需要重新调用一次 install()。

零第三方依赖：CDP 的 WebSocket 握手与帧收发都用手写 socket 实现。
"""
from __future__ import annotations

import base64
import hashlib
import json
import os
import socket
import struct
import urllib.request

STYLE_ID = "zcode-token-bar-wpfit"
HOOK_KEY = "__zcodeTokenBarWpFit"
DEFAULT_PORT = 9222

# 三种铺法。stretch 是插件本身没有的第四种。
FITS = ("cover", "contain", "stretch")
FIT_LABELS = {
    "cover": "覆盖（铺满裁切）",
    "contain": "适应（完整不裁）",
    "stretch": "拉伸（强行铺满）",
}


# ---------------------------------------------------------------- CDP WebSocket
class _WS:
    """只实现 CDP 需要的子集：文本帧收发、客户端掩码、ping/pong。"""

    def __init__(self, url: str, timeout: float = 8.0):
        rest = url.split("://", 1)[1]
        hostport, path = rest.split("/", 1)
        host, port = hostport.split(":")
        self.sock = socket.create_connection((host, int(port)), timeout=timeout)
        self.sock.settimeout(timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            "GET /%s HTTP/1.1\r\nHost: %s\r\nUpgrade: websocket\r\n"
            "Connection: Upgrade\r\nSec-WebSocket-Key: %s\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n" % (path, hostport, key)
        )
        self.sock.sendall(req.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.sock.recv(4096)
        if b" 101 " not in buf.split(b"\r\n", 1)[0]:
            raise RuntimeError("WebSocket 握手失败: %s" % buf[:120])
        self._rest = buf.split(b"\r\n\r\n", 1)[1]

    def _recv_exact(self, n: int) -> bytes:
        out = self._rest[:n]
        self._rest = self._rest[n:]
        while len(out) < n:
            chunk = self.sock.recv(n - len(out))
            if not chunk:
                raise RuntimeError("WebSocket 断开")
            out += chunk
        return out

    def send(self, text: str):
        data = text.encode("utf-8")
        mask = os.urandom(4)
        n = len(data)
        if n < 126:
            head = struct.pack("!BB", 0x81, 0x80 | n)
        elif n < 65536:
            head = struct.pack("!BBH", 0x81, 0x80 | 126, n)
        else:
            head = struct.pack("!BBQ", 0x81, 0x80 | 127, n)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        self.sock.sendall(head + mask + masked)

    def recv(self) -> str:
        while True:
            a, b = self._recv_exact(2)
            opcode, ln = a & 0x0F, b & 0x7F
            if ln == 126:
                ln = struct.unpack("!H", self._recv_exact(2))[0]
            elif ln == 127:
                ln = struct.unpack("!Q", self._recv_exact(8))[0]
            if b & 0x80:
                mask = self._recv_exact(4)
                data = bytes(c ^ mask[i % 4]
                             for i, c in enumerate(self._recv_exact(ln)))
            else:
                data = self._recv_exact(ln)
            if opcode == 0x1:
                return data.decode("utf-8", "replace")
            if opcode == 0x8:
                raise RuntimeError("WebSocket 关闭")
            # ping / pong / 分片：这个模块不用，忽略即可

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def _renderer_ws(port: int, timeout: float = 8.0) -> _WS:
    targets = json.load(urllib.request.urlopen(
        "http://127.0.0.1:%d/json/list" % port, timeout=3))
    page = next((t for t in targets if t.get("type") == "page"), None)
    if page is None:
        raise RuntimeError("CDP 里没有 page 目标")
    return _WS(page["webSocketDebuggerUrl"], timeout)


def cdp_eval(port: int, expression: str, timeout: float = 12.0):
    """在 ZCode 渲染进程里执行一段 JS，返回 returnByValue 的值。"""
    ws = _renderer_ws(port, timeout)
    try:
        ws.send(json.dumps({
            "id": 1, "method": "Runtime.evaluate",
            "params": {"expression": expression, "returnByValue": True,
                       "awaitPromise": True},
        }))
        while True:
            msg = json.loads(ws.recv())
            if msg.get("id") != 1:
                continue
            res = msg.get("result", {})
            if "exceptionDetails" in res:
                raise RuntimeError(res["exceptionDetails"].get("text", "JS 异常"))
            return res.get("result", {}).get("value")
    finally:
        ws.close()


# ---------------------------------------------------------------- 覆盖 CSS
def _css(fit: str) -> str:
    """插件不认 stretch，所以把它翻译成 100% 100%；cover/contain 也
    一并在这里覆盖，保证我们这层的优先级最高。"""
    size = {"cover": "cover", "contain": "contain",
            "stretch": "100% 100%"}.get(fit, "cover")
    pos = "50% 50%" if size == "contain" else "center"
    return (
        "#zcode-beautify-wallpaper{"
        "background-size:%s !important;"
        "background-position:%s !important;"
        "background-repeat:no-repeat !important;"
        "background-attachment:fixed !important;"
        "}" % (size, pos)
    )


def _install_js(fit: str) -> str:
    """生成幂等的安装脚本：建/更新 style 节点 + 装 resize 监听。

    resize 监听只做一件事：按新窗口尺寸重算并写回 CSS。CSS 的
    background-size 用百分比/关键字本来就会跟着容器（inset:0）走，
    但插件在窗口变化后不重算焦点，cover 的位置会漂；这里统一复位，
    同时给 data-* 属性留个现场标记，方便诊断。
    """
    css = _css(fit)
    return """(() => {
  const ID = %s, KEY = %s, CSS = %s, FIT = %s;
  let st = document.getElementById(ID);
  if (!st) {
    st = document.createElement('style');
    st.id = ID;
    document.head.appendChild(st);
  }
  st.textContent = CSS;
  // 铺法与回调都放全局：钩子只装一次，但每次 install() 都替换回调，
  // 否则窗口里可能还挂着更早版本脚本的旧闭包，把 data-wpfit 写回旧值。
  window[KEY + 'Fit'] = FIT;
  window[KEY + 'Apply'] = () => {
    const el = document.getElementById('zcode-beautify-wallpaper');
    if (!el) return;
    el.setAttribute('data-vp', innerWidth + 'x' + innerHeight);
    el.style.backgroundRepeat = 'no-repeat';
    // 触发一次重排，确保缩放后浏览器立刻用新视口重新绘制背景
    el.style.transform = el.style.transform || 'translateZ(0)';
  };
  const fire = () => { if (window[KEY + 'Apply']) window[KEY + 'Apply'](); };
  if (!window[KEY]) {
    window[KEY] = true;
    addEventListener('resize', fire, { passive: true });
    // 侧栏折叠、最大化这类不改 window 尺寸但改布局的情况
    if (typeof ResizeObserver !== 'undefined') {
      try {
        new ResizeObserver(fire).observe(document.documentElement);
      } catch (e) {}
    }
  }
  fire();
  return JSON.stringify({
    ok: true,
    style: !!document.getElementById(ID),
    fit: FIT,
    vp: [innerWidth, innerHeight],
    computed: getComputedStyle(
      document.getElementById('zcode-beautify-wallpaper') || document.body
    ).backgroundSize
  });
})()""" % (json.dumps(STYLE_ID), json.dumps(HOOK_KEY),
           json.dumps(css), json.dumps(fit))


def install(port: int | None = None, fit: str = "cover") -> dict:
    """把铺法覆盖层装进 ZCode。返回诊断字典；失败抛异常由调用方兜。"""
    if fit not in FITS:
        fit = "cover"
    return json.loads(cdp_eval(port or DEFAULT_PORT, _install_js(fit)))


def current(port: int | None = None) -> dict:
    """读现场：插件自己的值、我们的覆盖值、窗口尺寸、resize 钩子在不在。"""
    expr = """(() => {
      const el = document.getElementById('zcode-beautify-wallpaper');
      const plugin = document.getElementById('zcode-beautify-style');
      const own = document.getElementById(%s);
      const cs = el ? getComputedStyle(el) : null;
      return JSON.stringify({
        hasWallpaper: !!el,
        computedSize: cs ? cs.backgroundSize : null,
        computedPos: cs ? cs.backgroundPosition : null,
        pluginSize: plugin ? (/background-size:\\s*([^;]+)/.exec(plugin.textContent)||[])[1] : null,
        ourStyle: own ? own.textContent.slice(0, 160) : null,
        ourFit: window[%s] || null,
        hook: !!window[%s],
        vp: [innerWidth, innerHeight],
        dpr: devicePixelRatio
      });
    })()""" % (json.dumps(STYLE_ID), json.dumps(HOOK_KEY + "Fit"),
               json.dumps(HOOK_KEY))
    return json.loads(cdp_eval(port or DEFAULT_PORT, expr))


def uninstall(port: int | None = None) -> bool:
    expr = """(() => {
      const s = document.getElementById(%s);
      if (s) s.remove();
      const el = document.getElementById('zcode-beautify-wallpaper');
      if (el) { el.removeAttribute('data-wpfit'); el.removeAttribute('data-vp'); }
      return !document.getElementById(%s);
    })()""" % (json.dumps(STYLE_ID), json.dumps(STYLE_ID))
    return bool(cdp_eval(port or DEFAULT_PORT, expr))
