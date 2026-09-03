#!/usr/bin/env python3
"""main session の idle gap から keepalive horizon の費用を比較する。"""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import os
import sys
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Iterable

HOME = Path.home()
DEFAULT_OUTPUT = HOME / ".cache/claude-session-state/llm-gateway/keepalive-horizon-report.md"
JST = timezone(timedelta(hours=9))
MODEL_RATES = {
    "claude-fable-5-1": (0.25, 20.0),
    "claude-mythos-5-1": (0.25, 20.0),
    "claude-fable-5": (1.0, 20.0),
    "claude-mythos-5": (1.0, 20.0),
    "claude-opus-5": (0.5, 10.0),
    "claude-sonnet-5": (0.2, 4.0),
}
BRANCH_HOURS = {"5.1系": 80 * 55 / 60, "その他": 20 * 55 / 60}
HORIZONS = range(97)
MILLION = 1_000_000


@dataclass(frozen=True)
class Request:
    timestamp: datetime
    model: str
    prefix: int


@dataclass(frozen=True)
class Gap:
    namespace: str
    model: str
    model_family: str
    period: str
    minutes: float
    prefix: int


@dataclass
class CurvePoint:
    cost: float = 0.0
    ping_cost: float = 0.0
    rebuild_cost: float = 0.0
    pings: int = 0
    rebuilds: int = 0


def load_cache_module() -> Any:
    path = Path(__file__).with_name("cache-cost-sim.py")
    spec = importlib.util.spec_from_file_location("cache_cost_sim", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"module を読み込めません: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def integer(value: Any) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) else 0


def namespace(path: Path) -> str:
    parts = path.parts
    if ".claude-personal" in parts:
        return "personal"
    if ".claude-emrd" in parts:
        return "emrd"
    return "unknown"


def model_family(model: str) -> str:
    lowered = model.lower()
    return "5.1系" if "fable-5-1" in lowered or "mythos-5-1" in lowered else "その他"


def model_rates(model: str) -> tuple[float, float] | None:
    lowered = model.lower()
    for model_id, rates in MODEL_RATES.items():
        if lowered == model_id or lowered.startswith(model_id + "-"):
            return rates
    return None


def period(timestamp: datetime) -> str:
    local = timestamp.astimezone(JST)
    if local.weekday() >= 5:
        return "週末"
    if 9 <= local.hour < 18:
        return "平日昼"
    return "平日夜"


def read_requests(path: Path, cutoff: datetime) -> list[Request] | None:
    try:
        stat = path.stat()
        file_is_recent = stat.st_mtime >= cutoff.timestamp()
        has_recent_timestamp = False
        latest: dict[str, tuple[int, dict[str, Any]]] = {}
        with path.open(encoding="utf-8", errors="replace") as stream:
            for line_number, line in enumerate(stream, 1):
                if '"assistant"' not in line or '"usage"' not in line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if record.get("type") != "assistant":
                    continue
                message = record.get("message") or {}
                usage = message.get("usage")
                message_id = message.get("id")
                timestamp = parse_timestamp(record.get("timestamp"))
                if not isinstance(usage, dict) or not isinstance(message_id, str) or timestamp is None:
                    continue
                if timestamp >= cutoff:
                    has_recent_timestamp = True
                latest[message_id] = (line_number, record)
    except OSError as error:
        print(f"skip {path}: {error}", file=sys.stderr)
        return None
    if not file_is_recent and not has_recent_timestamp:
        return None

    requests = []
    for _, record in latest.values():
        message = record["message"]
        usage = message["usage"]
        timestamp = parse_timestamp(record["timestamp"])
        assert timestamp is not None
        requests.append(
            Request(
                timestamp=timestamp,
                model=str(message.get("model") or "unknown"),
                prefix=integer(usage.get("cache_read_input_tokens"))
                + integer(usage.get("cache_creation_input_tokens"))
                + integer(usage.get("input_tokens")),
            )
        )
    requests.sort(key=lambda request: (request.timestamp, request.model))
    return requests


def parse_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def collect_gaps(paths: Iterable[Path], cutoff: datetime) -> tuple[list[Gap], int, int]:
    gaps: list[Gap] = []
    sessions = 0
    requests_count = 0
    for path in paths:
        if "subagents" in path.parts:
            continue
        requests = read_requests(path, cutoff)
        if not requests:
            continue
        sessions += 1
        requests_count += len(requests)
        ns = namespace(path)
        for previous, current in zip(requests, requests[1:]):
            gap_minutes = (current.timestamp - previous.timestamp).total_seconds() / 60
            if gap_minutes < 60:
                continue
            gaps.append(
                Gap(
                    namespace=ns,
                    model=previous.model,
                    model_family=model_family(previous.model),
                    period=period(previous.timestamp),
                    minutes=gap_minutes,
                    prefix=previous.prefix,
                )
            )
    return gaps, sessions, requests_count


def gap_cost(gap: Gap, horizon: int) -> CurvePoint:
    rates = model_rates(gap.model)
    if rates is None:
        raise ValueError(f"単価未定義モデル: {gap.model}")
    read_rate, w1h_rate = rates
    if horizon == 0:
        rebuild = gap.prefix / MILLION * w1h_rate
        return CurvePoint(cost=rebuild, rebuild_cost=rebuild, rebuilds=1)
    active_minutes = min(gap.minutes, horizon * 60)
    pings = math.floor(active_minutes / 55)
    ping_cost = pings * gap.prefix / MILLION * read_rate
    rebuilds = int(gap.minutes > horizon * 60)
    rebuild_cost = rebuilds * gap.prefix / MILLION * w1h_rate
    return CurvePoint(
        cost=ping_cost + rebuild_cost,
        ping_cost=ping_cost,
        rebuild_cost=rebuild_cost,
        pings=pings,
        rebuilds=rebuilds,
    )


def curve(gaps: Iterable[Gap]) -> dict[int, CurvePoint]:
    result = {horizon: CurvePoint() for horizon in HORIZONS}
    for gap in gaps:
        for horizon in HORIZONS:
            item = gap_cost(gap, horizon)
            total = result[horizon]
            total.cost += item.cost
            total.ping_cost += item.ping_cost
            total.rebuild_cost += item.rebuild_cost
            total.pings += item.pings
            total.rebuilds += item.rebuilds
    return result


def optimum(points: dict[int, CurvePoint]) -> int:
    return min(points, key=lambda horizon: (points[horizon].cost, horizon))


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] * (upper - position) + ordered[upper] * (position - lower)


