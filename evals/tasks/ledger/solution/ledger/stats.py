"""Reference solution (used only to validate the tests; agents never see it)."""


def totals_by_category(expenses):
    out = {}
    for e in expenses:
        out[e.category] = out.get(e.category, 0) + e.amount_cents
    return dict(sorted(out.items()))


def totals_by_month(expenses):
    out = {}
    for e in expenses:
        key = f"{e.date.year:04d}-{e.date.month:02d}"
        out[key] = out.get(key, 0) + e.amount_cents
    return dict(sorted(out.items()))


def top_categories(expenses, n=3):
    pairs = [(c, t) for c, t in totals_by_category(expenses).items() if t > 0]
    return sorted(pairs, key=lambda p: (-p[1], p[0]))[:n]


def monthly_average(expenses):
    if not expenses:
        return 0
    first = min(e.date for e in expenses)
    last = max(e.date for e in expenses)
    months = (last.year - first.year) * 12 + last.month - first.month + 1
    total = sum(e.amount_cents for e in expenses)
    return (2 * total + months) // (2 * months)
