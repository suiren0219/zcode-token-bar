# ZCode Token · Rust 原生版

独立 Windows 悬浮条与模型管理程序。运行时不需要 Python、Node、Git 或 Slint SDK，不修改 ZCode 安装文件。SQLite 连接始终只读。

## 使用

启动 `zcode-token-bar.exe` 后，右键托盘图标或点击条面齿轮打开设置。关闭设置窗口不会退出程序；在托盘菜单选择“退出悬浮条”结束运行。

- **位置**：六向贴靠、窗口/屏幕基准、偏移、自由拖动与位置恢复；钉住会话同时固定位置，另有独立位置锁。
- **外观与显示**：字体、字号、不透明度、指标标签及七项指标开关。普通设置先预览，“应用”保存，“关闭”撤销未保存修改。
- **壁纸**：原生选图、路径粘贴、拖放、预览、压暗、模糊、Monet 动态色、cover/contain/smart/stretch 和焦点。图片复制到数据目录保存；连接断开时保留设置并重试。设置窗口也可使用同一张壁纸，带独立背景开关、压暗遮罩和可读的控件底板。恢复主题只清理本程序及已识别旧壁纸插件的样式。
- **行为**：置顶、ZCode 最小化/关闭时收起、刷新间隔、自启动、桌面快捷方式、程序重启。
- **更新**：检查 GitHub 发布版本，明确区分源码模式与发行版。自动检查不会自动安装。没有匹配的 Rust 发行资产时显示实际情况，并提供发布页。
- **状态**：数据、窗口、自启动等真实诊断；日志目录与 SSH 远端配置。
- **模型**：全部/单提供方发现、分页、有界结果、筛选、选择、复制 ID、能力与来源详情、手填导入、文本/工具/图片/PDF 实测、进度与取消。提供方选择按稳定 ID 绑定。
- **小亚**：原生连接文件选择、提供方/模型选择、问候、消息、主动陪伴、暂停、取消、流式显示与可选回复历史。默认关闭；自动上下文仅包含轮次终态等最小元数据。
- **详情与报表**：条面悬停摘要、完整会话详情、近 7/30 天用量及模型统计。

### 模型导入与探针

发现不发送对话生成请求。实测会调用你选择的提供方，可能产生费用；界面会在执行前确认，最多处理一批 12 个模型。取消后不会继续发新请求或自动重试，但已到达服务端的请求无法撤回。

导入前备份，原子替换配置，复核提供方身份和并发变化，并延迟读回确认。**写入并读回成功不等于客户端已接受配置**；界面不会把磁盘验证冒充运行时确认。已有模型不覆盖，未知能力保持未知。多文件部分成功会分别报告。

### 壁纸连接

ZCode 需要开启本机调试端口（默认 `9222`）。壁纸页可检查连接、修复已识别的 ZCode 快捷方式；修复前备份快捷方式，保留其它参数。设置页本身默认使用不透明背景。

壁纸引擎随悬浮条常驻。壁纸页的维护区域提供“ZCode 更新后自动恢复”和“立即恢复自定义外观”。后台检测客户端 `app.asar` 指纹、安装路径及渲染窗口变化，将变更前后指纹和自己的配置保存为恢复记录，再通过原生壁纸引擎恢复外观。不会把旧版整个 `app.asar` 覆盖到新版客户端。

若 ZCode 本次启动没有开放端口，保留待恢复配置，在下一次通过修复后的入口启动 ZCode 时重试。恢复只有在应用及健康检查均成功后才显示完成。程序不会自行关闭或重启 ZCode。

## 数据与迁移

数据目录优先采用 `ZCODE_TOKEN_BAR_DATA` / `TOKEN_BAR_DATA_DIR`，其次读取程序祖先目录的 `data-dir.txt`、便携标记和具有应用文件证据的旧数据目录，最后使用 `%LOCALAPPDATA%/ZCodeTokenBar`。

- `settings-rs.json`：普通偏好与连接文件路径，不存 API 密钥。
- `wallpapers/`：按内容哈希保存的图片。
- `logs/`：程序日志。
- `backups/`、模型配置备份目录：迁移和写入前备份。
- `model-discovery-cache.json`：模型 ID 与能力缓存，不保存凭据。

已有 Rust 设置优先，只补旧版缺少的字段。可在位置页显式恢复旧 Python 设置；旧文件保留。自启动仅接管经完整路径识别的本应用注册项，拒绝覆盖未知同名项。

ZCode 数据库、模型配置和壁纸旧插件配置各有独立路径规则，不能简单认为都在悬浮条数据目录。模型模块识别活动数据目录及环境变量覆盖，读取首选文件失败时不会偷偷换成旧凭据。

## 命令行

