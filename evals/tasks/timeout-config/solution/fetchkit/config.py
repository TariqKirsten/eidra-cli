"""Reads a TOML configuration file. Keys the file leaves out take these defaults."""

import copy
import tomllib

DEFAULTS = {
    "request": {"timeout": 45, "connect_timeout": 5, "retries": 3},
    "cache": {"ttl": 300, "max_entries": 30},
    "listing": {"page_size": 30},
}


def load(path=None):
    """The configuration: DEFAULTS, overridden by the file at ``path`` when one is given."""
    config = copy.deepcopy(DEFAULTS)
    if path is not None:
        with open(path, "rb") as f:
            for section, values in tomllib.load(f).items():
                config.setdefault(section, {}).update(values)
    return config
