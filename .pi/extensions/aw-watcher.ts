/**
 * aw-watcher pi 扩展
 *
 * 新架构：不再为每个事件 spawn CLI，而是向后台 aw-watcher-agent daemon
 * 发送 HTTP session 事件。扩展只记录会话级信息：code agent、项目目录、模型、
 * token 用量和费用；不记录 prompt 文本，也不记录 tool 调用。
 *
 * 先启动 daemon：
 *   aw-watcher-agent daemon
 *
 * 环境变量：
 *   AW_WATCHER_DAEMON_URL - daemon 地址，默认 http://127.0.0.1:5667
 *
 * 架构说明：
 * - agent_start → 启动扩展侧定时 heartbeat（agent 运行期间保活）
 * - agent_end  → 停止定时 heartbeat，本地累计 token/cost，并 update 当前模型
 * - session_shutdown → 停止 heartbeat 并 end（含整个 session 的最终 usage 汇总）
 * - session 结束时 daemon 将 token/cost 明细写入 sum bucket
 */

import type {
  ExtensionAPI,
  ExtensionContext,
} from "@mariozechner/pi-coding-agent";
import type { CostUsage } from "../bindings/CostUsage";
import type { ModelUsage } from "../bindings/ModelUsage";
import type { TokenUsage } from "../bindings/TokenUsage";

const DAEMON_URL = (
  process.env.AW_WATCHER_DAEMON_URL ?? "http://127.0.0.1:5667"
).replace(/\/$/, "");
const CODE_AGENT = "pi";

const configuredHeartbeatInterval = Number(
  process.env.AW_WATCHER_AGENT_HEARTBEAT_INTERVAL_MS ?? 15_000,
);
const AGENT_HEARTBEAT_INTERVAL_MS = Math.max(
  5_000,
  Number.isFinite(configuredHeartbeatInterval)
    ? configuredHeartbeatInterval
    : 15_000,
);

// ---- 内部类型 ----

type UsageSnapshot = {
  tokens: {
    input: number;
    output: number;
    cache_read: number;
    cache_write: number;
    total: number;
  };
  cost: number;
};

type PiUsageLike = {
  input?: number;
  output?: number;
  cacheRead?: number;
  cacheWrite?: number;
  totalTokens?: number;
  cost?: { total?: number } | number;
};

// ---- 状态 ----

let currentSessionId: string | null = null;
let daemonLastWarnAt = 0;
let currentModel: string | undefined;
let lastBranchLength = 0;
let sessionUsage = emptyUsageSnapshot();
let sessionModelUsage = new Map<string, UsageSnapshot>();
let heartbeatTimer: ReturnType<typeof setInterval> | null = null;
let heartbeatInFlight = false;
let heartbeatPendingSessionId: string | null = null;

// ---- 用量计算 ----

function emptyUsageSnapshot(): UsageSnapshot {
  return {
    tokens: { input: 0, output: 0, cache_read: 0, cache_write: 0, total: 0 },
    cost: 0,
  };
}

function snapshotFromUsage(usage: PiUsageLike): UsageSnapshot {
  const input = Number(usage.input ?? 0);
  const output = Number(usage.output ?? 0);
  const cacheRead = Number(usage.cacheRead ?? 0);
  const cacheWrite = Number(usage.cacheWrite ?? 0);
  const cost = usage.cost;

  return {
    tokens: {
      input,
      output,
      cache_read: cacheRead,
      cache_write: cacheWrite,
      total: Number(
        usage.totalTokens ?? input + output + cacheRead + cacheWrite,
      ),
    },
    cost: Number(typeof cost === "number" ? cost : (cost?.total ?? 0)),
  };
}

function addUsageSnapshot(target: UsageSnapshot, source: UsageSnapshot): void {
  target.tokens.input += source.tokens.input;
  target.tokens.output += source.tokens.output;
  target.tokens.cache_read += source.tokens.cache_read;
  target.tokens.cache_write += source.tokens.cache_write;
  target.tokens.total += source.tokens.total;
  target.cost += source.cost;
}

