#!/bin/sh
# Regenerate a track's PGO profile (x86-64 under Rosetta, in the image that ships llvm-profdata), trained
# on public generator output at a seed the board does not use: tools/pgo.sh a [seed-scale dir under gen-seeds/<t>]
# Run this after the last code change: a stale profile silently stops applying to edited functions.
set -e
t=$1; root=$(cd "$(dirname "$0")/../.." && pwd); data="$root/gen-seeds/$t/${2:-sample-2}"
img=halloumee/speed-demon-rust:1.99.1-trixie-llvm-tools@sha256:13e4ee7135d8eac6c56476c7d1aee201f36479f4745fd5d0e6b4529f4e13251a
mkdir -p "$root/.work/pgo-$t"
rm -f "$root/repo/track-$t/pgo.profdata"
sed -i '' '/profile-use/d' "$root/repo/track-$t/.cargo/config.toml"
printf '[target.x86_64-unknown-linux-gnu]\nrustflags = ["-C", "target-cpu=x86-64-v3", "-C", "target-feature=+crt-static"]\n' > "$root/.work/pgo-$t/base.toml"
docker run --rm --platform linux/amd64 --network=none -v "$root/repo/track-$t:/src:ro" -v "$root/.work/pgo-$t:/out" -v "$data:/data:ro" \
  -e CARGO_TARGET_DIR=/tmp/t -e RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static -C profile-generate=/tmp/pgo" -w /src "$img" sh -c '
    cargo build --release --offline --quiet
    mkdir -p /tmp/o
    for i in 1 2 3; do SD_OUT=/tmp/o TRACK=x /tmp/t/release/solution; done
    "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-profdata merge -o /out/pgo.profdata /tmp/pgo'
cp "$root/.work/pgo-$t/pgo.profdata" "$root/repo/track-$t/pgo.profdata"
cat > "$root/repo/track-$t/.cargo/config.toml" <<'C'
# x86-64-v3 is exactly what the scoring CPU offers (AVX2, BMI2, no AVX-512), whichever machine builds.
# Static linking skips the dynamic loader. The profile comes from tools/pgo.sh, on generator output.
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "target-cpu=x86-64-v3", "-C", "target-feature=+crt-static", "-C", "profile-use=/src/pgo.profdata"]
C
ls -la "$root/repo/track-$t/pgo.profdata"
