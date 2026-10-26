import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from ledger.cli import main

DATA = Path(__file__).resolve().parent.parent / "data"
CSV = str(DATA / "expenses.csv")
BUDGETS = str(DATA / "budgets.json")

REPORT = """\
Expense report: 13 expenses, 2026-01-03 to 2026-03-04
Total: $4,251.40

By category
Category       Total
---------  ---------
dining       $114.40
groceries    $358.25
rent       $3,600.00
transport     $47.00
utilities    $131.75

By month
Month        Total
-------  ---------
2026-01  $1,445.05
2026-02  $1,510.75
2026-03  $1,295.60

Monthly average: $1,417.13
Top categories: rent ($3,600.00), groceries ($358.25), utilities ($131.75)"""

MARCH = """\
Budgets for 2026-03
Category          Budget      Spent  Remaining    Used
-------------  ---------  ---------  ---------  ------
rent           $1,200.00  $1,200.00      $0.00  100.0%
groceries        $300.00     $95.60    $204.40   31.9%
dining            $50.00      $0.00     $50.00    0.0%
entertainment     $25.50      $0.00     $25.50    0.0%
transport         $45.00      $0.00     $45.00    0.0%"""

FEBRUARY = """\
Budgets for 2026-02
Category          Budget      Spent  Remaining         Used
-------------  ---------  ---------  ---------  -----------
dining            $50.00     $68.40    -$18.40  136.8% OVER
rent           $1,200.00  $1,200.00      $0.00       100.0%
transport         $45.00     $23.50     $21.50        52.2%
groceries        $300.00     $87.10    $212.90        29.0%
entertainment     $25.50      $0.00     $25.50         0.0%"""


def run(*argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(list(argv))
    return code, out.getvalue().rstrip("\n"), err.getvalue()


class Report(unittest.TestCase):
    def test_report(self):
        self.assertEqual(run(CSV), (0, REPORT, ""))

    def test_budgets_default_to_latest_month(self):
        self.assertEqual(run(CSV, "--budgets", BUDGETS), (0, REPORT + "\n\n" + MARCH, ""))

    def test_budgets_for_month_and_top(self):
        code, out, _ = run(CSV, "--budgets", BUDGETS, "--month", "2026-02", "--top", "2")
        self.assertEqual(code, 0)
        self.assertIn("\nTop categories: rent ($3,600.00), groceries ($358.25)\n", out)
        self.assertTrue(out.endswith("\n\n" + FEBRUARY), out)


class Errors(unittest.TestCase):
    def test_missing_file(self):
        code, out, err = run("/nonexistent/expenses.csv")
        self.assertEqual((code, out), (2, ""))
        self.assertTrue(err.startswith("error: "), err)

    def test_bad_row_reports_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            bad = Path(tmp) / "bad.csv"
            bad.write_text("date,amount,category,note\n2026-01-01,12.3.4,food,\n")
            code, out, err = run(str(bad))
        self.assertEqual((code, out), (2, ""))
        self.assertTrue(err.startswith("error: ") and "line 2" in err, err)

    def test_no_expenses(self):
        with tempfile.TemporaryDirectory() as tmp:
            empty = Path(tmp) / "empty.csv"
            empty.write_text("date,amount,category,note\n")
            self.assertEqual(run(str(empty)), (0, "No expenses.", ""))


if __name__ == "__main__":
    unittest.main()
