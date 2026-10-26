#!/bin/sh
# The CLI's report against the reference solution's, for the sample data and for a second data
# set the agent never saw. SPEC.md fixes the report exactly, but not the wording of errors, so an
# error only has to look like one: exit status 2 and "error: ..." on stderr.
set -u
status=0
cp -r /check/data2 data2

same() { # NAME ARGS...: stdout and exit status as the reference's
  name=$1
  shift
  python3 -m ledger.cli "$@" >/tmp/out 2>/tmp/err
  echo "[exit $?]" >>/tmp/out
  if ! diff -u "/check/expected/$name.txt" /tmp/out; then
    echo "^ differs from the reference: python3 -m ledger.cli $*"
    status=1
  fi
}

error() { # NEEDLE ARGS...: exit status 2, and stderr starting "error: ...NEEDLE"
  needle=$1
  shift
  python3 -m ledger.cli "$@" >/tmp/out 2>/tmp/err
  code=$?
  if [ "$code" != 2 ] || ! head -n 1 /tmp/err | grep -q "^error: .*$needle"; then
    echo "expected exit 2 and 'error: ...$needle' for: python3 -m ledger.cli $*"
    echo "got exit $code and stderr:"
    cat /tmp/err
    status=1
  fi
}

same plain data/expenses.csv
same budgets data/expenses.csv --budgets data/budgets.json --month 2026-02
same top5 data/expenses.csv --budgets data/budgets.json --top 5
same data2-budgets data2/expenses.csv --budgets data2/budgets.json
same data2-top2 data2/expenses.csv --top 2
same empty data2/empty.csv
error "" missing.csv
error "line 3" data2/bad.csv
error "" data2/expenses.csv --budgets data2/budgets.json --month 2026-4
exit $status
