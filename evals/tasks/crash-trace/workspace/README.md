# inventory

Tells you which stock items to reorder, most urgent first.

    python3 -m inventory data/stock.json

## The stock file

A JSON object with a list of `items`. Each item has:

- `sku`: its stock-keeping unit, such as `"BOLT-M6"`
- `name`
- `stock`: units in stock now
- `reorder_at`: the stock level at or below which the item is reordered, or `null` for items that
  are never reordered (made to order, or being discontinued)
- `daily_use`: units used on an average day
- `lead_days`: days a reorder takes to arrive

## The report

One line per item to reorder, the most urgent first: those whose reorder would arrive latest
compared with when they run out. Then the items within five units of their reorder level, and a
count.

## Tests

    python3 -m unittest -q
