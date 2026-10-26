#!/usr/bin/env python3
"""Summarises web server access logs: python3 report.py LOG..."""

import re
import sys
from collections import defaultdict
from decimal import ROUND_HALF_UP, Decimal

LINE = re.compile(r'^\S+ \S+ \S+ \[[^\]]+\] "\S+ (\S+) \S+" (\d{3}) \S+ rt=(\d+(?:\.\d+)?)$')


def p95(times):
    if not times:
        return "p95 -"
    times = sorted(times)
    rank = (95 * len(times) + 99) // 100  # ceil(0.95 * n), counting from 1
    ms = (times[rank - 1] * 1000).quantize(Decimal(1), rounding=ROUND_HALF_UP)
    return f"p95 {ms} ms"


def main(files):
    requests = skipped = 0
    counts = defaultdict(int)
    times = defaultdict(list)
    for name in files:
        with open(name, encoding="utf-8", errors="replace") as f:
            for raw in f:
                line = raw.rstrip("\n")
                if not line.strip():
                    continue
                match = LINE.match(line)
                if not match:
                    skipped += 1
                    continue
                target, status, rt = match.groups()
                path = target.split("?", 1)[0]
                requests += 1
                counts[path] += 1
                if int(status) < 500:
                    times[path].append(Decimal(rt))
    top = sorted(counts.items(), key=lambda pair: (-pair[1], pair[0]))[:5]
    print(f"Requests: {requests} (skipped {skipped})")
    print("Top paths:")
    if top:
        count_width = max(len(str(count)) for _, count in top)
        path_width = max(len(path) for path, _ in top)
        for path, count in top:
            print(f"  {count:>{count_width}}  {path:<{path_width}}  {p95(times[path])}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
