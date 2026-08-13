/**
 * aw-watcher pi 扩展
 *
 * 扩展向本机 aw-watcher-agent daemon 上报 session 生命周期。活动时间由 Pi
 * 事件和扩展侧 heartbeat 的采样时间决定；daemon 只负责聚合并写入 ActivityWatch。
 * 扩展只记录会话级信息，不记录 prompt、assistant 文本或工具参数。
 *
 * 环境变量：
 *   AW_WATCHER_DAEMON_URL - daemon 地址，默认 http://127.0.0.1:5667
 *   AW_WATCHER_AGENT_HEARTBEAT_INTERVAL_MS - agent 运行期间的 heartbeat 间隔
 *
 * session_id 是 Pi 的逻辑 session ID；session_instance_id 标识本次扩展生命周期，
 * 用于区分 reload、resume 或重复启动产生的运行片段。
 */

import { randomUUID } from "node:crypto";
import type {
  ExtensionAPI,
  ExtensionContext,
} from "@mariozechner/pi-coding-agent";
import type { CostUsage } from "../bindings/CostUsage";
import type { ModelUsage } from "../bindings/ModelUsage";
import type { SessionEndRequest } from "../bindings/SessionEndRequest";
import type { SessionHeartbeatRequest } from "../bindings/SessionHeartbeatRequest";
import type { SessionStartRequest } from "../bindings/SessionStartRequest";
import type { SessionUpdateRequest } from "../bindings/SessionUpdateRequest";
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

type UsageMessageLike = {
  role?: string;
  provider?: string;
  model?: string;
  responseModel?: string;
  usage?: PiUsageLike;
};

type UsageEntryLike = {
  id?: string;
  type?: string;
  message?: UsageMessageLike;
  usage?: PiUsageLike;
};

type ModelLike = {
  id?: string;
  name?: string;
  provider?: string;
};

type SessionIdentity = {
  sessionId: string;
  instanceId: string;
};

type ActivityOptions = {
  active?: boolean;
  activeAt?: string;
};

type UsagePayload = {
  tokens: TokenUsage | null;
  cost: CostUsage | null;
  model_usage: ModelUsage[] | null;
};

// ---- 状态 ----

let currentSessionId: string | null = null;
let currentSessionInstanceId: string | null = null;
let currentModel: string | undefined;
let sessionUsage = emptyUsageSnapshot();
let sessionModelUsage = new Map<string, UsageSnapshot>();
let consumedUsageEntryIds = new Set<string>();

let heartbeatTimer: ReturnType<typeof setInterval> | null = null;
let heartbeatIdentity: SessionIdentity | null = null;

// 请求串行进入 daemon，确保 start、heartbeat、update、end 顺序与 Pi 事件一致。
let requestChain: Promise<void> = Promise.resolve();

// ---- 用量计算 ----

function emptyUsageSnapshot(): UsageSnapshot {
  return {
    tokens: { input: 0, output: 0, cache_read: 0, cache_write: 0, total: 0 },
    cost: 0,
  };
}

function finiteNonNegative(value: unknown): number {
  const number = Number(value ?? 0);
  return Number.isFinite(number) && number >= 0 ? number : 0;
}

