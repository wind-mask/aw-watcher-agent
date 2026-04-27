# aw-watcher-agent

!!!本项目是纯粹的vibe coding产物，我尽可能确保其可用性和正确性，但是不对此有任何保障。!!!

[English](#overview) | [中文](#概览)

## Overview

ActivityWatch watcher for code-agent sessions. It records high-level AI coding session activity from code agent into ActivityWatch without storing prompt text or tool-call details.

## 概览

`aw-watcher-agent` 是一个面向 code agent 会话的 ActivityWatch watcher。它可以把 code agent 中的 AI 编码会话记录到 ActivityWatch，但不会保存 prompt 文本、assistant 回复文本或工具调用详情。

---

## What it records

`aw-watcher-agent` tracks session-level metadata only:

- code agent name (`pi`)
- project directory/name
- session duration via ActivityWatch heartbeats
- current/final model
- total token usage and cost
- per-model token/cost breakdown for sessions that use multiple models

It does **not** record:

- prompt text
- assistant response text
- tool-call arguments/results
- file contents

## 记录哪些数据

`aw-watcher-agent` 只记录会话级元数据：

- code agent 名称，例如 `pi`
- 项目目录/项目名
- 通过 ActivityWatch heartbeat 记录的 session 时长
- 当前/最终使用的模型
- 总 token 用量和费用
- 多模型 session 中按模型拆分的 token/cost 明细

它**不会**记录：

- prompt 文本
- assistant 回复文本
- 工具调用参数/结果
- 文件内容

---

## Components

This repository ships two parts:

1. **Rust daemon** (`aw-watcher-agent`)
   - Receives HTTP session events from code-agent integrations.
   - Writes ActivityWatch heartbeat events to the bucket `aw-watcher-agent_<hostname>`.

2. **pi extension** (`.pi/extensions/aw-watcher.ts`)
   - Hooks into pi session/model/agent lifecycle events.
   - Sends session start/update/heartbeat/end events to the daemon.

## 组件

本仓库包含两部分：

1. **Rust daemon** (`aw-watcher-agent`)
   - 接收来自 code agent 集成的 HTTP session 事件。
   - 将 ActivityWatch heartbeat 写入 `aw-watcher-agent_<hostname>` bucket。

2. **pi 扩展** (`.pi/extensions/aw-watcher.ts`)
   - 监听 pi 的 session/model/agent 生命周期事件。
   - 向 daemon 发送 session start/update/heartbeat/end 事件。

---

## ActivityWatch data model

The daemon writes to an ActivityWatch bucket with type:

```text
app.editor.activity
```

The final completed event contains top-level aggregate fields:

```json
{
  "status": "completed",
  "session_id": "pi-72aa7acf",
  "code_agent": "pi",
  "project": "aw-watcher-agent",
  "project_dir": "/path/to/aw-watcher-agent",
  "model": "deepseek-v4-pro",
  "tokens_input": 24364,
  "tokens_output": 95,
  "tokens_cache_read": 5120,
  "tokens_cache_write": 0,
  "tokens_total": 29579,
  "cost_total": 0.02621523,
  "cost_currency": "USD"
}
```

For multi-model sessions, the final event also contains `model_usage`:

```json
{
  "model_usage": {
    "deepseek-v4-flash": {
      "tokens_input": 8050,
      "tokens_output": 39,
      "tokens_cache_read": 2560,
      "tokens_cache_write": 0,
      "tokens_total": 10649,
      "cost": 0.0012096
    },
    "deepseek-v4-pro": {
      "tokens_input": 8073,
      "tokens_output": 42,
      "tokens_cache_read": 2560,
      "tokens_cache_write": 0,
      "tokens_total": 10675,
      "cost": 0.01456438
    }
  }
}
```

`model_usage` values sum to the top-level token/cost totals.

## ActivityWatch 数据模型

daemon 写入的 ActivityWatch bucket 类型为：

```text
app.editor.activity
```

最终的 completed event 会包含顶层汇总字段，例如：

```json
{
  "status": "completed",
  "session_id": "pi-72aa7acf",
  "code_agent": "pi",
  "project": "aw-watcher-agent",
  "project_dir": "/path/to/aw-watcher-agent",
  "model": "deepseek-v4-pro",
  "tokens_input": 24364,
  "tokens_output": 95,
  "tokens_cache_read": 5120,
  "tokens_cache_write": 0,
  "tokens_total": 29579,
  "cost_total": 0.02621523,
  "cost_currency": "USD"
}
```

如果一个 session 使用了多个模型，final event 还会包含 `model_usage`：

```json
{
  "model_usage": {
    "deepseek-v4-flash": {
      "tokens_input": 8050,
      "tokens_output": 39,
      "tokens_cache_read": 2560,
      "tokens_cache_write": 0,
      "tokens_total": 10649,
      "cost": 0.0012096
    },
    "deepseek-v4-pro": {
      "tokens_input": 8073,
      "tokens_output": 42,
      "tokens_cache_read": 2560,
      "tokens_cache_write": 0,
      "tokens_total": 10675,
      "cost": 0.01456438
    }
  }
}
```

`model_usage` 中各模型的 token/cost 之和会等于顶层汇总值。

---

## Installation

### Rust daemon

Download the binary from GitHub Releases, or build from source:

```bash
cargo build --release
```

Then place the binary on your `PATH`:

```bash
install -Dm755 target/release/aw-watcher-agent ~/.local/bin/aw-watcher-agent
```

### pi extension

The npm package contains only the pi extension and package metadata.

```bash
pi install @wind_mask/aw-watcher-agent-pi
```

Alternatively, load the extension file directly while developing:

```bash
pi --extension .pi/extensions/aw-watcher.ts
```

## 安装

### Rust daemon

可以从 GitHub Releases 下载二进制文件，也可以从源码构建：

```bash
cargo build --release
```

然后将二进制放入 `PATH`：

```bash
install -Dm755 target/release/aw-watcher-agent ~/.local/bin/aw-watcher-agent
```

### pi 扩展

npm 包只包含 pi 扩展和 package 元数据。

```bash
pi install @wind_mask/aw-watcher-agent-pi
```

开发时也可以直接加载扩展文件：

```bash
pi --extension .pi/extensions/aw-watcher.ts
```

---

## Usage

Start ActivityWatch first, then run the daemon:

```bash
aw-watcher-agent daemon
```

By default, the daemon:

- connects to ActivityWatch at `localhost:5600`
- listens for pi extension events on `127.0.0.1:5667`

Check daemon status:

```bash
aw-watcher-agent status
```

Remove the ActivityWatch bucket created by this watcher:

```bash
aw-watcher-agent teardown
```



### Environment variables

The pi extension sends events to:

```text
http://127.0.0.1:5667
```

Override it with:

```bash
export AW_WATCHER_DAEMON_URL=http://127.0.0.1:5667
```

Agent heartbeat interval while pi is generating a response defaults to 15 seconds.
The minimum effective value is 5 seconds:

```bash
export AW_WATCHER_AGENT_HEARTBEAT_INTERVAL_MS=15000
```

The daemon connects to ActivityWatch using CLI options:

```bash
aw-watcher-agent --host localhost --port 5600 daemon
```

## 使用

先启动 ActivityWatch，然后运行 daemon：

```bash
aw-watcher-agent daemon
```

默认情况下，daemon 会：

- 连接到 `localhost:5600` 上的 ActivityWatch
- 在 `127.0.0.1:5667` 监听 pi 扩展事件

检查 daemon 状态：

```bash
aw-watcher-agent status
```

删除本 watcher 创建的 ActivityWatch bucket：

```bash
aw-watcher-agent teardown
```

### 环境变量

pi 扩展默认将事件发送到：

```text
http://127.0.0.1:5667
```

可以通过环境变量覆盖：

```bash
export AW_WATCHER_DAEMON_URL=http://127.0.0.1:5667
```

agent 生成回复期间的 heartbeat 间隔默认为 15 秒；有效最小值为 5 秒：

```bash
export AW_WATCHER_AGENT_HEARTBEAT_INTERVAL_MS=15000
```

daemon 连接 ActivityWatch 的地址通过 CLI 选项配置：

```bash
aw-watcher-agent --host localhost --port 5600 daemon
```

---

## Resume behavior

When a pi session is resumed, the extension records only usage produced after the resume point. Existing historical messages in the resumed session file are not counted again.

## Resume 行为

当 pi session 被 resume 时，扩展只记录 resume 之后新增的用量。resume 前已经存在于 session 文件中的历史消息不会被重复计入。

---


## License

MIT

## 许可证

MIT
