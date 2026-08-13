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
- logical Pi session ID and per-run `session_instance_id`
- session duration via ActivityWatch heartbeats sampled by the Pi extension
- actual model used for token/cost usage and the selected model separately
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
- Pi 逻辑 session ID 和每次运行的 `session_instance_id`
- 由 Pi 扩展 heartbeat 采样的 session 时长
- 实际产生 usage 的模型，以及单独记录当前选中的模型
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
   - Writes merged live activity heartbeats to `aw-watcher-agent-event_<hostname>`.
   - Writes per-session completed/abandoned summaries to `aw-watcher-agent-sum_<hostname>`.

2. **pi extension** (`.pi/extensions/aw-watcher.ts`)
   - Hooks into pi session/model/agent lifecycle events.
   - Sends session start/update/heartbeat/end events to the daemon.

## 组件

本仓库包含两部分：

1. **Rust daemon** (`aw-watcher-agent`)
   - 接收来自 code agent 集成的 HTTP session 事件。
   - 将合并后的实时活动 heartbeat 写入 `aw-watcher-agent-event_<hostname>`。
   - 将单个 session 的 completed/abandoned 汇总写入 `aw-watcher-agent-sum_<hostname>`。

2. **pi 扩展** (`.pi/extensions/aw-watcher.ts`)
   - 监听 pi 的 session/model/agent 生命周期事件。
   - 向 daemon 发送 session start/update/heartbeat/end 事件。

---

## ActivityWatch data model

The daemon writes two ActivityWatch buckets:

```text
aw-watcher-agent-event_<hostname>  type=app.editor.activity
aw-watcher-agent-sum_<hostname>    type=app.code-agent.summary
```

The event bucket contains merged live activity. If multiple agent sessions are
active at the same time, they are represented as one heartbeat event:

```json
{
  "status": "active",
  "language": "code-agent",
  "project": "multiple",
  "file": "multiple",
  "active_session_count": 2,
  "code_agents": ["pi", "codex"],
  "projects": ["aw-watcher-agent", "virt"],
  "project_dirs": ["/path/to/aw-watcher-agent", "/path/to/virt"],
  "sessions": [
    {
      "code_agent": "pi",
      "session_id": "pi-72aa7acf",
      "session_instance_id": "...",
      "project": "aw-watcher-agent",
      "project_dir": "/path/to/aw-watcher-agent",
      "model": "deepseek-v4-pro"
    }
  ]
}
```

Each live heartbeat is timestamped with the sampling time reported by the
extension (`agent_start`, periodic heartbeat, `agent_settled`, or end). The
daemon only aggregates these reported times; it does not generate periodic
activity itself.

The sum bucket contains one event per session instance. Its ActivityWatch
event `timestamp` is the start of the active period, `duration` is the active
duration in seconds, and `data` contains session metadata and completion
state. Completed summaries contain the final usage reported by the session
end event:

