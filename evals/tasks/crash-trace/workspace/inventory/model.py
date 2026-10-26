"""Stock items, read from a JSON file."""

import json
from dataclasses import dataclass


@dataclass(frozen=True)
class Item:
    sku: str
    name: str
    stock: int
    # Reorder when stock is at or below this; None for items that are never reordered.
    reorder_at: int | None
    daily_use: float
    lead_days: int


def load(path):
    with open(path, encoding="utf-8") as f:
        raw = json.load(f)
    return [Item(**entry) for entry in raw["items"]]
