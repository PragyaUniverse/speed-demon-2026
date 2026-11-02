#!/bin/sh
# Wait for the board on HEAD, then re-run the same code N-1 more times: tools/samples.sh 3
cd "$(dirname "$0")/.."
fmt() { sed -E "s/^([a-d]) latest .*'run_id': '([^']+)'.*'stage': '([a-z]+)'.*'T_s': ([0-9.]+), 'M_bytes': ([0-9]+), 'S': ([0-9]+).*'K': ([0-9.a-zA-Z]+).*/\1 \2 \3 T=\4 M=\5 S=\6 K=\7/" | cut -c1-90; }
for i in $(seq 1 "$1"); do
  [ "$i" -gt 1 ] && { git commit -q --allow-empty -m "Re-run the same code to sample board noise

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"; GIT_TERMINAL_PROMPT=0 git push -q origin speed-demon; }
  sleep 5
  python3.13 -I tools/board.py --wait "$(git rev-parse --short=7 HEAD)" | grep latest | fmt
done
