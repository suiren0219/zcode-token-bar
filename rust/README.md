# zcode-token-bar-rs

ZCode token 用量悬浮条的 **Rust 重写版**。Python 原版（同目录 `../zcode-token-bar`）功能对齐目标，本项目是全部功能的 Rust 实现。

## 为什么重写

- **单 exe**：`cargo build --release` 出一个 ~5MB 可执行文件，不依赖 Python
- **快**：启动 <50ms，常驻内存 ~10MB（Python 版 ~50MB）
- **稳**：无 GBK/UTF-8 编码坑；崩溃面收窄到 unsafe 边界（就 win.rs 一处）

## 进度（对照 Python 版模块）

| 模块 | Python 版 | Rust 版 | 状态 |
|---|---|---|---|
| 数据层（只读 db.sqlite，快照口径） | zusage.py | src/data.rs | ✅ 当前会话/今日/本轮/tps/上下文/缓存率/工具 |
| 条面（胶囊、贴靠、属主绑定） | hoverbar.py | src/bar.slint + src/win.rs | ✅ 基础形态 |
| 单实例 / --quit / --once | hoverbar.py | src/main.rs + src/win.rs | ✅ |
| 设置面板（8 页） | hoverbar.py | 待做 | ⏳ 下个会话 |
| 钉住 / 六向贴靠 / 拖动 | hoverbar.py | 待做 | ⏳ |
| 模型发现/导入/探针（热写入） | model_discovery.py | 待做 | ⏳ |
| 壁纸 / Monet 取色 / CDP | wallpaper_fit.py | 待做 | ⏳ |
| 自启动 / 更新检查 / 悬停详情 | hoverbar.py | 待做 | ⏳ |

## 环境（这台机器）

- 工具链：`x86_64-pc-windows-gnu`（rustup）+ **llvm-mingw**（`D:\rust\llvm-mingw\llvm-mingw-20261020-ucrt-x86_64`）
- llvm-mingw 没有 libgcc/libgcc_eh，rustc GNU target 硬编码要它们 → 本仓库 `mingw-stub/` 放了别名库（builtins→libgcc，libunwind→libgcc_eh），`.cargo/config.toml` 已配 `-L native`。**编译前把 llvm-mingw 的 bin 加入 PATH**：

```bash
export PATH="/d/rust/llvm-mingw/llvm-mingw-20261020-ucrt-x86_64/bin:$PATH"
cargo build --release
```

## 命令

```bash
target/release/zcode-token-bar-rs.exe            # 启动条面
target/release/zcode-token-bar-rs.exe --once     # 终端打印一次快照
target/release/zcode-token-bar-rs.exe --quit     # 退出运行中的实例
```

数据根与 Python 版共用 `D:\zcode-data\token-bar-data`（db 只读共享；Rust 版日志在 `logs/hoverbar-rs.log.*`，单实例 mutex 名 `Local\zcode-token-bar-rs`——与 Python 版互不干扰，可同时运行对比）。

## 架构

```
src/main.rs    装配：参数解析、单实例、日志、数据泵线程、贴靠定时器
src/data.rs    数据层：SQL 口径逐条对照 Python zusage.py（当前会话判定/
               session_usage/turn_usage 回退/tps/上下文档位推断/ctx 超限告警）
src/win.rs     Win32：EnumWindows 找 ZCode、贴靠计算、WS_EX_TOOLWINDOW+
               NOACTIVATE+TOPMOST、属主绑定 GWLP_HWNDPARENT、命名互斥量/事件
src/bar.slint  条面 UI：胶囊、三档配色函数、上下文微进度条；无业务逻辑，
               全部数据由 Rust 侧拼好传属性
```

学习提示：每个文件头部注释都标注了 Python 对应物与 Rust 概念（Connection 的 Send/!Sync、
unsafe 边界、invoke_from_event_loop 的跨线程 UI 更新、命名内核对象）。
