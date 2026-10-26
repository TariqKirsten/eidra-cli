"""Reference solution (used only to validate the tests; agents never see it)."""
import argparse
import sys
from pathlib import Path

from ledger.budget import check_budgets, load_budgets
from ledger.fmt import format_money, render_table
from ledger.parse import parse_expenses
from ledger.stats import monthly_average, top_categories, totals_by_category, totals_by_month


def main(argv=None):
    p = argparse.ArgumentParser(prog="ledger")
    p.add_argument("expenses")
    p.add_argument("--budgets")
    p.add_argument("--month")
    p.add_argument("--top", type=int, default=3)
    args = p.parse_args(argv)
    try:
        expenses = parse_expenses(Path(args.expenses).read_text())
        budgets = load_budgets(Path(args.budgets).read_text()) if args.budgets else None
        if not expenses:
            print("No expenses.")
            return 0
        dates = [e.date for e in expenses]
        sections = [
            f"Expense report: {len(expenses)} expenses, {min(dates)} to {max(dates)}\n"
            f"Total: {format_money(sum(e.amount_cents for e in expenses))}",
            "By category\n" + render_table(["Category", "Total"], [
                [c, format_money(t)] for c, t in totals_by_category(expenses).items()], "lr"),
            "By month\n" + render_table(["Month", "Total"], [
                [m, format_money(t)] for m, t in totals_by_month(expenses).items()], "lr"),
            f"Monthly average: {format_money(monthly_average(expenses))}\n"
            "Top categories: " + ", ".join(
                f"{c} ({format_money(t)})" for c, t in top_categories(expenses, n=args.top)),
        ]
        if budgets is not None:
            month = args.month or max(totals_by_month(expenses))
            rows = [[b.category, format_money(b.budget_cents), format_money(b.spent_cents),
                     format_money(b.remaining_cents), f"{b.percent_used:.1f}%" + (" OVER" if b.over else "")]
                    for b in check_budgets(expenses, budgets, month)]
            sections.append(f"Budgets for {month}\n" + render_table(
                ["Category", "Budget", "Spent", "Remaining", "Used"], rows, "lrrrr"))
        print("\n\n".join(sections))
        return 0
    except (OSError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
