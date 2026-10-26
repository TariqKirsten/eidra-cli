"""Reference solution (used only to validate the tests; agents never see it)."""
import decimal
import json
import re

from ledger.models import BudgetLine


def load_budgets(text):
    data = json.loads(text)
    if not isinstance(data, dict):
        raise ValueError("budgets must be a JSON object")
    out = {}
    for key, value in data.items():
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ValueError(f"budget for {key!r} is not a number")
        amount = decimal.Decimal(str(value))
        if amount < 0 or amount.as_tuple().exponent < -2:
            raise ValueError(f"bad budget for {key!r}: {value}")
        out[key.strip().lower()] = int(amount * 100)
    return out


def check_budgets(expenses, budgets, month):
    m = re.fullmatch(r"(\d{4})-(0[1-9]|1[0-2])", month)
    if not m:
        raise ValueError(f"month must be YYYY-MM: {month!r}")
    year, mon = int(m.group(1)), int(m.group(2))
    spent = {}
    for e in expenses:
        if e.date.year == year and e.date.month == mon:
            spent[e.category] = spent.get(e.category, 0) + e.amount_cents
    lines = []
    for cat, budget in budgets.items():
        s = spent.get(cat, 0)
        pct = round(s / budget * 100, 1) if budget else 0.0
        lines.append(BudgetLine(cat, budget, s, budget - s, pct, s > budget))
    return sorted(lines, key=lambda b: (-b.percent_used, b.category))
