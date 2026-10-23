# 使用说明

面向已经跑起来的人：每个设置项改什么、行为细节、出问题怎么查。
安装见 [README](../README.md)。

---

## 1. 三个入口

| 入口 | 位置 | 用来做什么 |
|---|---|---|
| 条面 | ZCode 窗口上方的胶囊 | 看当前会话用量 |
| 右端切角方块 | 条面最右侧 | 单击打开设置面板，悬停翻酸黄色 |
| 设置面板 | 见上 | 位置 / 外观 / 显示项 / 面板与壁纸 / 行为 |

另外三个等价入口：双击条面任意位置、右键菜单「设置…」、
`python hoverbar.py --settings`（走本地端口 47615 通知已运行的那条）。

拖动条面本身是移动位置。指针按在右端方块上时会被识别成按钮，不会把条拖走。

---

## 2. 条面各项

| 项 | 显示内容 |
|---|---|
| 速度 | 最近一次完成请求的输出 tokens ÷ 生成耗时 |
| 上下文 | 微进度条 + 占比 |
| 本轮 | 本轮 tokens |
| 会话 | 当前会话累计 tokens |
| 今日 | 今天所有会话的 tokens |
| 工具 | 当前会话工具调用次数，错误时右侧标红 |
| 缓存率 | 命中缓存的输入 token ÷ 全部输入 token |

颜色：生成速度 ≥70 绿、40–70 黄、<40 红；上下文占比在大窗口（≥100 万）下
40%/60% 预警，其余窗口 70%/85% 预警；缓存率 ≥60% 绿、30–60% 黄、<30% 灰。

**缓存率的分母是 `input_tokens` 本身。** provider 的口径里 `input_tokens` 已经
包含 `cache_read_input_tokens`，所以命中率 = cache_read ÷ input。不要再把两者
相加当分母，那样算出来会正好差一半。

数据来自本地 SQLite 的只读查询，请求完成后才落库，所以数字反映的是
「最近一次已完成请求」。正在生成的请求不计入。

### 悬停看详情

鼠标停在条面上约 0.45 秒，条面下方弹出一块详情浮层：

```
会话    zcode-token-usage-statusbar
状态    生成中
上下文  376.9K / 1M  37.7%
缓存率  49.5%  (读 327M)
本轮    16M  46次  4:01
本轮输入 16M  缓存 49.9%
本轮输出 22.2K
会话累计 661.9M  31轮 1475次
今日    515.2M  1018次
最近请求 deepseek-v4.1-flash  257.8 t/s
首 token 2.7s
重试    1 次
工具    1484 次  ✕18
  · Bash  992 次
  · Edit  220 次
  · Read  198 次
改动    +120 / -30  9 文件
数据源  远端 · <host>
```

只显示有数据的那几行，空字段直接略过。浮层默认在条面正下方，放不下就翻到
上方并夹在屏幕内。鼠标移开、拖动条面或打开设置面板都会收起。

---

## 3. 设置面板逐项说明

### 01 位置

| 项 | 默认 | 说明 |
|---|---|---|
| 贴靠 | 左下 | 六选一：左下 / 下中 / 右下 / 左上 / 上中 / 右上。拖过条面后变成「自定义」。 |
| 范围 | 跟随窗口 | `跟随窗口` = 坐标相对 ZCode 窗口，窗口挪它跟着挪；`整块屏幕` = 贴显示器边缘，不管 ZCode 在哪。 |
| 边距 | 16 / 6 px | 左右 / 上下。贴靠点是窗口或屏幕的边缘，这个值是从边缘往里缩多少。 |

### 02 外观

| 项 | 默认 | 说明 |
|---|---|---|
| 透明度 | 0.92 | 0.30–1.00。拖动时条面**立刻**跟着变（只预览），点「应用」才写进配置。 |
| 字号 | 13 px | 10–22。上下文那根微进度条会按同比例缩放，不会出现字大了条还细的情况。 |

### 03 显示项

七项独立开关，加一个「显示文字标签」总开关。关掉标签只留数字，条面会明显变短；
全关时条面显示「等待数据…」而不是缩成一条线。

### 04 面板与壁纸

