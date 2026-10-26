"""Defaults shared across fetchkit."""

# Seconds a whole request may take, from connecting to the last byte of the response.
DEFAULT_TIMEOUT = 45.0
# Seconds to wait for the connection itself.
CONNECT_TIMEOUT = 5.0
# Attempts after the first, for requests that fail in a way worth retrying.
MAX_RETRIES = 3
# Items per page when listing.
PAGE_SIZE = 30
USER_AGENT = "fetchkit/1.4"
