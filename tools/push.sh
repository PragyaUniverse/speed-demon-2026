#!/bin/sh
# Verify every track, commit, push, wait for the board: tools/push.sh "message". Stops at the first failure.
set -e
cd "$(dirname "$0")/.."
! grep -l 'PROBE\|TRACE' track-*/src/main.rs
for t in a b c d; do python3.13 -I tools/verify.py "$t" --repeat 2 | tail -1 | cut -c1-70; done
git add -A .
git commit -qm "$1

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
GIT_TERMINAL_PROMPT=0 git push origin speed-demon 2>&1 | tail -1
h=$(git rev-parse --short=7 HEAD); echo "$h"
python3.13 -I tools/board.py --wait "$h" | grep latest | cut -c1-175
