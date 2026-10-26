"""Per-request options."""

from dataclasses import dataclass, field


@dataclass
class RequestOptions:
    # Seconds the whole request may take.
    timeout: float = 45
    # Seconds to wait for the connection.
    connect_timeout: float = 5
    # The most items to collect when following pages.
    max_items: int = 30
    headers: dict = field(default_factory=dict)
