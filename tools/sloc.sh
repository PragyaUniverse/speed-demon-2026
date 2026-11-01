#!/bin/sh
# Source size of a track as the board counts it: rustfmt defaults, then scc code lines, Dockerfile included.
t=$1; root=$(cd "$(dirname "$0")/.." && pwd); d=$(mktemp -d)
cp -r "$root/track-$t/src" "$root/track-$t/Dockerfile" "$d/"
rustfmt --edition 2021 "$d"/src/main.rs
scc --no-cocomo --no-complexity "$d" | awk '/^(Rust|Dockerfile) /{s+=$NF} END{print s}'
rm -rf "$d"
