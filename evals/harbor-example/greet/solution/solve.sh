#!/bin/bash
set -euo pipefail
cat > /app/greet.py <<'EOF'
import sys

for name in sys.argv[1:] or ["world"]:
    print(f"Hello, {name}!")
EOF
