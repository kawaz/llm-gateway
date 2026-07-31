#!/usr/bin/env python3
"""Claude Code セッション jsonl から過去日のトークン集計を作る一発スクリプト。

gateway の日次集計 (DR-0011) は稼働開始日以降しか持たないので、それ以前の分を
~/.claude-*/projects/**/*.jsonl の usage から遡って集計し、stats 置き場に
`YYYY-MM-DD.backfill.json` として書く。reader は writer 名を問わず日付ファイルを
全部マージするので、置くだけで /llm-gateway/stats に合流する。

- credential は jsonl から特定できないので一律 "unknown"
- 重複行 (同一 message.id × requestId の再記録) は 1 回だけ数える
- gateway 自身の集計がある日 (= --until 以降) は二重計上になるので書かない
"""

import glob
import json
import os
import sys
from collections import defaultdict
from datetime import datetime, timedelta, timezone

JST = timezone(timedelta(hours=9))
STATS_DIR = os.path.expanduser("~/.local/state/llm-gateway/stats")
SOURCES = sorted(glob.glob(os.path.expanduser("~/.claude-*/projects/**/*.jsonl"), recursive=True))

# gateway 集計の最古日 (これ以降は gateway 側が正)
until = sys.argv[1] if len(sys.argv) > 1 else "2026-07-30"

seen = set()
# days[date][model] = counters
days: dict = defaultdict(lambda: defaultdict(lambda: defaultdict(int)))

for path in SOURCES:
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                if '"usage"' not in line:
                    continue
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue
                msg = rec.get("message") or {}
                usage = msg.get("usage")
                model = msg.get("model")
                ts = rec.get("timestamp")
                if not usage or not model or not ts or model == "<synthetic>":
                    continue
                key = (msg.get("id"), rec.get("requestId"))
                if key == (None, None) or key in seen:
                    continue
                seen.add(key)
                date = (
                    datetime.fromisoformat(ts.replace("Z", "+00:00"))
                    .astimezone(JST)
                    .strftime("%Y-%m-%d")
                )
                if date >= until:
                    continue
                c = days[date][model]
                c["requests"] += 1
                for k in (
                    "input_tokens",
                    "output_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ):
                    c[k] += usage.get(k) or 0
    except OSError as e:
        print(f"skip {path}: {e}", file=sys.stderr)

os.makedirs(STATS_DIR, exist_ok=True)
for date, models in sorted(days.items()):
    out = {"unknown": {m: dict(c) for m, c in sorted(models.items())}}
    tmp = os.path.join(STATS_DIR, f".{date}.backfill.json.tmp")
    dst = os.path.join(STATS_DIR, f"{date}.backfill.json")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(out, f, ensure_ascii=False, indent=1)
    os.replace(tmp, dst)
    total = sum(c["requests"] for c in models.values())
    print(f"{date}: {len(models)} models, {total} requests")

print(f"done: {len(days)} days from {len(SOURCES)} files (until {until} exclusive)")
