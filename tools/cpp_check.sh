#!/bin/sh
# Check a C++ track source in the allowlisted GCC image against known answers: tools/cpp_check.sh c main.cpp [x86]
t=$1; src=$2; root=$(cd "$(dirname "$0")/../.." && pwd)
case $t in a) o=summary.txt;; b) o=results.txt;; c) o=aggregates.txt;; d) o=distances.txt;; esac
img=gcc:15@sha256:eb5e64286ad47187b32bda75c6b6916166ecb41da56559b8b09e0001748d2cce
plat=""; arch="-march=native"; [ "$3" = x86 ] && plat="--platform linux/amd64" && arch="-march=x86-64-v3"
suffix=$3; rm -rf "$root/.work/cpp-$t"; mkdir -p "$root/.work/cpp-$t"
docker run --rm $plat --network=none -v "$root/$src:/s.cpp:ro" -v "$root/.work/cpp-$t:/b" "$img" g++ -std=c++20 -O3 $arch -flto -static -pthread -o /b/solution$suffix /s.cpp || exit 1
run() { docker run --rm $plat --network=none -e SD_OUT=/o --tmpfs /o -v "$root/.work/cpp-$t:/b:ro" -v "$1:/data:ro" "$img" sh -c "/b/solution$suffix && sha256sum /o/$o | cut -d' ' -f1"; }
c() { [ "$2" = "$3" ] && echo "ok   $1" || echo "FAIL $1"; }
sh1() { shasum -a 256 "$1" | cut -d' ' -f1; }
c worked "$(run "$root/repo/tools/cases/$t")" "$(sh1 "$root/repo/tools/cases/$t/expected")"
for s in tiny sample; do c "$s" "$(run "$root/gen/$t/$s")" "$(grep "^$t $s " "$root/refs/expected-small.txt" | cut -d' ' -f3)"; done
for x in tiny-2 sample-2 tiny-3 sample-3 tiny-4 sample-4; do c "$x" "$(run "$root/gen-seeds/$t/$x")" "$(sh1 "$root/gen-seeds/$t/$x.ref/$o")"; done
echo "full $(run "$root/data/$t")"
