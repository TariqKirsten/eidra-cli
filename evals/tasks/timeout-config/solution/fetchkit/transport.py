"""Where requests go. The default transport serves canned responses from memory, for tests and
offline use."""

from .errors import RequestTimeout


class Response:
    def __init__(self, status, body, elapsed):
        self.status = status
        self.body = body
        self.elapsed = elapsed


class MemoryTransport:
    """Serves canned responses: ``routes`` maps a URL to (status, body, seconds it takes)."""

    def __init__(self, routes=None):
        self.routes = dict(routes or {})
        self.sent = []

    def send(self, url, *, timeout, connect_timeout):
        self.sent.append((url, timeout, connect_timeout))
        status, body, seconds = self.routes.get(url, (404, "", 0.0))
        if seconds > timeout:
            raise RequestTimeout(url, timeout)
        return Response(status, body, seconds)
