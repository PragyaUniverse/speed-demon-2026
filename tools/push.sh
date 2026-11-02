#!/bin/sh
# Verify every track strictly, commit, push: tools/push.sh "message". Stops at the first failure.
# Rust tracks go through verify.py; a C++ track must pass every known answer in the GCC image.
set -e
cd "$(dirname "$0")/.."
! grep -l 'PROBE\|TRACE' track-*/src/main.rs track-*/main.cpp 2>/dev/null
for t in a b c d; do
  if [ -f "track-$t/main.cpp" ]; then
    r=$(tools/cpp_check.sh "$t" "repo/track-$t/main.cpp" 2>&1 | grep -E '^ok|^FAIL|^full')
    echo "$r" | grep -q FAIL && { echo "$r"; echo "FAILED $t"; exit 1; }
    want=$(grep -E "\"$t\": \"" tools/verify.py | sed -E 's/.*: "([0-9a-f]+)".*/\1/')
    echo "$r" | grep -q "full $want" || { echo "FAILED full $t"; exit 1; }
    echo "ok   $t cpp"
  else
    python3.13 -I tools/verify.py "$t" --repeat 3 | tail -1 | cut -c1-70
  fi
done
git add -A .
git commit -qm "$1

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
GIT_TERMINAL_PROMPT=0 git push origin speed-demon 2>&1 | tail -1
git rev-parse --short=7 HEAD
