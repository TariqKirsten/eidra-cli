# The command line

    python3 -m fetchkit.cli URL [--timeout SECONDS] [--connect-timeout SECONDS] [--retries N] [--page-size N]

- `--timeout SECONDS`: how long the whole request may take. The default is 30.
- `--connect-timeout SECONDS`: how long to wait for the connection. The default is 5.
- `--retries N`: retries after the first attempt. The default is 3.
- `--page-size N`: items per page when listing. The default is 30.

Retries wait 0.5 s, then 1 s, then 2 s, doubling up to at most 30 s.
