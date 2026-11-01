#!/bin/sh
# Instructions per source line (callgrind, arm64 VM, one thread): tools/profile.sh c gen/c/sample [lines]
# The AVX2 paths do not run here, so hand-written SIMD shows as its scalar fallback.
t=$1; data=$2; lines=${3:-45}; root=$(cd "$(dirname "$0")/../.." && pwd)
docker run --rm --network=none -v "$root/repo/track-$t:/src:ro" -v "sd-prof-$t:/target" -e CARGO_TARGET_DIR=/target -e RUSTFLAGS="-C target-cpu=native -C debuginfo=line-tables-only" -w /src \
  rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb \
  sh -c 'cargo build --release --offline --quiet --config profile.release.debug=1 2>&1 | grep -v warning | head; cp /target/release/solution /target/solution'
cat > "$root/.work/prof-run.sh" <<S
mkdir -p /var/sd/prof /dev/shm/po /src/src
cp /var/lib/docker/volumes/sd-prof-$t/_data/solution /var/sd/prof/sol-$t
cp "$root/$data"/* /var/sd/prof/
cp "$root/repo/track-$t/src/main.rs" /src/src/main.rs
cd /var/sd/prof
SD_DATA=/var/sd/prof SD_OUT=/dev/shm/po taskset -c 0 valgrind --tool=callgrind --callgrind-out-file=/var/sd/prof/cg.out ./sol-$t 2>&1 | grep -E 'Collected'
callgrind_annotate --threshold=100 --auto=yes --include=/src/src /var/sd/prof/cg.out 2>/dev/null > /var/sd/prof/ann.txt
grep -E '^ *[0-9,]+ \(' /var/sd/prof/ann.txt | grep -v '=>' | grep -v '/rustc/' | grep -v 'main.rs:' | sort -t'(' -k2 -rn | head -$lines
grep -E '^ *[0-9,]+ \(' /var/sd/prof/ann.txt | grep '/rustc/' | grep -v '=>' | sed -E 's#/rustc/[0-9a-f]+/library/##' | sort -t'(' -k2 -rn | head -6
rm -f /var/sd/prof/*.csv /var/sd/prof/*.txt /var/sd/prof/*.log
S
colima ssh -- sudo sh "$root/.work/prof-run.sh"
