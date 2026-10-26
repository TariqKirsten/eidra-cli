# ledger — a tiny expense report tool

Python 3 standard library only. Money is always integer **cents** (never floats).
The shared data types live in `ledger/models.py` (already written — do not change it).

## ledger/parse.py

`parse_amount(text: str) -> int`
- Converts a dollar amount to cents. Surrounding whitespace is ignored.
- Accepted forms: `12`, `12.5`, `12.50`, `$12.50`, `1,234.56`, `$1,234.56`, `-3`, `-$3.00`.
  A single leading `-` comes before an optional single `$`. Commas are thousands separators.
- At least one digit before the decimal point; a decimal point must be followed by one or two
  digits (so `.5` and `1.` are invalid).
- Must be exact: `"7.05"` is `705` (do not go through float).
- Anything else (empty, letters, `1.234`, `12.3.4`, `$`, `--3`, `3-`) raises `ValueError`.

`parse_expenses(text: str) -> list[Expense]`
- `text` is CSV. Blank lines and comment lines (see below) are skipped everywhere, including
  before the header. The first remaining line is the header; it must contain the columns
  `date`, `amount`, `category` and `note` (any order, case-insensitive). A missing column raises
  `ValueError` whose message contains that column name.
- Fields may be quoted (`"1,200.00"`, `"rent, flat 2"`); use the `csv` module.
- Blank lines and lines whose first non-space character is `#` are skipped.
- `date` is ISO `YYYY-MM-DD`; `amount` uses `parse_amount`; `category` is stripped and
  lower-cased and must not be empty; `note` is stripped (may be empty).
- Any bad row raises `ValueError` whose message contains `line N`, where N is the 1-based line
  number in `text` (the header is line 1 when it is the first line).
- Returns expenses in file order. Empty or whitespace-only text, or a header with no rows, returns `[]`.

## ledger/stats.py

`totals_by_category(expenses) -> dict[str, int]` — sum per category, keys in alphabetical order.

`totals_by_month(expenses) -> dict[str, int]` — sum per `"YYYY-MM"`, keys in ascending order.
Months without expenses are not included.

`top_categories(expenses, n=3) -> list[tuple[str, int]]` — `(category, total)` pairs sorted by
total descending, ties by category ascending. Categories whose total is `<= 0` are excluded.
At most `n` pairs.

`monthly_average(expenses) -> int` — the sum of all amounts divided by the number of calendar
months from the earliest expense's month to the latest's, inclusive (months in between with no
expenses still count). Rounded to the nearest cent, halves rounded up. Empty input returns `0`.

## ledger/budget.py

`load_budgets(text: str) -> dict[str, int]` — `text` is a JSON object mapping category to a
dollar amount given as a JSON number (`400`, `12.5`, `0.1`). Returns cents (`40000`, `1250`, `10`),
exact (convert via `decimal.Decimal(str(value))`). Keys are stripped and lower-cased. Raises
`ValueError` if the JSON is not an object, a value is not a number (bool is not a number), a value
is negative, or a value has more than two decimal places.

`check_budgets(expenses, budgets: dict[str, int], month: str) -> list[BudgetLine]`
- `month` is `"YYYY-MM"`: four-digit year, two-digit month 01–12 (otherwise `ValueError`;
  note `datetime.strptime` alone also accepts `2026-2`). Only expenses dated in that month count.
- One `BudgetLine` for every budgeted category, even with no spending. Spending in categories
  without a budget is ignored. Refunds (negative amounts) reduce `spent_cents`.
- `remaining_cents = budget_cents - spent_cents`; `over = spent_cents > budget_cents`;
  `percent_used = round(spent_cents / budget_cents * 100, 1)`, or `0.0` when the budget is 0.
- Sorted by `percent_used` descending, ties by category ascending.

## ledger/fmt.py

`format_money(cents: int) -> str` — `"$1,234.50"`, `"$0.07"`, `"$0.00"`, negatives `"-$3.00"`.

`render_table(headers: list[str], rows: list[list[str]], align: str | None = None) -> str`
- Each column's width is the longest of its header and cells. Columns are joined by two spaces.
- `align` has one character per column, `l` (left) or `r` (right); default all left. Headers use
  the same alignment as their column.
- Output lines: the header line, a separator line of `-` repeated to each column's width
  (joined by two spaces), then one line per row. Trailing spaces are removed from every line.
  Lines are joined with `\n`, no trailing newline.
- A row with the wrong number of cells, or an `align` of the wrong length or with other
  characters, raises `ValueError`.

## ledger/cli.py (written after the four modules above)

`main(argv: list[str] | None = None) -> int`, usable as `python3 -m ledger.cli EXPENSES.csv
[--budgets BUDGETS.json] [--month YYYY-MM] [--top N]` (use `argparse`; `--top` defaults to 3).
Prints this report to stdout, sections separated by one blank line:

```
Expense report: <count> expenses, <earliest date> to <latest date>
Total: <format_money(sum of all amounts)>

By category
<render_table(["Category", "Total"], ..., "lr") using totals_by_category>

By month
<render_table(["Month", "Total"], ..., "lr") using totals_by_month>

Monthly average: <format_money(monthly_average)>
Top categories: <"name ($x.xx)" joined by ", " from top_categories(n=--top)>
```

When `--budgets` is given, add a final section for `--month` (default: the latest month in the data):

```
Budgets for <YYYY-MM>
<render_table(["Category", "Budget", "Spent", "Remaining", "Used"], ..., "lrrrr")>
```
where money columns use `format_money`, `Used` is `percent_used` formatted as `"30.9%"`, and
`" OVER"` is appended to the `Used` cell of lines where `over` is true.
Returns 0. On a missing file or a `ValueError`, prints `error: <message>` to stderr and returns 2.
If there are no expenses, prints `No expenses.` and returns 0.
