#!/usr/bin/env python3
"""Claude Code セッション jsonl の prompt cache 費用を集計する。"""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

HOME = Path.home()
SOURCES = (HOME / ".claude-personal/projects", HOME / ".claude-emrd/projects")
DEFAULT_OUTPUT = HOME / ".cache/claude-session-state/llm-gateway/cache-sim-report.md"
MILLION = 1_000_000
RATES = {"read": 0.25, "write_5m": 12.5, "write_1h": 20.0, "input": 10.0, "output": 50.0}


@dataclass
class Request:
    timestamp: datetime
    message_id: str
    read: int
    write: int
    input: int
    output: int
    write_5m: int
    write_1h: int
    is_sidechain: bool | None
    gap_minutes: float | None = None
    rebuild: bool = False

    @property
    def prefix(self) -> int:
        return self.read + self.write + self.input


@dataclass
class Session:
    path: Path
    sid: str
    cwd: str
    size_bytes: int
    raw_entries: int
    requests: list[Request]
    rebuild_tokens_by_gap: dict[str, int] = field(default_factory=dict)
    rebuild_count_by_gap: dict[str, int] = field(default_factory=dict)

    @property
    def read(self) -> int:
        return sum(r.read for r in self.requests)

    @property
    def write(self) -> int:
        return sum(r.write for r in self.requests)

    @property
    def input(self) -> int:
        return sum(r.input for r in self.requests)

    @property
    def output(self) -> int:
        return sum(r.output for r in self.requests)

    @property
    def rebuild_write(self) -> int:
        return sum(r.write for r in self.requests if r.rebuild)

    @property
    def incremental_write(self) -> int:
        return self.write - self.rebuild_write

    @property
    def max_prefix(self) -> int:
        return max((r.prefix for r in self.requests), default=0)


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


def integer(value: Any) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) else 0


def discover_paths() -> list[Path]:
    paths: list[Path] = []
    for root in SOURCES:
        if root.exists():
            paths.extend(root.rglob("*.jsonl"))
    return sorted(paths)


def read_session(path: Path, cutoff: datetime) -> Session | None:
    # streaming の同一 message.id は後勝ちにする。
    latest: dict[str, tuple[int, dict[str, Any]]] = {}
    raw_entries = 0
    cwd = ""
    session_id = path.stem
    try:
        stat = path.stat()
        size_bytes = stat.st_size
        file_is_recent = stat.st_mtime >= cutoff.timestamp()
        has_recent_timestamp = False
        with path.open(encoding="utf-8", errors="replace") as stream:
            for line_number, line in enumerate(stream, 1):
                if '"usage"' not in line or '"assistant"' not in line:
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
                raw_entries += 1
                latest[message_id] = (line_number, record)
    except OSError as error:
        print(f"skip {path}: {error}", file=sys.stderr)
        return None

    if not file_is_recent and not has_recent_timestamp:
        return None

    requests: list[Request] = []
    for _, record in latest.values():
        message = record["message"]
        usage = message["usage"]
        cache_creation = usage.get("cache_creation") or {}
        timestamp = parse_timestamp(record["timestamp"])
        assert timestamp is not None
        requests.append(
            Request(
                timestamp=timestamp,
                message_id=message["id"],
                read=integer(usage.get("cache_read_input_tokens")),
                write=integer(usage.get("cache_creation_input_tokens")),
                input=integer(usage.get("input_tokens")),
                output=integer(usage.get("output_tokens")),
                write_5m=integer(cache_creation.get("ephemeral_5m_input_tokens")),
                write_1h=integer(cache_creation.get("ephemeral_1h_input_tokens")),
                is_sidechain=record.get("isSidechain") if isinstance(record.get("isSidechain"), bool) else None,
            )
        )
        cwd = record.get("cwd") or cwd
        session_id = record.get("sessionId") or session_id
    if not requests:
        return None

    requests.sort(key=lambda request: (request.timestamp, request.message_id))
    gap_tokens = {"≤5": 0, "5–60": 0, ">60": 0}
    gap_counts = {"≤5": 0, "5–60": 0, ">60": 0}
    previous: Request | None = None
    for request in requests:
        if previous is not None:
            request.gap_minutes = (request.timestamp - previous.timestamp).total_seconds() / 60
            request.rebuild = request.write > 20_000 and request.read < 0.2 * previous.prefix
            if request.rebuild:
                bucket = "≤5" if request.gap_minutes <= 5 else "5–60" if request.gap_minutes <= 60 else ">60"
                gap_counts[bucket] += 1
                gap_tokens[bucket] += request.write
        previous = request

    return Session(
        path=path,
        sid=str(session_id),
        cwd=str(cwd),
        size_bytes=size_bytes,
        raw_entries=raw_entries,
        requests=requests,
        rebuild_tokens_by_gap=gap_tokens,
        rebuild_count_by_gap=gap_counts,
    )


