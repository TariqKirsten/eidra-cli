"""Following paged listings."""

import json

from . import settings


def collect(client, path, *, page_size=settings.PAGE_SIZE, max_items=None):
    """Every item of the paged JSON listing at ``path``, page by page, up to ``max_items``."""
    items = []
    page = 1
    while True:
        batch = json.loads(client.get(f"{path}?page={page}&size={page_size}"))
        items.extend(batch)
        if len(batch) < page_size or (max_items is not None and len(items) >= max_items):
            break
        page += 1
    return items if max_items is None else items[:max_items]
