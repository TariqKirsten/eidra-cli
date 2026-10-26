"""Reference solution (used only to validate the tests; agents never see it)."""


def format_money(cents):
    sign = "-" if cents < 0 else ""
    dollars, rest = divmod(abs(cents), 100)
    return f"{sign}${dollars:,}.{rest:02d}"


def render_table(headers, rows, align=None):
    ncol = len(headers)
    align = align if align is not None else "l" * ncol
    if len(align) != ncol or set(align) - {"l", "r"}:
        raise ValueError("bad align")
    for row in rows:
        if len(row) != ncol:
            raise ValueError("row has wrong number of cells")
    widths = [max(len(str(c)) for c in [h] + [r[i] for r in rows]) for i, h in enumerate(headers)]

    def line(cells):
        parts = [c.rjust(w) if a == "r" else c.ljust(w) for c, w, a in zip(cells, widths, align)]
        return "  ".join(parts).rstrip()

    return "\n".join([line(headers), line(["-" * w for w in widths])] + [line(r) for r in rows])