def usd(tokens: int | float, rate: float) -> float:
    return tokens / MILLION * rate


def actual_cost(session: Session) -> float:
    return (
        usd(session.read, RATES["read"])
        + usd(session.write, RATES["write_5m"])
        + usd(session.input, RATES["input"])
        + usd(session.output, RATES["output"])
    )


def simulated_costs(session: Session) -> tuple[float, float, float]:
    fixed = usd(session.read, RATES["read"]) + usd(session.input, RATES["input"]) + usd(session.output, RATES["output"])
    cost_b = fixed + usd(session.write, RATES["write_1h"])
    cost_a = fixed + usd(session.write, RATES["write_5m"])
    pessimistic_surcharge = 0.0

    for request in session.requests:
        gap = request.gap_minutes
        if not request.rebuild or gap is None:
            continue
        if gap <= 60:
            cost_b += usd(request.write, RATES["read"] - RATES["write_1h"])
        if 5 < gap <= 24 * 60:
            ping_count = 1 if gap <= 60 else math.floor(gap / 55)
            cost_a += usd(request.write, RATES["read"] - RATES["write_5m"])
            cost_a += usd(ping_count * request.prefix, RATES["read"])
            pessimistic_surcharge += usd(ping_count * request.prefix, RATES["write_1h"])

    return cost_b, cost_a, cost_a + pessimistic_surcharge


def short_path(value: str) -> str:
    github_prefix = str(HOME / ".local/share/repos/github.com") + "/"
    home_prefix = str(HOME) + "/"
    if value.startswith(github_prefix):
        return "gh:" + value[len(github_prefix) :]
    if value == str(HOME):
        return "~"
    if value.startswith(home_prefix):
        return "~/" + value[len(home_prefix) :]
    return value or "(unknown)"


def kind(path: Path) -> str:
    return "subagent" if "subagents" in path.parts else "main"


def money(value: float) -> str:
    return f"${value:,.2f}"


def mtok(value: int) -> str:
    return f"{value / MILLION:.3f}"


def gap_summary(session: Session) -> str:
    return ", ".join(
        f"{bucket}分 {session.rebuild_count_by_gap[bucket]}回/{mtok(session.rebuild_tokens_by_gap[bucket])}M"
        for bucket in ("≤5", "5–60", ">60")
    )


def effectiveness(session: Session) -> str:
    if session.write == 0 or session.rebuild_write == 0:
        return "rebuild write なし → A/B の置換効果なし"
    share = session.rebuild_write / session.write * 100
    mid = session.rebuild_tokens_by_gap["5–60"] / session.rebuild_write * 100
    long = session.rebuild_tokens_by_gap[">60"] / session.rebuild_write * 100
    if mid >= 50:
        verdict = "5〜60分 gap が中心 → A/B とも有効"
    elif long >= 50:
        verdict = "60分超 gap が中心 → B は効果薄、A は24時間以内のみ有効"
    else:
        verdict = "5分以内または分散した gap → idle 延長の効果は限定的"
    return f"rebuild が write の {share:.1f}%、5〜60分 {mid:.1f}%、60分超 {long:.1f}% → {verdict}"


def aggregate(sessions: list[Session]) -> Session:
    requests = [request for session in sessions for request in session.requests]
    total = Session(Path("-"), "TOTAL", "-", sum(s.size_bytes for s in sessions), sum(s.raw_entries for s in sessions), requests)
    total.rebuild_tokens_by_gap = {bucket: sum(s.rebuild_tokens_by_gap[bucket] for s in sessions) for bucket in ("≤5", "5–60", ">60")}
    total.rebuild_count_by_gap = {bucket: sum(s.rebuild_count_by_gap[bucket] for s in sessions) for bucket in ("≤5", "5–60", ">60")}
    return total


def group_trend(main: Session, subagent: Session) -> str:
    def write_share(session: Session) -> float:
        cache = session.read + session.write
        return session.write / cache * 100 if cache else 0

    def rebuild_share(session: Session) -> float:
        return session.rebuild_write / session.write * 100 if session.write else 0

    main_write = write_share(main)
    sub_write = write_share(subagent)
    main_rebuild = rebuild_share(main)
    sub_rebuild = rebuild_share(subagent)
    if main_write > sub_write and main_rebuild > sub_rebuild:
        return f"main は subagent より write 比率（{main_write:.1f}% vs {sub_write:.1f}%）と rebuild 比率（{main_rebuild:.1f}% vs {sub_rebuild:.1f}%）が高く、待機を挟む長寿命 session の影響が強い。subagent は連続実行による read 再利用が相対的に多い。"
    if main_write < sub_write and main_rebuild < sub_rebuild:
        return f"subagent は main より write 比率（{sub_write:.1f}% vs {main_write:.1f}%）と rebuild 比率（{sub_rebuild:.1f}% vs {main_rebuild:.1f}%）が高く、短命でも cache 再構築の影響が強い。"
    return f"write 比率は main {main_write:.1f}% / subagent {sub_write:.1f}%、rebuild 比率は main {main_rebuild:.1f}% / subagent {sub_rebuild:.1f}%で、2指標の傾向は一致しない。"


