# report.py

`python3 report.py LOG...` reads web server access logs and prints a summary of the requests in
all of them together. Use only the Python standard library.

## Input

A log line looks like this:

    203.0.113.9 - - [01/Oct/2026:10:00:03 +0000] "GET /api/items?page=2 HTTP/1.1" 200 512 rt=0.084

After the client address, two dashes and the time in brackets come:

- the request in double quotes: the method, the target and the protocol, separated by single spaces
- the status code: three digits
- the response size: anything without spaces, such as `512` or `-`
- `rt=` and the response time in seconds: digits, optionally with a decimal point and more digits

Fields are separated by single spaces. The path of a request is its target without the query
string, so `/api/items?page=2` has the path `/api/items`.

Blank lines are ignored. Any other line that is not in this format is skipped and counted as
skipped.

## Output

For the example below, the output is:

    Requests: 5 (skipped 1)
    Top paths:
      3  /api/items  p95 120 ms
      1  /           p95 12 ms
      1  /api/login  p95 -

- The first line gives the number of requests read and the number of lines skipped.
- Then come the five paths with the most requests, most first. Paths with the same number of
  requests are in alphabetical order. With fewer than five paths there are fewer lines, and with
  none there are none.
- A path's line is two spaces, then its count right-aligned to the width of the largest count
  shown, then two spaces. Then the path, padded with spaces to the length of the longest path
  shown, two spaces, and its response-time percentile.
- The percentile is the 95th percentile of the path's response times by the nearest-rank method:
  sort the times and take the one at position ceil(0.95 × n), counting from 1. Only responses
  with a status below 500 count towards it; every request counts towards the totals.
- The percentile is printed in whole milliseconds, rounded half up (12.5 ms is `p95 13 ms`). A
  path with no response below 500 prints `p95 -`.

The example:

    198.51.100.7 - - [02/Oct/2026:09:15:00 +0000] "GET /api/items?page=1 HTTP/1.1" 200 812 rt=0.041
    198.51.100.7 - - [02/Oct/2026:09:15:02 +0000] "GET /api/items?page=2 HTTP/1.1" 200 790 rt=0.120
    198.51.100.9 - - [02/Oct/2026:09:15:03 +0000] "GET / HTTP/1.1" 200 5120 rt=0.012
    198.51.100.9 - - [02/Oct/2026:09:15:04 +0000] "POST /api/login HTTP/1.1" 503 0 rt=2.500
    connection reset by peer
    198.51.100.7 - - [02/Oct/2026:09:15:09 +0000] "GET /api/items HTTP/1.1" 200 805 rt=0.067
