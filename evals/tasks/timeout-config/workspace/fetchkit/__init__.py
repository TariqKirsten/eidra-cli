"""fetchkit: a small HTTP client with timeouts, retries, paging and caching."""

from .client import Client
from .errors import FetchError, RequestTimeout
from .options import RequestOptions

__all__ = ["Client", "FetchError", "RequestOptions", "RequestTimeout"]
__version__ = "1.4.0"
