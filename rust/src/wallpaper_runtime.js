// 仅由 Rust 通过本机 CDP 调用；不访问网络、不保存图片到 localStorage。
(async function (p) {
  "use strict";
  const KEY = "__zcodeTokenBarWallpaper";
  const STYLE = "zcode-token-bar-wallpaper-style";
  const WP = "zcode-beautify-wallpaper";
  const BP = "zcode-beautify-backdrop";
  const LEGACY = ["zcode-beautify-style", "zcode-token-bar-wpfit"];
  const byId = id => document.getElementById(id);
  const clamp = x => Math.max(0, Math.min(1, x));
  const local = location.protocol === "file:" || location.protocol === "app:" ||
    location.protocol === "zcode:" ||
    (/^https?:$/.test(location.protocol) &&
      /^(localhost|127(?:\.\d{1,3}){3}|\[::1\])$/.test(location.hostname));
  if (!local || !(location.pathname.endsWith("/out/renderer/index.html") || document.title === "ZCode")) {
    throw new Error("Not a ZCode renderer");
  }

  function hasLegacy() {
    return LEGACY.some(id => !!byId(id));
  }
  function status() {
    const s = window[KEY];
    const st = byId(STYLE), wp = byId(WP), bp = byId(BP);
    if (!p.enabled) {
      return { healthy: !s && !st && !wp && !bp && !hasLegacy() };
    }
    const valid = s && s.version === p.version && s.fingerprint === p.fingerprint &&
      s.active && s.resizeInstalled && s.imageHash === p.imageHash &&
      (!window.ResizeObserver || !!s.observer) && !hasLegacy() &&
      st && st === s.style && st.isConnected && st.textContent === s.css && !st.disabled &&
      (p.visible
        ? wp && bp && wp === s.wallpaper && bp === s.backdrop &&
          wp.isConnected && bp.isConnected && wp.style.backgroundImage === s.imageCss &&
          bp.style.backgroundImage === s.imageCss && wp.dataset.fingerprint === p.fingerprint &&
          getComputedStyle(wp).display !== "none" &&
          getComputedStyle(wp).backgroundSize === s.resolvedFit &&
          wp.style.backgroundPosition === s.position &&
          Math.abs(parseFloat(getComputedStyle(wp).opacity) - s.opacity) < 0.001 &&
          (bp.dataset.on === "1") === (s.resolvedFit === "contain")
        : !wp && !bp);
    return { healthy: !!valid };
  }
  function clearLegacy() {
    for (const id of LEGACY) byId(id)?.remove();
    // Python 旧监听没有保存函数引用；清空它调用的回调即可让旧监听失效。
    delete window.__zcodeTokenBarWpFitApply;
    delete window.__zcodeTokenBarWpFitFit;
    if (window.__zcodeBeautify) window.__zcodeBeautify.cssText = null;
    try {
      localStorage.removeItem("zcode-beautify:css");
      localStorage.removeItem("zcode-beautify:wallpaper");
    } catch (_) {}
  }
  function clear() {
    const old = window[KEY];
    if (old && typeof old.dispose === "function") old.dispose();
    delete window[KEY];
    for (const id of [STYLE, WP, BP]) byId(id)?.remove();
    clearLegacy();
  }
  if (p.op === "health") return status();
  if (p.op === "reset") {
    const reused = status().healthy;
    clear();
    return { ok: status().healthy, reused };
  }
  if (p.op !== "apply") throw new Error("Unsupported wallpaper operation");
  if (status().healthy) return { ok: true, reused: true };

  // 先确认图像可解码，失败时保留原来的壁纸，不形成半套 DOM。
  if (p.visible) {
    await new Promise((resolve, reject) => {
      const img = new Image();
      const timer = setTimeout(() => reject(new Error("Wallpaper decode timeout")), 6000);
      img.onload = () => { clearTimeout(timer); resolve(); };
      img.onerror = () => { clearTimeout(timer); reject(new Error("Wallpaper decode failed")); };
      img.src = p.dataUri;
    });
  }
  if (!document.documentElement) throw new Error("Renderer not ready");
  clear();
  const st = document.createElement("style");
  st.id = STYLE;
  st.textContent = p.css;
  (document.head || document.documentElement).appendChild(st);
  const s = {
    version: p.version, fingerprint: p.fingerprint, imageHash: p.imageHash,
    css: p.css, style: st, active: true, resizeInstalled: false,
    wallpaper: null, backdrop: null, observer: null, frame: 0,
    imageCss: "", resolvedFit: "", position: "", opacity: p.opacity
  };
  window[KEY] = s;
  if (p.visible) {
    for (const id of [BP, WP]) {
      const el = document.createElement("div");
      el.id = id;
      el.setAttribute("aria-hidden", "true");
      el.dataset.fingerprint = p.fingerprint;
      el.style.backgroundImage = 'url("' + p.dataUri + '")';
      document.documentElement.appendChild(el);
      if (id === WP) s.wallpaper = el;
      else s.backdrop = el;
    }
    s.imageCss = s.wallpaper.style.backgroundImage;
  }

  function update() {
    s.frame = 0;
    if (!s.active || !s.wallpaper) return;
    const w = Math.max(1, innerWidth), h = Math.max(1, innerHeight);
    const imageAspect = p.width / p.height, viewportAspect = w / h;
    const coverage = Math.min(imageAspect / viewportAspect, viewportAspect / imageAspect);
    let fit = p.fit === "smart" ? (coverage < 0.60 ? "contain" : "cover") : p.fit;
    let fx = p.focusX, fy = p.focusY;
    if (p.fit === "smart") {
      // 中位保留自动焦点；滑到两端时可以确实对齐图片边缘。
      const bias = (automatic, manual) => manual < 0.5
        ? automatic * manual * 2 : automatic + (1 - automatic) * (manual - 0.5) * 2;
      fx = bias(p.autoX, fx);
      fy = bias(p.autoY, fy);
    }
    let px = fx, py = fy;
    if (fit === "cover") {
      const scale = Math.max(w / p.width, h / p.height);
      const excessX = p.width * scale - w, excessY = p.height * scale - h;
      px = excessX > 0.0001 ? clamp((p.width * scale * fx - w / 2) / excessX) : 0.5;
      py = excessY > 0.0001 ? clamp((p.height * scale * fy - h / 2) / excessY) : 0.5;
    }
    if (fit === "stretch") { fit = "100% 100%"; px = py = 0.5; }
    s.resolvedFit = fit;
    s.wallpaper.style.backgroundSize = fit;
    s.wallpaper.style.backgroundPosition = (px * 100).toFixed(4) + "% " + (py * 100).toFixed(4) + "%";
    s.position = s.wallpaper.style.backgroundPosition;
    s.wallpaper.style.transform = "scale(" + (1 + p.blur * 6 / Math.min(w, h)) + ") translateZ(0)";
    s.wallpaper.dataset.viewport = w + "x" + h;
    s.backdrop.dataset.on = fit === "contain" ? "1" : "0";
    s.backdrop.style.backgroundPosition = s.position;
    s.backdrop.style.transform = "scale(" + (1 + 168 / Math.min(w, h)) + ") translateZ(0)";
  }
  const schedule = () => {
    if (s.active && !s.frame) s.frame = requestAnimationFrame(update);
  };
  s.dispose = () => {
    s.active = false;
    removeEventListener("resize", schedule);
    if (window.visualViewport) window.visualViewport.removeEventListener("resize", schedule);
    if (s.observer) s.observer.disconnect();
    if (s.frame) cancelAnimationFrame(s.frame);
  };
  addEventListener("resize", schedule, { passive: true });
  if (window.visualViewport) window.visualViewport.addEventListener("resize", schedule, { passive: true });
  s.resizeInstalled = true;
  if (window.ResizeObserver) {
    s.observer = new ResizeObserver(schedule);
    s.observer.observe(document.documentElement);
  }
  update();
  return { ok: status().healthy, reused: false };
})