| 项 | 默认 | 说明 |
|---|---|---|
| 面板透明度 | 1.00 | 0.55–1.00。调的是**这个设置面板自己**，不是条面（条面透明度在「02 外观」）。拖动即预览，方便一边透出壁纸一边看内容。 |
| 显示壁纸 | 开 | 关掉后 ZCode 回到纯色背景，壁纸相关项一并失效。 |
| 壁纸压暗 | 25 | 0–100。越高壁纸越沉、文字越清楚。ZCode 客户端里设的压暗值也会反映到这里。 |
| 壁纸模糊 | 0 | 0–40 px。界面更糊但更安静。 |
| 换壁纸 | — | 打开文件选择框，选 jpg / png，选完**立刻生效**（自动调插件 CLI 重注入，不用重启 ZCode）。右侧显示当前文件名。 |

这一节读写的是 **zcode-beautify 插件的同一份配置**（
`%APPDATA%\.zcode\cli\plugins\data\zcode-beautify@zcode-beautify\config.json`），
不另存副本，避免两边打架。所以从这里改的效果，和在 ZCode 客户端里改完全等价；
反过来在客户端改了，重新打开面板也会看到新值。

改完点「应用」或选完图片，会自动调用插件自带的 CLI（`colors` 子命令，走 CDP
端口，端口号从插件配置里读，默认 9222）把主题重新注入，**不用重启 ZCode**。
调不通时会弹窗说明「已保存但未能立刻生效」，而不是假装成功——那时重启一次
客户端即可。

插件没装时整节只剩一行提示「没找到 zcode-beautify 插件的数据目录」，不会报错。

### 05 行为

| 项 | 默认 | 说明 |
|---|---|---|
| 总是置顶 | 关 | 勾了就是真正的 topmost，会盖住其它程序（包括全屏播放器）。默认不勾。 |
| ZCode 不在前台时隐藏 | 关 | 勾了之后切到别的窗口就自动收起；属主绑定本身已经会在 ZCode 最小化时让路，一般不用再勾。 |

面板没有系统标题栏（`overrideredirect`），顶部是一条自绘的深色命令栏：
左边三色斜条，中间 `ZCODE TOKEN / BAR / SETTINGS`，右边切角关闭按钮。
拖动这条命令栏可以移动面板。

面板内容比屏幕高时，右侧会出现 3px 酸黄细线滚动指示，滚轮在**任何控件上**
都有效（标签、滑块、勾选框上滚动不会失效）。面板按指针摆放并夹在屏幕内
（留 8px 边距和 48px 底部余量）。

---

## 4. 只浮在 ZCode 上的机制

默认不勾「总是置顶」，靠 Windows 的**属主窗口绑定**：
`SetWindowLongPtrW(条窗口, GWLP_HWNDPARENT, ZCode 主窗口)`。效果是

- 条面始终压在 ZCode 之上，不会被对话区盖住
- ZCode 最小化时条面一起收走，不会孤零零留在桌面上
- 不抢焦点，点条面不会把输入框踢掉
- 任务栏上没有它的条目

这比 `-topmost` 温和得多：切到浏览器看文档时它不会挡在那儿。真需要压住一切，
再去勾「总是置顶」。

找不到 ZCode 窗口时（客户端没开）条面会隐藏而不是乱飘；ZCode 启动后下一次轮询
自动接上。

---

## 5. 取数链路

悬浮条本身不查库，它起一个常驻子进程 `python zusage.py serve`：

```
悬浮条 ──stdin 一行空行──> zusage.py serve ──只读查询──> db.sqlite
       <──stdout 一行 JSON──
```

间隔默认 1500 ms（`hoverbar.json` 的 `poll_ms`）。用常驻进程而不是每次都起
解释器，省 60–100 ms/次，也不会每次闪一个控制台窗口。

`zusage.py` 也单独可用：

```bash
python zusage.py            # 当前会话
python zusage.py json       # 机器可读快照（悬浮条用的就是这个）
python zusage.py days 7     # 近 7 天
python zusage.py sessions 10
python zusage.py models 7
python zusage.py serve      # 行协议，给悬浮条用
```

### SSH 远程会话

桌面客户端连的是远端 zcode-server 时，会话数据落在**远端机**的
`~/.zcode/cli/db/db.sqlite`，本地库永远查不到——条面会显示「不在本地库」。
要让这种会话出数，**两端各准备一半**：

远端机只放一个文件：

```bash
mkdir -p ~/.zcode/zcode-token-usage-statusbar
scp zusage.py <host>:~/.zcode/zcode-token-usage-statusbar/zusage.py
ssh <host> python3 ~/.zcode/zcode-token-usage-statusbar/zusage.py json   # 能打出 JSON 即可
```

