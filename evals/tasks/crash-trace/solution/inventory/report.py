"""What to reorder, and how urgently."""

# Items this close to their reorder level are listed as coming up.
NEAR = 5


def needs_reorder(item):
    """Whether the item's stock is at or below its reorder level. Items without one never are."""
    return item.reorder_at is not None and item.stock <= item.reorder_at


def days_left(item):
    """Days until the item runs out at its usual daily use; None if it is not used."""
    if item.daily_use <= 0:
        return None
    return item.stock / item.daily_use


def margin(item):
    """Days between a reorder placed today arriving and the item running out; negative is late."""
    left = days_left(item)
    if left is None:
        return float("inf")
    return left - item.lead_days


def shortfall(item):
    """Units below the reorder level."""
    if item.reorder_at is None:
        return 0
    return max(0, item.reorder_at - item.stock)


def format_line(item):
    days = margin(item)
    when = "LATE" if days < 0 else f"{days:.1f} days to spare"
    return f"{item.sku:<10} {item.name:<22} stock {item.stock:>4}  short {shortfall(item):>4}  {when}"


def coming_up(item):
    """Whether the item is above its reorder level, but close to it."""
    return item.reorder_at is not None and not needs_reorder(item) and item.stock - item.reorder_at <= NEAR


def build_report(items):
    """The report's lines: items to reorder, most urgent first; items coming up; a count."""
    due = [item for item in items if needs_reorder(item)]
    due.sort(key=lambda item: (margin(item), item.sku))
    lines = [format_line(item) for item in due]
    near = [item for item in items if coming_up(item)]
    if near:
        lines.append("Coming up: " + ", ".join(sorted(item.sku for item in near)))
    lines.append(f"{len(due)} of {len(items)} items to reorder")
    return lines
