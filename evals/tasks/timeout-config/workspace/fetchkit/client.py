"""The client."""

from . import settings
from .backoff import delays
from .cache import ResponseCache
from .errors import FetchError, RequestTimeout
from .transport import MemoryTransport


class Client:
    """Sends GET requests to one base URL, with a timeout, retries and a response cache."""

    def __init__(
        self,
        base_url,
        *,
        timeout=30.0,
        connect_timeout=settings.CONNECT_TIMEOUT,
        retries=settings.MAX_RETRIES,
        transport=None,
        sleep=None,
    ):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self.connect_timeout = connect_timeout
        self.retries = retries
        self.transport = transport or MemoryTransport()
        self.cache = ResponseCache()
        self.sleep = sleep or (lambda seconds: None)

    def get(self, path, *, timeout=None):
        """GET ``path`` from the base URL and return the response's body.

        ``timeout`` is how many seconds the whole request may take. It defaults to the client's
        own: 30 seconds, unless the client was made with another. Timeouts and 5xx responses are
        retried, with ``backoff.delays`` between attempts.
        """
        url = self.base_url + path
        cached = self.cache.get(url)
        if cached is not None:
            return cached.body
        limit = self.timeout if timeout is None else timeout
        waits = delays(self.retries)
        for attempt in range(self.retries + 1):
            try:
                response = self.transport.send(url, timeout=limit, connect_timeout=self.connect_timeout)
            except RequestTimeout:
                if attempt == self.retries:
                    raise
            else:
                if response.status < 500:
                    if response.status >= 400:
                        raise FetchError(f"{url}: status {response.status}")
                    self.cache.put(url, response)
                    return response.body
                if attempt == self.retries:
                    raise FetchError(f"{url}: status {response.status}")
            self.sleep(waits[attempt])
        raise AssertionError("unreachable")