```json
{
  "status": "completed",
  "session_id": "pi-72aa7acf",
  "session_instance_id": "...",
  "code_agent": "pi",
  "project": "aw-watcher-agent",
  "project_dir": "/path/to/aw-watcher-agent",
  "model": "deepseek-v4-pro",
  "selected_model": "gpt-5.4",
  "active_duration_seconds": 42.5,
  "wall_duration_seconds": 80.2,
  "tokens_input": 24364,
  "tokens_output": 95,
  "tokens_cache_read": 5120,
  "tokens_cache_write": 0,
  "tokens_total": 29579,
  "cost_total": 0.02621523,
  "cost_currency": "USD",
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

Top-level `model` reflects the actual usage model (`"multiple"` when more
than one model produced usage); `selected_model` records the model selected
for the session. Usage snapshots are cumulative per instance, and top-level
token/cost totals are derived from `model_usage` so they always match it.

Sessions that were active but timed out before an end event are written to
the sum bucket with `"status": "abandoned"` and `"reason": "timeout"` (or
`"reason": "duplicate_start"` / `"shutdown"`). Abandoned summaries keep the
last usage snapshot received before the timeout.

## ActivityWatch 数据模型

daemon 写入两个 ActivityWatch bucket：

```text
aw-watcher-agent-event_<hostname>  type=app.editor.activity
aw-watcher-agent-sum_<hostname>    type=app.code-agent.summary
```

event bucket 记录合并后的实时活动；如果多个 agent session 同时活跃，会被合成
一条 heartbeat：

```json
{
  "status": "active",
  "language": "code-agent",
  "project": "multiple",
  "file": "multiple",
  "active_session_count": 2,
  "code_agents": ["pi", "codex"],
  "projects": ["aw-watcher-agent", "virt"],
  "project_dirs": ["/path/to/aw-watcher-agent", "/path/to/virt"],
  "sessions": [
    {
      "code_agent": "pi",
      "session_id": "pi-72aa7acf",
      "session_instance_id": "...",
      "project": "aw-watcher-agent",
      "project_dir": "/path/to/aw-watcher-agent",
      "model": "deepseek-v4-pro"
    }
  ]
}
```

每条实时 heartbeat 都带有扩展上报的采样时间（`agent_start`、定时 heartbeat、
`agent_settled` 或 end）；daemon 只负责聚合并写入，不自行生成周期性活动。

sum bucket 每条 event 对应一个 session 实例。ActivityWatch event 的
`timestamp` 是活跃期开始时间，`duration` 是活跃秒数，`data` 包含 session
元数据和完成状态。completed summary 包含 session end 事件上报的最终
usage：

```json
{
  "status": "completed",
  "session_id": "pi-72aa7acf",
  "session_instance_id": "...",
  "code_agent": "pi",
  "project": "aw-watcher-agent",
  "project_dir": "/path/to/aw-watcher-agent",
  "model": "deepseek-v4-pro",
  "selected_model": "gpt-5.4",
  "active_duration_seconds": 42.5,
  "wall_duration_seconds": 80.2,
  "tokens_input": 24364,
  "tokens_output": 95,
  "tokens_cache_read": 5120,
  "tokens_cache_write": 0,
  "tokens_total": 29579,
  "cost_total": 0.02621523,
  "cost_currency": "USD",
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

顶层 `model` 反映实际产生 usage 的模型（多个模型时记为 `"multiple"`）；
`selected_model` 记录会话当前选中的模型。usage 快照按实例累计，顶层
token/cost 由 `model_usage` 汇总得到，两者始终一致。

活跃过但未收到 end 事件的 session 会以 `"status": "abandoned"` 和
`"reason": "timeout"`（或 `"reason": "duplicate_start"` / `"shutdown"`）写入
sum bucket；abandoned summary 会保留超时前最后收到的 usage 快照。

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
- does not generate activity heartbeats; those are sampled and scheduled by the pi extension
- runs a 10-second abandoned-session sweep; after 300 seconds without an agent signal, it archives agents that crash or are force-killed

Check daemon status:

```bash
aw-watcher-agent status
```

Remove the ActivityWatch buckets created by this watcher:

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
- 不自行生成活动 heartbeat，heartbeat 由 pi 扩展采样并定时上报
- 每 10 秒扫描一次 abandoned session；agent 连续 300 秒没有信号时归档崩溃或被强制终止的 agent

检查 daemon 状态：

```bash
aw-watcher-agent status
```

删除本 watcher 创建的 ActivityWatch buckets：

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

When a pi session is resumed, the extension creates a new `session_instance_id`
and records only usage produced after the resume point. Existing historical
messages in the resumed session file are not counted again. Summing summaries
with the same logical `session_id` gives the total across resumed instances.

## Resume 行为

当 pi session 被 resume 时，扩展会创建新的 `session_instance_id`，只记录 resume
之后新增的用量；resume 前已经存在于 session 文件中的历史消息不会被重复计入。
按相同逻辑 `session_id` 汇总多个 summary，即可得到跨 resume 实例的总量。

---

## Migrating legacy data

Versions before the dual-bucket data model wrote everything to the legacy
`aw-watcher-agent_<hostname>` bucket. To backfill existing exports into the new
`aw-watcher-agent-event_<hostname>` and `aw-watcher-agent-sum_<hostname>`
buckets, export the old bucket from ActivityWatch and run:

```bash
python3 scripts/migrate_legacy_aw_bucket.py aw-bucket-export_aw-watcher-agent_<hostname>.json --output migrated-aw-watcher-agent.json
```

The command above is a dry run and writes a converted export file. After
reviewing the printed counts, import directly into a running ActivityWatch
server with:

```bash
python3 scripts/migrate_legacy_aw_bucket.py aw-bucket-export_aw-watcher-agent_<hostname>.json --apply
```

Use `--skip-event-bucket` to migrate only completed/abandoned summaries, or
`--skip-abandoned` to avoid creating abandoned summaries for sessions that never
sent an end event.

## 迁移旧数据

双 bucket 数据模型之前的版本会把所有数据写入旧的
`aw-watcher-agent_<hostname>` bucket。要把已有导出回填到新的
`aw-watcher-agent-event_<hostname>` 和 `aw-watcher-agent-sum_<hostname>`
buckets，先从 ActivityWatch 导出旧 bucket，然后运行：

```bash
python3 scripts/migrate_legacy_aw_bucket.py aw-bucket-export_aw-watcher-agent_<hostname>.json --output migrated-aw-watcher-agent.json
```

上面的命令是 dry run，并会写出转换后的 export 文件。确认输出统计无误后，可以
直接导入到正在运行的 ActivityWatch：

```bash
python3 scripts/migrate_legacy_aw_bucket.py aw-bucket-export_aw-watcher-agent_<hostname>.json --apply
```

如果只想迁移 completed/abandoned 汇总，可加 `--skip-event-bucket`；如果不想为
没有 end 事件的 session 创建 abandoned summary，可加 `--skip-abandoned`。

---


## License

MIT

## 许可证

MIT