function modelId(ctx: ExtensionContext): string | undefined {
  return ctx?.model?.id ?? ctx?.model?.name ?? currentModel;
}

/**
 * 增量消费 branch 中新增的 assistant message 的 token/cost。
 * 扩展侧只在内存中累计，daemon 只在 session end 时接收最终 usage 汇总。
 */
function consumeIncrementalUsage(ctx: ExtensionContext): void {
  const branch = ctx?.sessionManager?.getBranch?.() ?? [];
  const startIdx = lastBranchLength;

  const inc = emptyUsageSnapshot();
  const modelMap = new Map<string, UsageSnapshot>();

  for (let i = startIdx; i < branch.length; i++) {
    const entry = branch[i];
    const message = entry?.type === "message" ? entry.message : undefined;
    if (!message || message.role !== "assistant") continue;

    const usage = message.usage;
    if (!usage) continue;

    const model = message.model ?? currentModel ?? "unknown";
    const usageSnapshot = snapshotFromUsage(usage);

    addUsageSnapshot(inc, usageSnapshot);

    let m = modelMap.get(model);
    if (!m) {
      m = emptyUsageSnapshot();
      modelMap.set(model, m);
    }
    addUsageSnapshot(m, usageSnapshot);
  }

  lastBranchLength = branch.length;

  const hasData = inc.tokens.total > 0 || inc.cost > 0;
  if (!hasData) return;

  addUsageSnapshot(sessionUsage, inc);
  for (const [model, snap] of modelMap.entries()) {
    let target = sessionModelUsage.get(model);
    if (!target) {
      target = emptyUsageSnapshot();
      sessionModelUsage.set(model, target);
    }
    addUsageSnapshot(target, snap);
  }
}

function finalUsagePayload():
  | { tokens: TokenUsage; cost: CostUsage; model_usage: ModelUsage[] }
  | Record<string, never> {
  const hasData = sessionUsage.tokens.total > 0 || sessionUsage.cost > 0;
  if (!hasData) return {};

  return {
    tokens: {
      input: sessionUsage.tokens.input,
      output: sessionUsage.tokens.output,
      cache_read: sessionUsage.tokens.cache_read,
      cache_write: sessionUsage.tokens.cache_write,
      total: sessionUsage.tokens.total,
    },
    cost: { total: sessionUsage.cost, currency: "USD" },
    model_usage: Array.from(sessionModelUsage.entries()).map(
      ([model, snap]) => ({
        model,
        tokens: {
          input: snap.tokens.input,
          output: snap.tokens.output,
          cache_read: snap.tokens.cache_read,
          cache_write: snap.tokens.cache_write,
          total: snap.tokens.total,
        },
        cost: snap.cost,
      }),
    ),
  };
}

// ---- HTTP 通信（带重试） ----

const MAX_RETRIES = 3;
const INITIAL_BACKOFF_MS = 100;
const DAEMON_WARN_INTERVAL_MS = 60_000;

function warnDaemon(message: string, err?: unknown): void {
  const now = Date.now();
  if (now - daemonLastWarnAt < DAEMON_WARN_INTERVAL_MS) return;
  daemonLastWarnAt = now;
  console.log(message);
  if (err) console.log(err);
}

