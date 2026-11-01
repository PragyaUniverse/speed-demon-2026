#!/bin/sh
# Run check.py (the board's sandbox) on tiny and sample against the reference's hashes: tools/check_small.sh a
t=$1; root=$(cd "$(dirname "$0")/../.." && pwd)
grep "^$t " "$root/refs/expected-small.txt" | while read -r _ scale hash; do
  echo "$scale: $(python3 "$root/repo/check.py" "$t" --data "$root/gen/$t/$scale" --expected "$hash" 2>&1 | tail -1)"
done
