"""Writes the log-report task's logs: the sample in workspace/logs, and the hidden ones in check/.

Deterministic: running it again writes the same files. Not part of the workspace or the checks.
"""

import math
import random
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).parent
MONTHS = "Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec".split()
METHODS = ["GET"] * 8 + ["POST", "PUT", "DELETE"]


def stamp(when):
    return f"{when.day:02d}/{MONTHS[when.month - 1]}/{when.year}:{when:%H:%M:%S} +0000"


def line(rng, when, target, status, rt):
    ip = f"203.0.113.{rng.randint(1, 254)}"
    size = 0 if status in (204, 304) else rng.randint(80, 9000)
    return f'{ip} - - [{stamp(when)}] "{rng.choice(METHODS)} {target} HTTP/1.1" {status} {size} rt={rt}'


def seconds(rng, typical):
    return f"{max(0.001, rng.gauss(typical, typical / 3)):.3f}"


MALFORMED = [
    '203.0.113.5 - - [01/Oct/2026:10:00:00 +0000] "GET /api/items HTTP/1.1" 200 512',
    "203.0.113.5 - - [01/Oct/2026:10:00:01 +0000] GET /api/items HTTP/1.1 200 512 rt=0.01",
    '\\x16\\x03\\x01 "garbage" rt=1',
    '203.0.113.5 - - [01/Oct/2026:10:00:02 +0000] "GET /api/items HTTP/1.1" two hundred 512 rt=0.02',
    '203.0.113.5 - - [01/Oct/2026:10:00:03 +0000] "GET /api/items HTTP/1.1" 200 512 rt=',
    "connection reset by peer",
    '203.0.113.5 - - [01/Oct/2026:10:00:04 +0000] "GET /api/items HTTP/1.1" 200 512 rt=fast',
]

# path: (weight, typical seconds, statuses to draw from, query strings to draw from)
SAMPLE_PATHS = {
    "/api/items": (30, 0.08, [200] * 18 + [304, 404], ["", "", "?page=2", "?page=3&sort=name"]),
    "/": (15, 0.012, [200] * 9 + [304], [""]),
    "/api/orders": (12, 0.15, [200] * 9 + [201, 500], ["", "?status=open"]),
    "/static/app.js": (10, 0.004, [200, 304], [""]),
    "/api/login": (8, 0.2, [200] * 6 + [401, 503], [""]),
    "/health": (8, 0.002, [200], [""]),
    "/api/search": (7, 0.35, [200] * 5 + [500, 503], ["?q=bolt", "?q=nut&limit=5"]),
    "/favicon.ico": (5, 0.001, [200, 404], [""]),
    "/api/reports": (5, 1.2, [200] * 4 + [504], ["", "?month=2026-09"]),
}


def sample_log(rng, start, lines, malformed):
    paths = list(SAMPLE_PATHS)
    weights = [SAMPLE_PATHS[p][0] for p in paths]
    out = []
    when = start
    for _ in range(lines):
        path = rng.choices(paths, weights)[0]
        _, typical, statuses, queries = SAMPLE_PATHS[path]
        when += timedelta(seconds=rng.randint(1, 40))
        out.append(line(rng, when, path + rng.choice(queries), rng.choice(statuses), seconds(rng, typical)))
    for bad in malformed:
        out.insert(rng.randrange(len(out)), bad)
    return out


def hidden_log(rng):
    """Exact counts: a tie at fourth and fifth place, a sixth path just below it, a path whose
    every response failed, and a path whose 95th percentile is exactly half a millisecond."""
    when = datetime(2026, 10, 3, 6, 0, tzinfo=timezone.utc)
    entries = []
    entries += [("/api/v2/items" + rng.choice(["", "?page=4", "?page=5&sort=price"]), 200, seconds(rng, 0.09)) for _ in range(60)]
    entries += [("/login", 200, "0.0125") for _ in range(40)]
    entries += [("/api/v2/broken", rng.choice([500, 502, 503]), seconds(rng, 0.5)) for _ in range(30)]
    entries += [("/contact", rng.choice([200, 200, 302]), seconds(rng, 0.03)) for _ in range(22)]
    entries += [("/about", 200, seconds(rng, 0.02)) for _ in range(22)]
    entries += [("/search?q=" + rng.choice(["a", "b"]), 200, seconds(rng, 0.3)) for _ in range(21)]
    entries += [("/robots.txt", 404, "0.001") for _ in range(4)]
    rng.shuffle(entries)
    out = []
    for target, status, rt in entries:
        when += timedelta(seconds=rng.randint(1, 20))
        out.append(line(rng, when, target, status, rt))
    for bad in MALFORMED:
        out.insert(rng.randrange(len(out)), bad)
    for _ in range(3):
        out.insert(rng.randrange(len(out)), "")
    return out


SMALL = [
    '192.0.2.1 - - [04/Oct/2026:12:00:00 +0000] "GET /a HTTP/1.1" 200 10 rt=0.0005',
    '192.0.2.1 - - [04/Oct/2026:12:00:01 +0000] "GET /c?x=1 HTTP/1.1" 200 10 rt=0.0005',
    "not a log line",
    '192.0.2.1 - - [04/Oct/2026:12:00:02 +0000] "GET /a HTTP/1.1" 200 10 rt=0.002',
    '192.0.2.1 - - [04/Oct/2026:12:00:03 +0000] "POST /b HTTP/1.1" 503 0 rt=1.0',
]


def check_ranks(counts):
    """The 95th-percentile rank, as ceil(0.95 * n) in floating point, must agree with exact
    arithmetic for every count used, so the task never turns on a floating-point accident."""
    for n in counts:
        assert math.ceil(0.95 * n) == (95 * n + 99) // 100, n


def write(path, lines):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(f"{line}\n" for line in lines))


def main():
    check_ranks(range(1, 400))
    write(HERE / "workspace/logs/access-1.log", sample_log(random.Random(1), datetime(2026, 10, 1, 8, tzinfo=timezone.utc), 220, MALFORMED[:2]))
    write(HERE / "workspace/logs/access-2.log", sample_log(random.Random(2), datetime(2026, 10, 2, 8, tzinfo=timezone.utc), 180, MALFORMED[2:3]))
    write(HERE / "check/logs/access-3.log", hidden_log(random.Random(3)))
    write(HERE / "check/small.log", SMALL)
    write(HERE / "check/empty.log", [])


if __name__ == "__main__":
    main()
