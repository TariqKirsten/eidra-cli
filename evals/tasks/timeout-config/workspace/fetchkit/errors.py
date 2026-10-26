"""fetchkit's exceptions."""


class FetchError(Exception):
    """A request failed."""


class RequestTimeout(FetchError):
    """A request took longer than its timeout."""

    def __init__(self, url, timeout):
        super().__init__(f"{url}: no complete response within {timeout:g} s")
        self.url = url
        self.timeout = timeout