def money(value: float) -> str:
    return f"${value:,.2f}"


def group_rows(gaps: list[Gap]) -> list[tuple[str, str, list[Gap]]]:
    rows = []
    for ns in ("personal", "emrd"):
        for family in ("5.1系", "その他"):
            rows.append((ns, family, [gap for gap in gaps if gap.namespace == ns and gap.model_family == family]))
    return rows


def render(gaps: list[Gap], excluded: list[Gap], sessions: int, requests: int, cutoff: datetime, generated: datetime) -> str:
    all_curve = curve(gaps)
    all_best = optimum(all_curve)
    baseline = all_curve[0].cost
    lines = [
        "# Keepalive Horizon 費用シミュレーション",
        "",
        "## kawaz の最適解",
        "",
        "| namespace | モデル系統 | idle gaps | baseline H=0 | 推奨 H* | 分岐時間 | H*/分岐 | H* 費用 | 削減額 |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for ns, family, group in group_rows(gaps):
        points = curve(group)
        best = optimum(points)
        branch = BRANCH_HOURS[family]
        lines.append(
            f"| {ns} | {family} | {len(group)} | {money(points[0].cost)} | {best}h | {branch:.1f}h | {best / branch:.2f} | {money(points[best].cost)} | {money(points[best].cost - points[0].cost)} |"
        )
    lines += [
        "",
        f"- 全体では H*={all_best}h、baseline {money(baseline)} → {money(all_curve[all_best].cost)}（差額 {money(all_curve[all_best].cost - baseline)}）。",
        "- 分岐回数がモデル系統で 80 回と 20 回に分かれるため、単一の絶対時間はモデル間で経済的意味が揃わない。設定値は分岐時間に対する比率を正規化して持ち、運用上の上限を絶対時間で設ける形が自然。",
        f"- 単価未定義のため費用曲線から除外: {len(excluded)} gaps（" + ", ".join(f"{model} {count}件" for model, count in sorted(Counter(gap.model for gap in excluded).items())) + "）。",
        "",
        "## 曜日・時間帯別",
        "",
        "gap 開始時刻を JST で分類。平日昼は月〜金 09:00–18:00、平日夜はそれ以外、週末は土日。`n < 10` はサンプルが少ない。",
        "",
        "| 区分 | namespace | モデル系統 | n | p50 gap | p90 gap | H* | baseline | H* 費用 | 差額 | 注記 |",
        "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---|",
    ]
    for period_name in ("平日昼", "平日夜", "週末"):
        for ns, family, group in group_rows([gap for gap in gaps if gap.period == period_name]):
            points = curve(group)
            best = optimum(points)
            durations = [gap.minutes / 60 for gap in group]
            note = "n が少ない" if len(group) < 10 else ""
            lines.append(
                f"| {period_name} | {ns} | {family} | {len(group)} | {percentile(durations, .5):.1f}h | {percentile(durations, .9):.1f}h | {best}h | {money(points[0].cost)} | {money(points[best].cost)} | {money(points[best].cost - points[0].cost)} | {note} |"
            )
    lines += [
        "",
        "## H 別週次費用",
        "",
        "| namespace | モデル系統 | H | total | ping | rebuild | pings | rebuilds | baseline 差額 |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for ns, family, group in group_rows(gaps):
        points = curve(group)
        base = points[0].cost
        for horizon in HORIZONS:
            point = points[horizon]
            lines.append(
                f"| {ns} | {family} | {horizon}h | {money(point.cost)} | {money(point.ping_cost)} | {money(point.rebuild_cost)} | {point.pings:,} | {point.rebuilds:,} | {money(point.cost - base)} |"
            )
    lines += [
        "",
        "## Gap 分布",
        "",
        "| namespace | モデル系統 | 区分 | n | prefix M | p50 | p75 | p90 | p95 | max |",
        "|---|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for ns, family, group in group_rows(gaps):
        for period_name in ("全体", "平日昼", "平日夜", "週末"):
            selected = group if period_name == "全体" else [gap for gap in group if gap.period == period_name]
            durations = [gap.minutes / 60 for gap in selected]
            lines.append(
                f"| {ns} | {family} | {period_name} | {len(selected)} | {sum(gap.prefix for gap in selected) / MILLION:.3f} | {percentile(durations, .5):.1f}h | {percentile(durations, .75):.1f}h | {percentile(durations, .9):.1f}h | {percentile(durations, .95):.1f}h | {max(durations, default=0):.1f}h |"
            )
    lines += [
        "",
        "## 集計条件",
        "",
        f"- 対象: {sessions} main sessions / {requests:,} dedupe 後 requests / {len(gaps):,} idle gaps。生成 {generated.isoformat()}。",
        f"- JSONL は cache-cost-sim と同じく、mtime または entry timestamp が {cutoff.isoformat()} 以降ならファイル全体を対象にする。",
        "- 同一 JSONL 内の message.id は最後の entry を採用し、会話本文は読まない。",
        "- namespace は JSONL の config root（.claude-personal / .claude-emrd）で判定する。cwd は namespace 判定に使わない。",
        "- gap のモデル系統は、保持中 cache を作った直前リクエストの model で判定する。Fable 5.1 / Mythos 5.1 のみ 5.1系。",
        "- gap 開始は直前リクエストの timestamp。prefix P も直前リクエストの read + creation + input。",
        "- ping は gap 開始から55分ごと。回数は floor(min(gap, H) / 55分)。gap > H のときだけ末尾で全量 rebuild。H=0 は全 gap rebuild。",
        "- 費用は直前リクエストのモデル単価を使用。Fable 5.1 / Mythos 5.1 は read $0.25・1h write $20、Fable 5 は $1/$20、Opus 5 は $0.5/$10、Sonnet 5 は $0.2/$4（すべて /MTok）。",
        "- 5.1系の分岐回数は80回。その他の定義済みモデルは read が input の0.1倍、1h writeが2倍なので20回。",
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--days", type=float, default=7, help="対象ファイル判定の日数（default: 7）")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT, help="Markdown 出力先")
    args = parser.parse_args()
    if args.days <= 0:
        parser.error("--days は正数で指定してください")

    generated = datetime.now(timezone.utc)
    cutoff = generated - timedelta(days=args.days)
    cache_module = load_cache_module()
    all_gaps, sessions, requests = collect_gaps(cache_module.discover_paths(), cutoff)
    gaps = [gap for gap in all_gaps if model_rates(gap.model) is not None]
    excluded = [gap for gap in all_gaps if model_rates(gap.model) is None]
    report = render(gaps, excluded, sessions, requests, cutoff, generated)
    output = args.output.expanduser()
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(f".{output.name}.tmp")
    temporary.write_text(report, encoding="utf-8")
    os.replace(temporary, output)
    print(f"wrote {output}: {sessions} main sessions, {len(gaps):,} idle gaps")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