```text
zcode-token-bar.exe --settings
zcode-token-bar.exe --wallpaper
zcode-token-bar.exe --companion
zcode-token-bar.exe --details
zcode-token-bar.exe --wait-zcode
zcode-token-bar.exe --quit
zcode-token-bar.exe --once
zcode-token-bar.exe --doctor
zcode-token-bar.exe --discover <provider-id>
zcode-token-bar.exe --check-update

zcode-token-bar.exe now
zcode-token-bar.exe today
zcode-token-bar.exe days 7
zcode-token-bar.exe sessions 20
zcode-token-bar.exe models 7
zcode-token-bar.exe workspace <目录关键词>
zcode-token-bar.exe session <SID或唯一前缀>
zcode-token-bar.exe watch 5
zcode-token-bar.exe --data-json [SID]
zcode-token-bar.exe --data-serve
```

诊断和数据命令可在 GUI 已运行时使用。`watch` 支持 Ctrl+C 与关闭输出管道退出。

原生 MCP 控制入口：`--mcp`。保留 `bar_status`、`bar_restart`、`bar_doctor` 和原控制面板资源。`bar_restart` 只确保运行，不强制重启。`--install-plugin` 只生成本地插件包，不自动注册到 ZCode。

## SSH 远端

`helper/` 是不依赖 GUI 的 Rust 数据辅助程序，可在目标机器上单独构建。它复用父目录的 `src/data*.rs`；需保留完整的 `helper/`（包括 `Cargo.lock`）和同级 `src/` 目录结构，并从本 README 所在目录执行：

```bash
cargo build --release --manifest-path helper/Cargo.toml
```

在目标机器放置 `zcode-token-data`，状态页配置主机、用户、端口、私钥文件路径与辅助程序路径。SSH 使用现有密钥认证，不写入私钥内容；超时保持钉住状态。自动跟随优先本地，SQLite 本身无法确认客户端当前打开的远端标签页。

Linux 构建需要在目标系统另行验证；不要把 Windows GUI exe 当作 Linux 辅助程序。

## 构建与测试

推荐 Windows MSVC Rust 工具链与 C/C++ Build Tools：

```bash
cargo build --release
cargo test --release -- --test-threads=1
```

Windows GNU + LLVM-MinGW 使用 [`scripts/build-llvm-mingw.ps1`](scripts/build-llvm-mingw.ps1)。先选择 `x86_64-pc-windows-gnu` Rust 工具链，再从本 README 所在目录在 PowerShell 中执行；将示例工具链目录替换为本机 LLVM-MinGW 根目录（其中应有 `bin/`）：

```powershell
.\scripts\build-llvm-mingw.ps1 -ToolchainRoot 'C:\toolchains\llvm-mingw' -Action build
.\scripts\build-llvm-mingw.ps1 -ToolchainRoot 'C:\toolchains\llvm-mingw' -Action test
.\scripts\build-llvm-mingw.ps1 -ToolchainRoot 'C:\toolchains\llvm-mingw' -Action check
```

`ToolchainRoot` 为必填参数；`Action` 默认为 `build`，分别执行 release 构建、release 单线程测试或 `cargo check`。脚本使用当前选定的 Rust 工具链，在 `target/llvm-mingw-compat/` 自动生成 compiler-rt/libunwind 的库名兼容副本，并临时设置编译环境，结束后恢复。无需手工复制兼容库或设置 `RUSTFLAGS`；生成文件不纳入版本控制。MSVC 构建使用上方命令，无需此脚本。

Slint 使用 Femtovg 图形合成、原生无障碍接口和 Win32 辅助能力。设置窗背景和暗色遮罩由同一个图形后端合成，避免软件局部重绘与外部分层窗口样式冲突。资源及透明图标编译进 exe。体积、启动速度和内存占用以实际构建和运行测量为准。

## 源码导航

| 文件 | 职责 |
|---|---|
| `main.rs` / `app.rs` | 命令分发、事件循环、后台任务与 UI 装配 |
| `settings.rs` / `storage.rs` | 设置迁移、保存与位置计算 |
| `bar.slint` / `settings.slint` / `ui-theme.slint` | 条面、设置页、详情与统一视觉样式 |
| `data*.rs` | 只读统计、上下文规则、远端与文本命令 |
| `models*.rs` / `model_ui.rs` | 提供方解析、发现、能力、探针、导入事务与模型页 |
| `wallpaper.rs` / `wallpaper_runtime.js` / `recovery.rs` | 本机 CDP、图片、Monet、客户端更新检测与外观恢复 |
| `companion*.rs` | 陪伴协议、轮次观察与取消调度 |
| `system.rs` / `update.rs` | 系统对话框、自启、快捷方式、更新与原生 helper |
| `control.rs` | MCP stdio、控制面板及受限配置入口 |
| `win.rs` | 窗口句柄、物理坐标、剪贴板与单实例信号 |
