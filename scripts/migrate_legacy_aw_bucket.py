#!/usr/bin/env python3
"""将旧版 aw-watcher-agent 单 bucket 导出转换为新版双 bucket 并回填 ActivityWatch。"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


EVENT_BUCKET_TYPE = "app.editor.activity"
SUM_BUCKET_TYPE = "app.code-agent.summary"
DEFAULT_INPUT = "aw-bucket-export_aw-watcher-agent_arch.json"
DEFAULT_CLIENT = "aw-watcher-agent-migration"


JsonObject = dict[str, Any]


@dataclass
class SessionStats:
    key: tuple[str, str]
    active_events: list[JsonObject] = field(default_factory=list)
    completed_events: list[JsonObject] = field(default_factory=list)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "把旧版 aw-watcher-agent_<hostname> 导出拆分为 "
            "aw-watcher-agent-event_<hostname> 和 aw-watcher-agent-sum_<hostname>。"
        )
    )
    parser.add_argument(
        "input",
        nargs="?",
        default=DEFAULT_INPUT,
        help=f"旧版 ActivityWatch bucket export JSON，默认 {DEFAULT_INPUT}",
    )
    parser.add_argument("--host", default="localhost", help="ActivityWatch host，默认 localhost")
    parser.add_argument("--port", type=int, default=5600, help="ActivityWatch port，默认 5600")
    parser.add_argument(
        "--hostname",
        help="目标 bucket hostname；默认优先使用导出 bucket.hostname，再从 bucket id 推断",
    )
    parser.add_argument(
        "--client",
        default=DEFAULT_CLIENT,
        help=f"导入 bucket 的 client 字段，默认 {DEFAULT_CLIENT}",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="仅写出转换后的 export JSON 到文件；可与 --apply 同时使用",
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="实际 POST 到 AW /api/0/import；不指定时只预览统计",
    )
    parser.add_argument(
        "--skip-event-bucket",
        action="store_true",
        help="只导入 sum bucket，不回填新版 event bucket",
    )
    parser.add_argument(
        "--skip-abandoned",
        action="store_true",
        help="不为只有 active、没有 completed 的旧 session 生成 abandoned summary",
    )
    parser.add_argument(
        "--abandoned-reason",
        default="migration",
        help='迁移生成 abandoned summary 时写入的 reason，默认 "migration"',
    )
    return parser.parse_args()


def parse_aw_time(value: str) -> datetime:
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as err:
        raise ValueError(f"无法解析时间戳 {value!r}") from err
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def format_aw_time(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).isoformat(timespec="microseconds").replace("+00:00", "Z")


def event_start(event: JsonObject) -> datetime:
    return parse_aw_time(str(event["timestamp"]))


def event_duration(event: JsonObject) -> float:
    duration = event.get("duration", 0)
    return float(duration or 0)


def event_end(event: JsonObject) -> datetime:
    return event_start(event) + timedelta(seconds=event_duration(event))


def positive_duration(seconds: float) -> float:
    return max(0.0, seconds)


def project_name_from_dir(project_dir: str) -> str:
    stripped = project_dir.rstrip("/\\")
    if not stripped:
        return "root"
    return Path(stripped).name or stripped


def detect_source_bucket(export_data: JsonObject) -> JsonObject:
    buckets = export_data.get("buckets")
    if not isinstance(buckets, dict) or not buckets:
        raise ValueError("输入 JSON 中没有 buckets")

    candidates = [
        bucket
        for bucket in buckets.values()
        if isinstance(bucket, dict)
        and bucket.get("type") == EVENT_BUCKET_TYPE
        and str(bucket.get("id", "")).startswith("aw-watcher-agent_")
    ]
    if len(candidates) == 1:
        return candidates[0]
    if len(buckets) == 1:
        bucket = next(iter(buckets.values()))
        if isinstance(bucket, dict):
            return bucket
    raise ValueError("无法唯一识别旧版 aw-watcher-agent bucket，请只传入单 bucket export")


def detect_hostname(bucket: JsonObject, override: str | None) -> str:
    if override:
        return override
    hostname = bucket.get("hostname")
    if isinstance(hostname, str) and hostname:
        return hostname
    bucket_id = str(bucket.get("id", ""))
    prefix = "aw-watcher-agent_"
    if bucket_id.startswith(prefix) and len(bucket_id) > len(prefix):
        return bucket_id[len(prefix) :]
    raise ValueError("无法确定 hostname，请通过 --hostname 指定")


def session_key(data: JsonObject) -> tuple[str, str] | None:
    code_agent = data.get("code_agent")
    session_id = data.get("session_id")
    if isinstance(code_agent, str) and isinstance(session_id, str):
        return code_agent, session_id
    return None


def sorted_events(events: list[JsonObject]) -> list[JsonObject]:
    return sorted(events, key=lambda event: event_start(event))


def single_or_multiple(values: list[str]) -> str:
    unique = sorted(set(values))
    return unique[0] if len(unique) == 1 else "multiple"


def session_item(data: JsonObject) -> JsonObject:
    project_dir = str(data.get("project_dir") or data.get("file") or "")
    project = str(data.get("project") or project_name_from_dir(project_dir))
    item: JsonObject = {
        "code_agent": data.get("code_agent"),
        "session_id": data.get("session_id"),
        "project": project,
        "project_dir": project_dir,
    }
    if isinstance(data.get("model"), str):
        item["model"] = data["model"]
    return item


def convert_active_event(event: JsonObject) -> JsonObject:
    data = event.get("data") or {}
    if not isinstance(data, dict):
        data = {}
    item = session_item(data)
    project = str(item["project"])
    project_dir = str(item["project_dir"])
    code_agent = str(item["code_agent"])

    converted_data: JsonObject = {
        "status": "active",
        "language": "code-agent",
        "project": project,
        "file": project_dir,
        "active_session_count": 1,
        "code_agents": [code_agent],
        "projects": [project],
        "project_dirs": [project_dir],
        "sessions": [item],
    }
    if isinstance(data.get("model"), str):
        converted_data["models"] = [data["model"]]

    return {
        "id": None,
        "timestamp": event["timestamp"],
        "duration": event_duration(event),
        "data": converted_data,
    }


def session_timing(stats: SessionStats, fallback_event: JsonObject) -> tuple[datetime, datetime, float]:
    if stats.active_events:
        active_events = sorted_events(stats.active_events)
        first_active = event_start(active_events[0])
        last_active = max(event_end(event) for event in active_events)
        active_duration = sum(positive_duration(event_duration(event)) for event in active_events)
        return first_active, last_active, active_duration

    timestamp = event_start(fallback_event)
    return timestamp, timestamp, 0.0


def started_at_from_data(data: JsonObject, fallback: datetime) -> datetime:
    value = data.get("started_at")
    if isinstance(value, str):
        try:
            return parse_aw_time(value)
        except ValueError:
            pass
    return fallback


def build_summary_data(
    *,
    stats: SessionStats,
    source_event: JsonObject,
    status: str,
    reason: str | None,
) -> tuple[JsonObject, datetime, float]:
    source_data = source_event.get("data") or {}
    if not isinstance(source_data, dict):
        source_data = {}

    first_active, last_active, active_duration = session_timing(stats, source_event)
    ended_at = event_start(source_event) if status == "completed" else last_active
    started_at = started_at_from_data(source_data, first_active)

    project_dir = str(source_data.get("project_dir") or source_data.get("file") or "")
    project = str(source_data.get("project") or project_name_from_dir(project_dir))
    code_agent, session_id = stats.key

    data = dict(source_data)
    data.update(
        {
            "project": project,
            "file": project_dir,
            "language": "code-agent",
            "status": status,
            "summary_id": summary_id(
                code_agent=code_agent,
                session_id=session_id,
                status=status,
                reason=reason,
                started_at=started_at,
                ended_at=ended_at,
            ),
            "session_id": session_id,
            "code_agent": code_agent,
            "project_dir": project_dir,
            "started_at": format_aw_time(started_at),
            "ended_at": format_aw_time(ended_at),
            "first_active_at": format_aw_time(first_active),
            "last_active_at": format_aw_time(last_active),
            "active_duration_seconds": active_duration,
            "wall_duration_seconds": positive_duration((ended_at - started_at).total_seconds()),
        }
    )
    if reason:
        data["reason"] = reason
    elif "reason" in data:
        del data["reason"]

    return data, first_active, active_duration


def summary_id(
    *,
    code_agent: str,
    session_id: str,
    status: str,
    reason: str | None,
    started_at: datetime,
    ended_at: datetime,
) -> str:
    return ":".join(
        [
            code_agent,
            session_id,
            status,
            reason or "none",
            format_aw_time(started_at),
            format_aw_time(ended_at),
        ]
    )


def convert_completed_summary(stats: SessionStats, event: JsonObject) -> JsonObject:
    data, timestamp, duration = build_summary_data(
        stats=stats,
        source_event=event,
        status="completed",
        reason=None,
    )
    return {
        "id": None,
        "timestamp": format_aw_time(timestamp),
        "duration": duration,
        "data": data,
    }


def convert_abandoned_summary(stats: SessionStats, reason: str) -> JsonObject | None:
    if not stats.active_events:
        return None
    source_event = sorted_events(stats.active_events)[-1]
    data, timestamp, duration = build_summary_data(
        stats=stats,
        source_event=source_event,
        status="abandoned",
        reason=reason,
    )
    return {
        "id": None,
        "timestamp": format_aw_time(timestamp),
        "duration": duration,
        "data": data,
    }


def group_sessions(events: list[JsonObject]) -> dict[tuple[str, str], SessionStats]:
    sessions: dict[tuple[str, str], SessionStats] = {}
    for event in events:
        data = event.get("data")
        if not isinstance(data, dict):
            continue
        key = session_key(data)
        if key is None:
            continue
        stats = sessions.setdefault(key, SessionStats(key=key))
        if data.get("status") == "completed":
            stats.completed_events.append(event)
        elif data.get("status") == "active":
            stats.active_events.append(event)
    return sessions


def bucket_time_range(events: list[JsonObject]) -> tuple[str | None, str | None]:
    if not events:
        return None, None
    start = min(event_start(event) for event in events)
    end = max(event_end(event) for event in events)
    return format_aw_time(start), format_aw_time(end)


def make_bucket(
    *,
    bucket_id: str,
    bucket_type: str,
    hostname: str,
    client: str,
    created: str | None,
    events: list[JsonObject],
) -> JsonObject:
    start, end = bucket_time_range(events)
    return {
        "id": bucket_id,
        "type": bucket_type,
        "client": client,
        "hostname": hostname,
        "created": created,
        "data": {},
        "metadata": {"start": start, "end": end},
        "events": sorted_events(events),
        "last_updated": None,
    }


def convert_export(
    export_data: JsonObject,
    *,
    hostname: str | None,
    client: str,
    include_event_bucket: bool,
    include_abandoned: bool,
    abandoned_reason: str,
) -> tuple[JsonObject, JsonObject]:
    source_bucket = detect_source_bucket(export_data)
    target_hostname = detect_hostname(source_bucket, hostname)
    source_events = source_bucket.get("events") or []
    if not isinstance(source_events, list):
        raise ValueError("旧 bucket 的 events 不是数组")

    sessions = group_sessions(source_events)
    active_events = [
        convert_active_event(event)
        for event in source_events
        if isinstance(event.get("data"), dict) and event["data"].get("status") == "active"
    ]

    summary_events: list[JsonObject] = []
    completed_count_by_session: defaultdict[tuple[str, str], int] = defaultdict(int)
    for key, stats in sessions.items():
        for completed_event in sorted_events(stats.completed_events):
            summary_events.append(convert_completed_summary(stats, completed_event))
            completed_count_by_session[key] += 1
        if include_abandoned and not stats.completed_events:
            abandoned = convert_abandoned_summary(stats, abandoned_reason)
            if abandoned is not None:
                summary_events.append(abandoned)

    buckets: dict[str, JsonObject] = {}
    created = source_bucket.get("created")
    if not isinstance(created, str):
        created = None

    event_bucket_id = f"aw-watcher-agent-event_{target_hostname}"
    sum_bucket_id = f"aw-watcher-agent-sum_{target_hostname}"
    if include_event_bucket:
        buckets[event_bucket_id] = make_bucket(
            bucket_id=event_bucket_id,
            bucket_type=EVENT_BUCKET_TYPE,
            hostname=target_hostname,
            client=client,
            created=created,
            events=active_events,
        )
    buckets[sum_bucket_id] = make_bucket(
        bucket_id=sum_bucket_id,
        bucket_type=SUM_BUCKET_TYPE,
        hostname=target_hostname,
        client=client,
        created=created,
        events=summary_events,
    )

    stats = {
        "source_bucket_id": source_bucket.get("id"),
        "hostname": target_hostname,
        "source_events": len(source_events),
        "sessions": len(sessions),
        "active_events": len(active_events),
        "completed_summaries": sum(completed_count_by_session.values()),
        "abandoned_summaries": sum(
            1 for event in summary_events if event.get("data", {}).get("status") == "abandoned"
        ),
        "target_buckets": {bucket_id: len(bucket["events"]) for bucket_id, bucket in buckets.items()},
    }
    return {"buckets": buckets}, stats


def post_import(base_url: str, payload: JsonObject) -> None:
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(
        f"{base_url.rstrip('/')}/api/0/import/",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            response.read()
    except urllib.error.HTTPError as err:
        detail = err.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"AW import 失败：HTTP {err.code}: {detail}") from err
    except urllib.error.URLError as err:
        raise RuntimeError(f"无法连接 AW：{err}") from err


def main() -> int:
    args = parse_args()
    input_path = Path(args.input)
    try:
        export_data = json.loads(input_path.read_text(encoding="utf-8"))
        converted, stats = convert_export(
            export_data,
            hostname=args.hostname,
            client=args.client,
            include_event_bucket=not args.skip_event_bucket,
            include_abandoned=not args.skip_abandoned,
            abandoned_reason=args.abandoned_reason,
        )
    except Exception as err:
        print(f"转换失败：{err}", file=sys.stderr)
        return 1

    if args.output:
        args.output.write_text(
            json.dumps(converted, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )

    print(json.dumps(stats, ensure_ascii=False, indent=2, sort_keys=True))

    if not args.apply:
        print("dry-run：未写入 ActivityWatch。确认后加 --apply 执行回填。")
        return 0

    base_url = f"http://{args.host}:{args.port}"
    try:
        post_import(base_url, converted)
    except RuntimeError as err:
        print(str(err), file=sys.stderr)
        return 1
    print(f"已回填到 {base_url}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
