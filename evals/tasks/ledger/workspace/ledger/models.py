"""Shared data types. Money is integer cents."""
import datetime
from dataclasses import dataclass


@dataclass(frozen=True)
class Expense:
    date: datetime.date
    amount_cents: int  # positive = spent, negative = refund
    category: str      # stripped, lower-case
    note: str = ""


@dataclass(frozen=True)
class BudgetLine:
    category: str
    budget_cents: int
    spent_cents: int
    remaining_cents: int  # budget - spent (negative when over)
    percent_used: float   # round(spent / budget * 100, 1); 0.0 when budget is 0
    over: bool            # spent > budget