def render(sessions: list[Session], cutoff: datetime, generated: datetime) -> str:
    total = aggregate(sessions)
    grouped_sessions = {
        label: [session for session in sessions if kind(session.path) == label]
        for label in ("main", "subagent")
    }
    grouped = {label: aggregate(items) for label, items in grouped_sessions.items()}
    actual = actual_cost(total)
    sim_b, sim_a, sim_a_bad = simulated_costs(total)
    cache_total = total.read + total.write
    read_share = total.read / cache_total * 100 if cache_total else 0
    write_share = total.write / cache_total * 100 if cache_total else 0
    rebuild_cost = usd(total.rebuild_write, RATES["write_5m"])
    raw = total.raw_entries
    deduped = len(total.requests)
    sidechain_mismatches = [
        request
        for session in sessions
        for request in session.requests
        if request.is_sidechain is None or request.is_sidechain != (kind(session.path) == "subagent")
    ]
    mismatch_sessions = sum(
        any(request.is_sidechain is None or request.is_sidechain != (kind(session.path) == "subagent") for request in session.requests)
        for session in sessions
    )

    lines = [
        "# Prompt cache 費用と TTL シミュレーション",
        "",
        "## 全体サマリ",
        "",
        f"- 対象: {len(sessions)} sessions / assistant usage {raw:,} entries → message.id dedupe 後 {deduped:,} requests（{raw - deduped:,} 件除外）",
        f"- 合計費用: 実績 {money(actual)} / B（常時1h）{money(sim_b)}（差額 {money(sim_b - actual)}） / A（α=0）{money(sim_a)}（差額 {money(sim_a - actual)}） / A（α悲観）{money(sim_a_bad)}（差額 {money(sim_a_bad - actual)}）",
        f"- cache token 比率: read {read_share:.1f}%（{mtok(total.read)}M） / write {write_share:.1f}%（{mtok(total.write)}M）",
        f"- rebuild: {sum(total.rebuild_count_by_gap.values()):,} 回 / {mtok(total.rebuild_write)}M write tokens / 実績5m write費 {money(rebuild_cost)}",
        f"- rebuild gap: {gap_summary(total)}",
        f"- 判定: {effectiveness(total)}",
        f"- main/subagent 分類: jsonl path を基準にし、dedupe 後 entry の isSidechain と照合。不一致 {len(sidechain_mismatches):,} requests / {mismatch_sessions} sessions。",
        "",
        "## main / subagent 別",
        "",
        "| 群 | sessions | requests | rt M | wt M | it M | ot M | rc | wc | ic | oc | read:write | rebuild/write | rebuild gap（回数/write M） | B 差額 | A α=0 差額 | A α悲観差額 |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|",
    ]
    for label in ("main", "subagent"):
        group = grouped[label]
        group_actual = actual_cost(group)
        group_b, group_a, group_bad = simulated_costs(group)
        group_cache = group.read + group.write
        group_read_share = group.read / group_cache * 100 if group_cache else 0
        group_write_share = group.write / group_cache * 100 if group_cache else 0
        group_rebuild_share = group.rebuild_write / group.write * 100 if group.write else 0
        lines.append(
            f"| {label} | {len(grouped_sessions[label])} | {len(group.requests):,} | {mtok(group.read)} | {mtok(group.write)} | {mtok(group.input)} | {mtok(group.output)} | "
            f"{money(usd(group.read, RATES['read']))} | {money(usd(group.write, RATES['write_5m']))} | {money(usd(group.input, RATES['input']))} | {money(usd(group.output, RATES['output']))} | "
            f"{group_read_share:.1f}%:{group_write_share:.1f}% | {group_rebuild_share:.1f}% | {gap_summary(group)} | {money(group_b - group_actual)} | {money(group_a - group_actual)} | {money(group_bad - group_actual)} |"
        )
    lines += [
        "",
        f"- 群別傾向: {group_trend(grouped['main'], grouped['subagent'])}",
        f"- main: {effectiveness(grouped['main'])}",
        f"- subagent: {effectiveness(grouped['subagent'])}",
        "",
        "## 集計条件",
        "",
        f"- 対象ファイル: mtime または entry timestamp が {cutoff.isoformat()} 以降（生成 {generated.isoformat()}）。選定した jsonl はファイル全体を集計。",
        "- 会話本文は参照せず、assistant entry の usage と指定メタデータのみ読む。",
        "- 同一ファイル内の同一 message.id は最終 entry を採用する。ファイルを session 単位とする。",
        "- 費用はモデルに関係なく Fable 5.1 相当: read $0.25/M、5m write $12.5/M、input $10/M、output $50/M。実績の write は cache_creation 内訳によらず全量を5m単価で計算する。",
        "- rebuild は前リクエストが存在する場合だけ判定する。初回 request は比較元がないため incremental 扱い。",
        "- A の α悲観値は、α=0 費用に各 replay ping の prefix 全量 × $20/M を追加した上限。24時間を超える gap は replay せず実績 rebuild のまま。",
        "",
        "## セッション別費用",
        "",
        "| sid | kind | cwd | jsonl MB | requests | rt M | wt M | it M | ot M | rc | wc | ic | oc | total | max prefix K |",
        "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    ordered = sorted(sessions, key=actual_cost, reverse=True)
    for session in ordered:
        lines.append(
            f"| {session.sid[:8]} | {kind(session.path)} | {short_path(session.cwd)} | {session.size_bytes / MILLION:.2f} | {len(session.requests):,} | "
            f"{mtok(session.read)} | {mtok(session.write)} | {mtok(session.input)} | {mtok(session.output)} | "
            f"{money(usd(session.read, RATES['read']))} | {money(usd(session.write, RATES['write_5m']))} | "
            f"{money(usd(session.input, RATES['input']))} | {money(usd(session.output, RATES['output']))} | "
            f"{money(actual_cost(session))} | {session.max_prefix / 1000:.1f} |"
        )
    lines.append(
        f"| **TOTAL** | — | — | **{total.size_bytes / MILLION:.2f}** | **{len(total.requests):,}** | **{mtok(total.read)}** | **{mtok(total.write)}** | "
        f"**{mtok(total.input)}** | **{mtok(total.output)}** | **{money(usd(total.read, RATES['read']))}** | "
        f"**{money(usd(total.write, RATES['write_5m']))}** | **{money(usd(total.input, RATES['input']))}** | "
        f"**{money(usd(total.output, RATES['output']))}** | **{money(actual)}** | **{total.max_prefix / 1000:.1f}** |"
    )

    lines += [
        "",
        "## 1h TTL シミュレーション",
        "",
        "| sid | rebuild | rebuild wt M | incremental wt M | rebuild gap（回数/write M） | 実績 | B 常時1h | B 差額 | A α=0 | A 差額 | A α悲観 | 効きそうな点 |",
        "|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---|",
    ]
    for session in ordered:
        cost = actual_cost(session)
        b, a, a_bad = simulated_costs(session)
        lines.append(
            f"| {session.sid[:8]} | {sum(session.rebuild_count_by_gap.values())} | {mtok(session.rebuild_write)} | {mtok(session.incremental_write)} | "
            f"{gap_summary(session)} | {money(cost)} | {money(b)} | {money(b - cost)} | {money(a)} | {money(a - cost)} | {money(a_bad)} | {effectiveness(session)} |"
        )
    lines.append(
        f"| **TOTAL** | **{sum(total.rebuild_count_by_gap.values())}** | **{mtok(total.rebuild_write)}** | **{mtok(total.incremental_write)}** | "
        f"**{gap_summary(total)}** | **{money(actual)}** | **{money(sim_b)}** | **{money(sim_b - actual)}** | "
        f"**{money(sim_a)}** | **{money(sim_a - actual)}** | **{money(sim_a_bad)}** | **{effectiveness(total)}** |"
    )

    lines += [
        "",
        "## リクエスト間隔",
        "",
        "各行は session 内で timestamp 順。先頭 request の gap は `—`。`R` は rebuild、`I` は incremental。",
        "",
    ]
    for session in ordered:
        gaps = ["—" if request.gap_minutes is None else f"{request.gap_minutes:.1f}{'R' if request.rebuild else 'I'}" for request in session.requests]
        lines.append(f"- `{session.sid[:8]}` ({kind(session.path)}): " + ", ".join(gaps))
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--days", type=float, default=7, help="集計対象の日数（default: 7）")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT, help="Markdown 出力先")
    args = parser.parse_args()
    if args.days <= 0:
        parser.error("--days は正数で指定してください")

    generated = datetime.now(timezone.utc)
    cutoff = generated - timedelta(days=args.days)
    sessions = [session for path in discover_paths() if (session := read_session(path, cutoff)) is not None]
    report = render(sessions, cutoff, generated)
    output = args.output.expanduser()
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(f".{output.name}.tmp")
    temporary.write_text(report, encoding="utf-8")
    os.replace(temporary, output)
    print(f"wrote {output}: {len(sessions)} sessions, {sum(len(s.requests) for s in sessions):,} requests")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