本机在 `zusage.py` 同目录建 `config.json`：

```json
{
  "remote": {
    "enabled": true,
    "ssh": "ssh <host>",
    "script": "/home/<user>/.zcode/zcode-token-usage-statusbar/zusage.py",
    "python": "python3",
    "timeout_s": 8,
    "poll_ms": 3000,
    "host_label": "<host>"
  }
}
```

`script` 必须写**绝对路径**——`~` 在非交互式 ssh 里不展开。改完不用重启悬浮条。
条面出现「远端」徽标即表示正在用远端数据。

远端快照和本地快照是**同一份结构**（远端跑的就是同一份 `zusage.py`），所以
缓存率、悬停详情的每一行在服务器会话上一样可用，不需要额外配置。悬停浮层里的
`数据源` 一行会标出 `远端 · <host_label>`，一眼能分清当前看的是哪台机器。

远端脚本要和本机**一起更新**：新增字段（比如缓存率依赖的 `cache_read`）远端
旧版没有时会按空值处理，那一项就不显示，但不会报错。更新方式就是重跑一次上面
那条 `scp`。

---

## 6. 上下文窗口从哪来

优先级：

1. `~/.zcode/v2/runtime/provider/<平台>/<版本>/endpoint-*/zcode-builtin.json`
   里 `config.modelConfigRules.modelRules[].config.properties.contextWindow`，
   按 `modelMatch` 正则匹配模型名
2. 内置兜底表（`_legacy_window`）

第一项是 ZCode 运行期下发的配置，目录带版本号，所以用 glob 找最新的。
**ZCode 改了目录结构时这里是第一个要跟的地方**——症状是上下文百分比突然变得离谱
（分母退成兜底值）。

排查：

```bash
python -c "import zusage; print(zusage.lookup_context_window('deepseek-v4.1-flash'))"
```

---

## 7. 配置文件

`hoverbar.json`，跟 `hoverbar.py` 同目录，首次运行自动生成。改完重启悬浮条生效。
点「恢复默认」会整体覆盖回 `DEFAULTS`。

```json
{
  "anchor": "bottom-left",
  "scope": "window",
  "offset_x": 16, "offset_y": 6,
  "free_x": 0, "free_y": 0,
  "opacity": 0.92,
  "panel_opacity": 1.0,
  "font_px": 13, "font_family": "",
  "always_on_top": false,
  "hide_when_inactive": false,
  "show": {"speed": true, "ctx": true, "turn": true,
           "session": true, "today": true, "tools": true, "cache": true},
  "labels": true,
  "poll_ms": 1500
}
```

取值越界会被钳到合法范围，不认识的字段忽略，不会让悬浮条起不来。

---

## 8. 排查

### 条面不出现

按顺序：

1. `python hoverbar.py --once` —— 数据能不能取到。有输出说明数据链路正常，
   问题在窗口层（多半是找不到 ZCode 窗口）。
2. `python hoverbar.py --debug` —— 看定位与取数过程。
3. 确认 ZCode 客户端确实开着。没开时条面会主动隐藏。
4. `python hoverbar.py --quit` 再启动，排掉上一次残留实例。

### 数字不对 / 一直是 0

- 请求完成才落库，正在生成的不计入
- 新会话还没产生已完成请求时是零值并带来源提示
- 会话迁移过工作区的话，历史用量会一并算进新目录（`model_usage` 没有工作区维度）

### 条面位置不对 / 掉到屏幕外

设置面板里选「范围 = 整块屏幕」再挑个贴靠角落，或直接拖动条面（会变成「自定义」）。
`hoverbar.json` 里的 `free_x` / `free_y` 是拖动后的坐标，手改也行。

### 开机没自动出来

```bash
python autostart.py status
```

它会把将要执行的命令行打出来。常见的失效原因：Python 换了版本、项目目录搬了——
注册表里存的是绝对路径。`status` 会检测解释器路径还在不在，不在就重跑
`python autostart.py on` 修好。

### 想回到默认配置

删掉 `hoverbar.json`，重启悬浮条。

---

## 9. 卸载

```bash
python autostart.py off     # 如果开过自启
python hoverbar.py --quit   # 关掉正在跑的那条
```

然后删掉整个项目目录。悬浮条不写任何别的地方——不碰客户端安装目录，
配置只在自己目录里的 `hoverbar.json`，自启只占一个 HKCU Run 项。
