# Skills —— 本程序的技能目录

本目录存放**运行在本程序之上的技能（Skills）**。技能不是独立程序，而是依托本机
「星际迷航语音电脑系统」（Star Trek Computer，Rust 版）的常驻语音服务运行的扩展能力：

- 唤醒词检测（KWS）、语音转文字（ASR）、TTS 播报、常驻麦克风采集，**全部由主程序提供**；
- 技能**不需要自带麦克风采集或语音模型**，直接调用主程序开放的能力即可；
- 因此多个技能 / 主链路（唤醒、打断、一次性识别）共享**同一路麦克风采集**（fan-out 广播），
  不会出现「第二路麦克风占用」导致截断的问题。

## 技能如何被加载

- 每个技能是一个文件夹，内含运行文件 + 说明文档 `SKILL.md`。
- 主程序启动时扫描配置的 `skill_dirs`（设置中可配置，或在 UI「Skills」页手动导入路径）。
- `SKILL.md` 的 frontmatter（`name` / `description`）与正文会拼进 Agent 的 system prompt；
  Agent 按其中的触发规则，用 bash 调用技能目录内的命令 / 脚本，不得绕过技能自行发挥。

## 技能清单

### personal-journal —— 个人日志系统

开启后持续录音（复用主程序常驻采集，不另开第二路麦克风）：整场「开启 → 结束」只产出
**一个音频文件**（静音不落盘），后台自动按语句转写，结果追加到与音频**同名的 Markdown**。
同时把内置的 ASR / KWS 以 CLI 形式开放给技能使用（搜索能力不开放，仍只归主 Agent）。

由 Rust 单二进制 `journal` 提供，源码见 [`crates/journal`](../crates/journal)。

```bash
cd Skills/personal-journal
./journal start              # 开启（AI 只播报：Personal Log, Stardate <今日日期>）
./journal stop               # 结束（AI 只播报：已结束）
./journal status             # 状态 / 当前会话文件 / 今日目录 / 段数
./journal dir                # 查看存储根目录
./journal dir "<根目录>"      # 设置存储根目录（写回 config.json 并即时生效）
./journal write "文本" --title "标题"   # 写入当日 text 目录的 Markdown
./journal stt <wav路径>       # 内置 ASR：对已有 wav 转文字
./journal stt-live 30        # 录一段实时语音并转文字
./journal kws <wav路径>       # 内置 KWS：对已有 wav 做唤醒词检测
```

存储结构（`root` 为空时默认 `~/Documents/个人日志`）：

```
<根目录>/<YYYY-MM-DD>/
├── audio/<会话开始时间 HH-MM-SS>.wav   # 整场一个文件，只含人声片段
└── text/<会话开始时间 HH-MM-SS>.md     # 同名转写文本，按时间戳逐条追加
```

## 重新编译技能二进制

本目录内的 `journal` 为预编译产物，源码在 `crates/journal`：

```bash
cargo build --release -p journal
cp target/release/journal Skills/personal-journal/journal
```