function snapshotFromUsage(usage: PiUsageLike): UsageSnapshot {
  const input = finiteNonNegative(usage.input);
  const output = finiteNonNegative(usage.output);
  const cacheRead = finiteNonNegative(usage.cacheRead);
  const cacheWrite = finiteNonNegative(usage.cacheWrite);
  const componentTotal = input + output + cacheRead + cacheWrite;
  const reportedTotal = finiteNonNegative(usage.totalTokens);
  const cost = usage.cost;

  return {
    tokens: {
      input,
      output,
      cache_read: cacheRead,
      cache_write: cacheWrite,
      // 与 Pi session totals 一致；无分项时才回退到 totalTokens。
      total: componentTotal > 0 ? componentTotal : reportedTotal,
    },
    cost: finiteNonNegative(
      typeof cost === "number" ? cost : (cost?.total ?? 0),
    ),
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

function hasUsage(snapshot: UsageSnapshot): boolean {
  return snapshot.tokens.total > 0 || snapshot.cost > 0;
}

function normalizeModel(
  model: ModelLike | undefined,
  fallback?: string,
): string | undefined {
  const id = model?.id ?? model?.name ?? fallback;
  if (!id) return undefined;
  const provider = model?.provider;
  return provider && !id.includes("/") ? `${provider}/${id}` : id;
}

function modelId(ctx: ExtensionContext): string | undefined {
  return normalizeModel(ctx?.model, currentModel);
}

function modelForUsage(message: UsageMessageLike): string {
  const model =
    message.responseModel ?? message.model ?? currentModel ?? "unknown";
  return message.provider && !model.includes("/")
    ? `${message.provider}/${model}`
    : model;
}

function usageForEntry(
  entry: UsageEntryLike,
): { model: string; snapshot: UsageSnapshot } | undefined {
  if (entry.type === "message" && entry.message?.usage) {
    const message = entry.message;
    if (message.role === "assistant") {
      return {
        model: modelForUsage(message),
        snapshot: snapshotFromUsage(message.usage),
      };
    }
    if (message.role === "toolResult") {
      return {
        model: "Tools/summaries",
        snapshot: snapshotFromUsage(message.usage),
      };
    }
  }

  if (
    (entry.type === "compaction" || entry.type === "branch_summary") &&
    entry.usage
  ) {
    return {
      model: "Tools/summaries",
      snapshot: snapshotFromUsage(entry.usage),
    };
  }

  return undefined;
}

/**
 * 按稳定 entry ID 消费整个 session 的新增 usage。resume 时已有 entry 会先标记
 * 为已消费，因此每个 session_instance 只上报本次运行片段产生的增量。
 */
function consumeIncrementalUsage(ctx: ExtensionContext): void {
  const entries = ctx?.sessionManager?.getEntries?.() ?? [];
  for (const rawEntry of entries) {
    const entry = rawEntry as UsageEntryLike;
    if (!entry.id || consumedUsageEntryIds.has(entry.id)) continue;
    consumedUsageEntryIds.add(entry.id);

    const usage = usageForEntry(entry);
    if (!usage || !hasUsage(usage.snapshot)) continue;

    addUsageSnapshot(sessionUsage, usage.snapshot);
    const target = sessionModelUsage.get(usage.model) ?? emptyUsageSnapshot();
    addUsageSnapshot(target, usage.snapshot);
    sessionModelUsage.set(usage.model, target);
  }
}

function usagePayload(): UsagePayload {
  if (!hasUsage(sessionUsage)) {
    return { tokens: null, cost: null, model_usage: null };
  }

  return {
    tokens: {
      input: sessionUsage.tokens.input,
      output: sessionUsage.tokens.output,
      cache_read: sessionUsage.tokens.cache_read,
      cache_write: sessionUsage.tokens.cache_write,
      total: sessionUsage.tokens.total,
    },
    cost: { total: sessionUsage.cost, currency: "USD" },
    model_usage: Array.from(sessionModelUsage.entries())
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([model, snapshot]) => ({
        model,
        tokens: {
          input: snapshot.tokens.input,
          output: snapshot.tokens.output,
          cache_read: snapshot.tokens.cache_read,
          cache_write: snapshot.tokens.cache_write,
          total: snapshot.tokens.total,
        },
        cost: snapshot.cost,
      })),
  };
}

// ---- 本机 HTTP 通信 ----

async function postNow(path: string, body: unknown): Promise<boolean> {
  try {
    const response = await fetch(`${DAEMON_URL}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (response.ok) return true;
    console.warn(
      `[aw-watcher] daemon returned HTTP ${response.status} for ${path}`,
    );
  } catch (err) {
    console.warn(`[aw-watcher] daemon unavailable at ${DAEMON_URL}`, err);
  }
  return false;
}

function post(path: string, body: unknown): Promise<boolean> {
  const operation = requestChain.then(() => postNow(path, body));
  requestChain = operation.then(
    () => undefined,
    () => undefined,
  );
  return operation;
}

// ---- 心跳管理 ----

function sameIdentity(
  left: SessionIdentity | null,
  right: SessionIdentity,
): boolean {
  return (
    left?.sessionId === right.sessionId && left.instanceId === right.instanceId
  );
}

async function sendHeartbeat(
  identity: SessionIdentity,
  heartbeatAt: string,
): Promise<void> {
  if (!sameIdentity(heartbeatIdentity, identity)) return;

  const body: SessionHeartbeatRequest = {
    session_id: identity.sessionId,
    session_instance_id: identity.instanceId,
    code_agent: CODE_AGENT,
    heartbeat_at: heartbeatAt,
  };
  await post("/api/v1/session/heartbeat", body);
}

function stopAgentHeartbeat(): void {
  heartbeatIdentity = null;
  if (heartbeatTimer) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = null;
  }
}

function startAgentHeartbeat(): void {
  if (!currentSessionId || !currentSessionInstanceId) return;
  const identity: SessionIdentity = {
    sessionId: currentSessionId,
    instanceId: currentSessionInstanceId,
  };

  stopAgentHeartbeat();
  heartbeatIdentity = identity;
  void sendHeartbeat(identity, new Date().toISOString());
  heartbeatTimer = setInterval(() => {
    void sendHeartbeat(identity, new Date().toISOString());
  }, AGENT_HEARTBEAT_INTERVAL_MS);
}

function updateBody(
  identity: SessionIdentity,
  activity: ActivityOptions = {},
): SessionUpdateRequest {
  return {
    session_id: identity.sessionId,
    session_instance_id: identity.instanceId,
    code_agent: CODE_AGENT,
    model: currentModel ?? null,
    ...usagePayload(),
    active: activity.active ?? null,
    active_at: activity.activeAt ?? null,
    metadata: null,
  };
}

async function postUsageSnapshot(ctx: ExtensionContext): Promise<void> {
  if (!currentSessionId || !currentSessionInstanceId) return;
  currentModel = modelId(ctx);
  consumeIncrementalUsage(ctx);
  await post(
    "/api/v1/session/update",
    updateBody({
      sessionId: currentSessionId,
      instanceId: currentSessionInstanceId,
    }),
  );
}

// ---- Pi 事件钩子 ----

export default function (pi: ExtensionAPI) {
  pi.on("model_select", async (event) => {
    currentModel = normalizeModel(event?.model, currentModel);
    if (currentSessionId && currentSessionInstanceId) {
      await post(
        "/api/v1/session/update",
        updateBody({
          sessionId: currentSessionId,
          instanceId: currentSessionInstanceId,
        }),
      );
    }
  });

  pi.on("session_start", async (_event, ctx) => {
    stopAgentHeartbeat();
    currentSessionId = ctx.sessionManager.getSessionId();
    currentSessionInstanceId = randomUUID();
    currentModel = modelId(ctx);
    sessionUsage = emptyUsageSnapshot();
    sessionModelUsage = new Map<string, UsageSnapshot>();
    consumedUsageEntryIds = new Set(
      ctx.sessionManager.getEntries().map((entry) => entry.id),
    );

    const sessionStart: SessionStartRequest = {
      session_id: currentSessionId,
      session_instance_id: currentSessionInstanceId,
      code_agent: CODE_AGENT,
      project_dir: ctx.sessionManager.getCwd() || ctx.cwd,
      model: currentModel ?? null,
      started_at: new Date().toISOString(),
      metadata: { extension: "pi-aw-watcher" },
    };
    await post("/api/v1/session/start", sessionStart);
  });

  // 低层 run 开始时立即发一次 heartbeat，并由扩展定时续写。
  pi.on("agent_start", async () => {
    startAgentHeartbeat();
  });

  // agent_end 可能紧接自动 retry；此时只同步快照并保持活动状态。
  pi.on("agent_end", async (_event, ctx) => {
    if (!currentSessionId || !currentSessionInstanceId) return;
    currentModel = modelId(ctx);
    consumeIncrementalUsage(ctx);
    const identity = {
      sessionId: currentSessionId,
      instanceId: currentSessionInstanceId,
    };
    void post(
      "/api/v1/session/update",
      updateBody(identity, {
        active: true,
        activeAt: new Date().toISOString(),
      }),
    );
  });

  // settled 表示 retry、compaction 和 queued follow-up 均已结束。
  pi.on("agent_settled", async (_event, ctx) => {
    if (!currentSessionId || !currentSessionInstanceId) return;
    currentModel = modelId(ctx);
    consumeIncrementalUsage(ctx);
    const identity = {
      sessionId: currentSessionId,
      instanceId: currentSessionInstanceId,
    };
    stopAgentHeartbeat();
    await post(
      "/api/v1/session/update",
      updateBody(identity, {
        active: false,
        activeAt: new Date().toISOString(),
      }),
    );
  });

  // 空闲时手动 compact/tree 也可能生成计费用量，但不构成活动信号。
  pi.on("session_compact", async (_event, ctx) => {
    await postUsageSnapshot(ctx);
  });
  pi.on("session_tree", async (_event, ctx) => {
    await postUsageSnapshot(ctx);
  });

  pi.on("session_shutdown", async (event, ctx) => {
    if (!currentSessionId || !currentSessionInstanceId) return;
    const identity = {
      sessionId: currentSessionId,
      instanceId: currentSessionInstanceId,
    };
    stopAgentHeartbeat();
    currentModel = modelId(ctx);
    consumeIncrementalUsage(ctx);

    const body: SessionEndRequest = {
      session_id: identity.sessionId,
      session_instance_id: identity.instanceId,
      code_agent: CODE_AGENT,
      ended_at: new Date().toISOString(),
      ...usagePayload(),
      metadata: { shutdown_reason: event.reason },
    };

    currentSessionId = null;
    currentSessionInstanceId = null;
    await post("/api/v1/session/end", body);
  });
}
