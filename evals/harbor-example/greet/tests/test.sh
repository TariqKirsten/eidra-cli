#!/bin/bash
# Writes 1 to /logs/verifier/reward.txt when greet.py behaves as instruction.md says, 0 otherwise.
mkdir -p /logs/verifier
echo 0 > /logs/verifier/reward.txt

check() {
  local want=$1
  shift
  local got
  got=$(python3 /app/greet.py "$@" 2>&1)
  if [ "$got" != "$want" ]; then
    printf 'greet.py %s\n  want: %q\n  got:  %q\n' "$*" "$want" "$got"
    return 1
  fi
}

check "Hello, world!" &&
  check "Hello, Ada!" Ada &&
  check $'Hello, Ada!\nHello, Grace!' Ada Grace &&
  echo 1 > /logs/verifier/reward.txt
cat /logs/verifier/reward.txt
