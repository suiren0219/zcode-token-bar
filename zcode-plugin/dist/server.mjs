#!/usr/bin/env node
// Token Bar 控制面板的 MCP server：两个工具 + 一个 UI 资源。
// 零依赖手写 JSON-RPC——协议子集只需要 initialize / tools/* / resources/*，
// 引整包 SDK（express/hono/ajv…）反而是负担。
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn, execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const PLUGIN_ROOT = path.resolve(HERE, "..");
// 悬浮条项目目录（安装时如果搬家了，可以用环境变量覆盖）
const BAR_DIR = process.env.TOKEN_BAR_DIR
  || path.join(os.homedir(), ".zcode", "workspace", "default", "zcode-token-bar");
const BAR = path.join(BAR_DIR, "hoverbar.py");
const LOCK = path.join(os.tmpdir(), "zusage-hoverbar.lock");
let protocolVersion = "2025-06-18";

const SERVER_INFO = { name: "zcode-token-bar-control", version: "0.1.0" };

// ---------- 条面进程操作 ----------
function readLock() {
  try { return fs.readFileSync(LOCK, "utf8").trim(); } catch { return null; }
}
function pidAlive(pid) {
  // 只查存在性；命令行核对在 python 侧做，这里尽力而为
  try { process.kill(Number(pid), 0); return true; } catch { return false; }
}
function findBarPid() {
  // 锁文件只在 HoverBar.run() 里写；--wait-zcode 待命期间（ZCode 没开、
  // 或刚启动还没建窗口）进程活着但没锁。这时按命令行找，别误报"未运行"。
  try {
    const out = execFileSync("powershell", ["-NoProfile", "-Command",
      "Get-CimInstance Win32_Process -Filter \"Name='pythonw.exe' OR Name='python.exe'\" | " +
      "Where-Object CommandLine -like '*hoverbar.py*' | Select-Object -First 1 -ExpandProperty ProcessId"],
      { encoding: "utf8", windowsHide: true, timeout: 10000 });
    const n = Number(String(out).trim());
    return Number.isInteger(n) && n > 0 ? n : 0;
  } catch {
    return 0;
  }
}
function barStatus() {
  const pid = readLock();
  if (!pid) {
    const found = findBarPid();
    return found
      ? { running: true, stale: false, pid: found, phase: "starting" }
      : { running: false, stale: false };
  }
  const alive = pidAlive(pid);
  return alive
    ? { running: true, stale: false, pid: Number(pid) }
    : { running: false, stale: true, pid: Number(pid) };
}
async function startBar() {
  if (!fs.existsSync(BAR)) throw new Error("hoverbar.py not found: " + BAR);
  // pythonw 优先：注册表自启项里记的解释器 → 常见安装位置 → PATH
  let py = "pythonw";
  try {
    const { execFileSync } = await import("node:child_process");
    const reg = execFileSync("reg", ["query",
      "HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
      "/v", "ZCodeTokenBar"], { encoding: "utf8", windowsHide: true, timeout: 8000 });
    const m = reg.match(/"([^"]*pythonw?\.exe)"/i);
    if (m) py = m[1];
  } catch {}
  if (py === "pythonw") {
    const home = os.homedir();
    const cands = [
      path.join(home, "AppData", "Local", "Programs", "Python", "Python311", "pythonw.exe"),
      path.join(home, "AppData", "Local", "Programs", "Python", "Python312", "pythonw.exe"),
      path.join(home, "AppData", "Local", "Programs", "Python", "Python313", "pythonw.exe"),
    ];
    const hit = cands.find((c) => fs.existsSync(c));
    if (hit) py = hit;
  }
  const child = spawn(py, [BAR, "--wait-zcode"], {
    cwd: BAR_DIR, detached: true, stdio: "ignore",
    windowsHide: true,
  });
  child.unref();
  return { started: true, pid: child.pid };
}
async function restartBar() {
  const st = barStatus();
  if (st.running) {
    // 活着的条面不需要重启，但温和提醒走 --quit 再起也行；这里直接报告
    return { action: "already-running", ...st };
  }
  if (st.stale) {
    try { fs.unlinkSync(LOCK); } catch {}
  }
  return { action: "started", ...(await startBar()) };
}

// ---------- 工具与资源 ----------
const TOOLS = [
  {
    name: "bar_status",
    description: "查询 ZCode Token 悬浮条是否在运行（含残留锁检测）",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
  },
  {
    name: "bar_restart",
    description: "启动/重启 ZCode Token 悬浮条（残留锁自动清理，等待 ZCode 窗口出现后显示）",
    inputSchema: { type: "object", properties: {}, additionalProperties: false },
  },
];

const PANEL_HTML = fs.readFileSync(path.join(HERE, "panel.html"), "utf8");

function toolResult(obj) {
  return { content: [{ type: "text", text: JSON.stringify(obj) }], structuredContent: obj };
}

async function handleTool(name) {
  switch (name) {
    case "bar_status": return toolResult(barStatus());
    case "bar_restart": return toolResult(await restartBar());
    default: throw new Error("Unknown tool: " + name);
  }
}

// ---------- JSON-RPC 派发 ----------
function reply(id, result) {
  return { jsonrpc: "2.0", id, result };
}
function replyErr(id, code, message) {
  return { jsonrpc: "2.0", id, error: { code, message } };
}

async function dispatch(msg) {
  const { id, method, params } = msg;
  if (id === undefined) return null;    // 通知：不回
  switch (method) {
    case "initialize":
      if (params?.protocolVersion) protocolVersion = String(params.protocolVersion);
      return reply(id, {
        protocolVersion: params?.protocolVersion || protocolVersion,
        capabilities: { tools: { listChanged: false }, resources: {} },
        serverInfo: SERVER_INFO,
      });
    case "tools/list":
      return reply(id, { tools: TOOLS });
    case "tools/call":
      try {
        const r = await handleTool(params?.name || "");
        return reply(id, r);
      } catch (e) {
        return reply(id, { content: [{ type: "text", text: String(e.message || e) }], isError: true });
      }
    case "resources/list":
      return reply(id, { resources: [{
        uri: "ui://zcode-token-bar-control/panel.html",
        name: "panel", mimeType: "text/html;profile=mcp-app",
      }]});
    case "resources/read": {
      const uri = params?.uri || "";
      if (uri.endsWith("panel.html"))
        return reply(id, { contents: [{ uri, mimeType: "text/html;profile=mcp-app", text: PANEL_HTML }] });
      return replyErr(id, -32602, "Unknown resource: " + uri);
    }
    case "ping":
      return reply(id, {});
    default:
      return replyErr(id, -32601, "Method not found: " + method);
  }
}

// ---------- stdio 主循环（按行分隔 JSON-RPC）----------
let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", async (chunk) => {
  buf += chunk;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    try {
      const resp = await dispatch(msg);
      if (resp) process.stdout.write(JSON.stringify(resp) + "\n");
    } catch (e) {
      if (msg.id !== undefined)
        process.stdout.write(JSON.stringify(replyErr(msg.id, -32603, String(e.message || e))) + "\n");
    }
  }
});
process.stdin.on("end", () => process.exit(0));
