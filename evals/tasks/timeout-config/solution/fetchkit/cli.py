"""fetchkit's command line: python3 -m fetchkit.cli URL [options]"""

import argparse
import sys

from . import settings


def build_parser():
    parser = argparse.ArgumentParser(prog="fetchkit", description="Fetch a URL with timeouts and retries.")
    parser.add_argument("url")
    parser.add_argument(
        "--timeout",
        type=float,
        default=settings.DEFAULT_TIMEOUT,
        help="seconds the whole request may take (default: 45)",
    )
    parser.add_argument(
        "--connect-timeout",
        type=float,
        default=settings.CONNECT_TIMEOUT,
        help="seconds to wait for the connection (default: 5)",
    )
    parser.add_argument(
        "--retries", type=int, default=settings.MAX_RETRIES, help="retries after the first attempt (default: 3)"
    )
    parser.add_argument(
        "--page-size", type=int, default=settings.PAGE_SIZE, help="items per page when listing (default: 30)"
    )
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    print(f"would fetch {args.url} with a {args.timeout:g} s timeout")
    return 0


if __name__ == "__main__":
    sys.exit(main())
