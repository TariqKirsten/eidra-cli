"""The docs give the request timeout as 45 seconds, and every other setting as it was. Checks
sections and lines rather than exact text, so the agent may reword them; the numbers must be right."""

import re
import sys

problems = []


def read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def section(text, heading):
    """The text under a "## heading", up to the next heading."""
    match = re.search(rf"^## {re.escape(heading)}\n(.*?)(?=^## |\Z)", text, re.S | re.M)
    return match.group(1) if match else ""


def line_with(text, needle):
    return next((line for line in text.splitlines() if needle in line), "")


def expect(ok, problem):
    if not ok:
        problems.append(problem)


readme = read("README.md")
timeouts = section(readme, "Timeouts")
expect("45" in timeouts and "30" not in timeouts, "README.md, Timeouts: the request timeout should be 45 seconds")
expect("5 seconds" in timeouts, "README.md, Timeouts: the connect timeout should still be 5 seconds")
expect("30 s" in section(readme, "Retries"), "README.md, Retries: the waits should still stop at 30 s")
paging = section(readme, "Paging and caching")
expect("30 items" in paging and "at most 30" in paging, "README.md, Paging and caching: 30 items a page, at most 30 cached")

usage = read("docs/usage.md")
timeout = line_with(usage, "`--timeout SECONDS`")
expect("45" in timeout and "30" not in timeout, "docs/usage.md: --timeout should default to 45")
expect("30" in line_with(usage, "`--page-size N`"), "docs/usage.md: --page-size should still default to 30")
expect("30 s" in line_with(usage, "Retries wait"), "docs/usage.md: retry waits should still stop at 30 s")

configuration = read("docs/configuration.md")
row = line_with(configuration, "`request.timeout`")
expect(re.search(r"\b45\b", row) and not re.search(r"\b30\b", row), "docs/configuration.md: request.timeout should default to 45")
for key in ("cache.max_entries", "listing.page_size"):
    expect(re.search(r"\b30\b", line_with(configuration, f"`{key}`")), f"docs/configuration.md: {key} should still default to 30")

if problems:
    print("\n".join(problems))
    sys.exit(1)
