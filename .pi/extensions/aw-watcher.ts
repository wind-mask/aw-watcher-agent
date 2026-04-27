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
 * - agent_start → heartbeat（会话保活）
 * - agent_end  → update（增量 token/cost + 按模型拆分的用量，daemon 侧累加）
 * - session_shutdown → end（含最后一批增量+分模型数据）
 * - session 结束时 AW bucket 的 final event 包含按模型的 token/cost 明细
 */

import { randomUUID } from "node:crypto";
import type {
  ExtensionAPI,
  ExtensionContext,
} from "@mariozechner/pi-coding-agent";

const DAEMON_URL = (
  process.env.AW_WATCHER_DAEMON_URL ?? "http://127.0.0.1:5667"
).replace(/\/$/, "");

type TokenUsage = {
  input?: number;
  output?: number;
  cache_read?: number;
  cache_write?: number;
  total?: number;
};

type CostUsage = {
  total?: number;
  currency?: string;
};

type ModelUsage = {
  model: string;
  tokens: TokenUsage;
  cost: number;
};

type UsageSnapshot = {
  tokens: Required<TokenUsage>;
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

let currentSessionId: string | null = null;
let daemonWarned = false;
let currentModel: string | undefined;
let lastBranchLength = 0;

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
      total: Number(usage.totalTokens ?? input + output + cacheRead + cacheWrite),
    },
    cost: Number(typeof cost === "number" ? cost : cost?.total ?? 0),
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

function generateSessionId(): string {
  return `pi-${randomUUID().substring(0, 8)}`;
}

function modelId(ctx: ExtensionContext): string | undefined {
  return ctx?.model?.id ?? ctx?.model?.name ?? currentModel;
}

/**
 * 增量消费 branch 中新增的 assistant message 的 token/cost。
 * 只处理 lastBranchLength 之后的新条目，返回增量值（daemon 侧做累加）。
 * 同时按模型汇总，供 AW 最终事件写入分模型用量。
 * 返回 null 表示没有新数据需要上报。
 */
function consumeIncrementalUsage(
  ctx: ExtensionContext,
): { tokens: TokenUsage; cost: CostUsage; model_usage: ModelUsage[] } | null {
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
  if (!hasData) return null;

  const model_usage: ModelUsage[] = Array.from(modelMap.entries()).map(
    ([model, snap]) => ({
      model,
      tokens: { ...snap.tokens },
      cost: snap.cost,
    }),
  );

  return {
    tokens: { ...inc.tokens },
    cost: { total: inc.cost, currency: "USD" },
    model_usage,
  };
}

async function post(path: string, body: unknown): Promise<void> {
  try {
    const res = await fetch(`${DAEMON_URL}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });

    if (!res.ok && !daemonWarned) {
      daemonWarned = true;
      console.log(`[aw-watcher] daemon returned HTTP ${res.status}`);
    }
  } catch (err) {
    if (!daemonWarned) {
      daemonWarned = true;
      console.log(
        `[aw-watcher] daemon unavailable at ${DAEMON_URL}; session will not be tracked.`,
      );
      console.log(err);
    }
  }
}

export default function (pi: ExtensionAPI) {
  pi.on("model_select", async (event) => {
    currentModel = event?.model?.id ?? event?.model?.name ?? currentModel;
    if (currentSessionId) {
      await post("/api/v1/session/update", {
        session_id: currentSessionId,
        model: currentModel,
      });
    }
  });

  pi.on("session_start", async (_event, ctx) => {
    currentSessionId = generateSessionId();
    // 记录当前 branch 长度，后续增量消费只处理新消息
    lastBranchLength = (
      ctx?.sessionManager?.getBranch?.() ?? []
    ).length;
    currentModel = modelId(ctx);

    await post("/api/v1/session/start", {
      session_id: currentSessionId,
      code_agent: "pi",
      project_dir: process.cwd(),
      model: currentModel,
      metadata: {
        extension: "pi-aw-watcher",
      },
    });
  });

  // agent_start：仅发 heartbeat 保活，不发数据（token/cost 稳定时避免 AW 切段）
  pi.on("agent_start", async () => {
    if (!currentSessionId) return;
    await post("/api/v1/session/heartbeat", {
      session_id: currentSessionId,
    });
  });

  // agent_end：只发增量 token/cost update，不发 heartbeat（减少冗余）
  pi.on("agent_end", async (_event, ctx) => {
    if (!currentSessionId) return;
    currentModel = modelId(ctx);
    const delta = consumeIncrementalUsage(ctx);

    if (delta) {
      await post("/api/v1/session/update", {
        session_id: currentSessionId,
        model: currentModel,
        tokens: delta.tokens,
        cost: delta.cost,
        model_usage: delta.model_usage,
      });
    }
  });

  // session_shutdown：发最后一批增量数据并结束 session
  pi.on("session_shutdown", async (_event, ctx) => {
    if (!currentSessionId) return;
    const sessionId = currentSessionId;
    currentSessionId = null;

    currentModel = modelId(ctx);
    const delta = consumeIncrementalUsage(ctx);

    await post("/api/v1/session/end", {
      session_id: sessionId,
      tokens: delta?.tokens,
      cost: delta?.cost,
      model_usage: delta?.model_usage,
    });
  });
}