async function post(path: string, body: unknown): Promise<void> {
  let delay = INITIAL_BACKOFF_MS;
  for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
    try {
      const res = await fetch(`${DAEMON_URL}${path}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      if (res.ok) return;
      if (attempt === MAX_RETRIES - 1) {
        warnDaemon(`[aw-watcher] daemon returned HTTP ${res.status}`);
      }
    } catch (err) {
      if (attempt < MAX_RETRIES - 1) {
        await new Promise((r) => setTimeout(r, delay));
        delay *= 2;
      } else {
        warnDaemon(
          `[aw-watcher] daemon unavailable at ${DAEMON_URL}; session will not be tracked.`,
          err,
        );
      }
    }
  }
}

// ---- 心跳管理 ----

async function sendHeartbeat(sessionId: string): Promise<void> {
  if (heartbeatInFlight) {
    heartbeatPendingSessionId = sessionId;
    return;
  }
  heartbeatInFlight = true;
  try {
    let nextSessionId: string | null = sessionId;
    while (nextSessionId) {
      heartbeatPendingSessionId = null;
      await post("/api/v1/session/heartbeat", {
        session_id: nextSessionId,
        code_agent: CODE_AGENT,
      });

      const pendingSessionId = heartbeatPendingSessionId;
      nextSessionId =
        pendingSessionId && currentSessionId === pendingSessionId
          ? pendingSessionId
          : null;
    }
  } finally {
    heartbeatInFlight = false;
  }
}

function stopAgentHeartbeat(): void {
  if (heartbeatTimer) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = null;
  }
}

function startAgentHeartbeat(): void {
  if (!currentSessionId) return;
  const sessionId = currentSessionId;

  stopAgentHeartbeat();
  void sendHeartbeat(sessionId);

  heartbeatTimer = setInterval(() => {
    if (currentSessionId !== sessionId) {
      stopAgentHeartbeat();
      return;
    }
    void sendHeartbeat(sessionId);
  }, AGENT_HEARTBEAT_INTERVAL_MS);
}

// ---- pi 事件钩子 ----

export default function (pi: ExtensionAPI) {
  pi.on("model_select", async (event) => {
    currentModel = event?.model?.id ?? event?.model?.name ?? currentModel;
    if (currentSessionId) {
      await post("/api/v1/session/update", {
        session_id: currentSessionId,
        code_agent: CODE_AGENT,
        model: currentModel,
      });
    }
  });

  pi.on("session_start", async (_event, ctx) => {
    currentSessionId = ctx.sessionManager.getSessionId();
    // 记录当前 branch 长度，后续增量消费只处理新消息
    lastBranchLength = (ctx?.sessionManager?.getBranch?.() ?? []).length;
    sessionUsage = emptyUsageSnapshot();
    sessionModelUsage = new Map<string, UsageSnapshot>();
    currentModel = modelId(ctx);
    stopAgentHeartbeat();

    await post("/api/v1/session/start", {
      session_id: currentSessionId,
      code_agent: CODE_AGENT,
      project_dir: process.cwd(),
      model: currentModel,
      metadata: {
        extension: "pi-aw-watcher",
      },
    });
  });

  // agent_start：agent 运行期间启动扩展侧定时 heartbeat，避免长时间生成时 AW pulsetime 超时
  pi.on("agent_start", async () => {
    startAgentHeartbeat();
  });

  // agent_end：停止定时 heartbeat，只在扩展侧累计 usage，daemon 只在 end 接收最终汇总。
  pi.on("agent_end", async (_event, ctx) => {
    if (!currentSessionId) return;
    stopAgentHeartbeat();
    currentModel = modelId(ctx);
    consumeIncrementalUsage(ctx);

    await post("/api/v1/session/update", {
      session_id: currentSessionId,
      code_agent: CODE_AGENT,
      model: currentModel,
    });
  });

  // session_shutdown：发送整个 session 的最终 usage 汇总并结束 session。
  pi.on("session_shutdown", async (_event, ctx) => {
    if (!currentSessionId) return;
    const sessionId = currentSessionId;
    currentSessionId = null;
    stopAgentHeartbeat();

    currentModel = modelId(ctx);
    consumeIncrementalUsage(ctx);
    const usage = finalUsagePayload();

    await post("/api/v1/session/end", {
      session_id: sessionId,
      code_agent: CODE_AGENT,
      ended_at: new Date().toISOString(),
      ...usage,
    });
  });
}
