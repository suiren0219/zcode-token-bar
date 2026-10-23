# 使用说明

面向已经跑起来的人：每个设置项改什么、行为细节、出问题怎么查。
安装见 [README](../README.md)。

---

## 1. 三个入口

| 入口 | 位置 | 用来做什么 |
|---|---|---|
| 条面 | ZCode 窗口上方的胶囊 | 看当前会话用量 |
| 右端切角方块 | 条面最右侧 | 单击打开设置面板，悬停翻酸黄色 |
| 设置面板 | 见上 | 位置 / 外观 / 显示项 / 行为 |

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

颜色：生成速度 ≥70 绿、40–70 黄、<40 红；上下文占比在大窗口（≥100 万）下
40%/60% 预警，其余窗口 70%/85% 预警。

数据来自本地 SQLite 的只读查询，请求完成后才落库，所以数字反映的是
「最近一次已完成请求」。正在生成的请求不计入。

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

六项独立开关，加一个「显示文字标签」总开关。关掉标签只留数字，条面会明显变短；
全关时条面显示「等待数据…」而不是缩成一条线。

### 04 行为

| 项 | 默认 | 说明 |
|---|---|---|
| 总是置顶 | 关 | 勾了就是真正的 topmost，会盖住其它程序（包括全屏播放器）。默认不勾。 |
| ZCode 不在前台时隐藏 | 关 | 勾了之后切到别的窗口就自动收起；属主绑定本身已经会在 ZCode 最小化时让路，一般不用再勾。 |

面板内容比屏幕高时，右侧会出现 3px 酸黄细线滚动指示。面板按指针摆放并夹在
屏幕内（留 8px 边距和 48px 底部余量）。

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
  "font_px": 13, "font_family": "",
  "always_on_top": false,
  "hide_when_inactive": false,
  "show": {"speed": true, "ctx": true, "turn": true,
           "session": true, "today": true, "tools": true},
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
