"""How long to wait between retries."""


def delays(retries, base=0.5, cap=30.0):
    """Seconds to wait before each retry: doubling from ``base``, never more than ``cap``."""
    return [min(cap, base * 2**attempt) for attempt in range(retries)]
