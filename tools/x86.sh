#!/bin/sh
# Compile a track for linux/amd64 (the scoring architecture, so the AVX2 paths are the ones built) and run
# it under Rosetta on each scale, printing the output hash: tools/x86.sh a
t=$1; root=$(cd "$(dirname "$0")/../.." && pwd)
case $t in a) o=summary.txt;; b) o=results.txt;; c) o=aggregates.txt;; d) o=distances.txt;; esac
img=rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb
docker run --rm --platform linux/amd64 --network=none -v "$root/repo/track-$t:/src:ro" -v "$root/.work/x86-$t:/out" -e CARGO_TARGET_DIR=/tmp/t -w /src "$img" \
  sh -c 'cargo build --release --offline --quiet && cp /tmp/t/release/solution /out/solution' || exit 1
for d in gen/$t/tiny gen/$t/sample data/$t; do
  docker run --rm --platform linux/amd64 --network=none -e SD_OUT=/res -e TRACK=$t -v "$root/.work/x86-$t:/b:ro" -v "$root/$d:/data:ro" --tmpfs /res "$img" \
    sh -c "/b/solution && echo $d \$(sha256sum /res/$o | cut -d' ' -f1)"
done
