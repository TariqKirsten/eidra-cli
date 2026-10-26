"""python3 -m inventory STOCK.json: the reorder report."""

import sys

from .model import load
from .report import build_report


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 1:
        print("usage: python3 -m inventory STOCK.json", file=sys.stderr)
        return 2
    for line in build_report(load(argv[0])):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
