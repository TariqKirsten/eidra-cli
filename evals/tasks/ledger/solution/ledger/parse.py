"""Reference solution (used only to validate the tests; agents never see it)."""
import csv
import datetime
import re

from ledger.models import Expense

_AMOUNT = re.compile(r"(-?)\$?(\d{1,3}(?:,\d{3})+|\d+)(?:\.(\d{1,2}))?")
_COLUMNS = ("date", "amount", "category", "note")


def parse_amount(text):
    m = _AMOUNT.fullmatch(text.strip())
    if not m:
        raise ValueError(f"invalid amount: {text!r}")
    sign, whole, frac = m.groups()
    cents = int(whole.replace(",", "")) * 100 + int((frac or "0").ljust(2, "0"))
    return -cents if sign else cents


def parse_expenses(text):
    lines = [(n, line) for n, line in enumerate(text.splitlines(), 1)
             if line.strip() and not line.lstrip().startswith("#")]
    if not lines:
        return []
    header = [h.strip().lower() for h in next(csv.reader([lines[0][1]]))]
    for col in _COLUMNS:
        if col not in header:
            raise ValueError(f"missing column: {col}")
    idx = {col: header.index(col) for col in _COLUMNS}
    out = []
    for n, line in lines[1:]:
        try:
            row = next(csv.reader([line]))
            if len(row) != len(header):
                raise ValueError(f"expected {len(header)} fields, got {len(row)}")
            date = datetime.date.fromisoformat(row[idx["date"]].strip())
            amount = parse_amount(row[idx["amount"]])
            category = row[idx["category"]].strip().lower()
            if not category:
                raise ValueError("empty category")
            out.append(Expense(date, amount, category, row[idx["note"]].strip()))
        except ValueError as exc:
            raise ValueError(f"line {n}: {exc}") from None
    return out
