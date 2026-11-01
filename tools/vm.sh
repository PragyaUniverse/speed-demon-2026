#!/bin/sh
# Fast local loop: compile a track for Linux in the allowlisted image, then time it N times directly
# inside the Colima VM on the VM's own disk, with no container start-up. tools/vm.sh a [runs] [flags]
t=$1; runs=${2:-11}; flags=${3:--C target-cpu=native -C target-feature=+crt-static}
root=$(cd "$(dirname "$0")/../.." && pwd)
docker run --rm --network=none -v "$root/repo/track-$t:/src:ro" -v "sd-target-$t:/target" -e CARGO_TARGET_DIR=/target -e RUSTFLAGS="$flags" -w /src \
  rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb \
  sh -c 'cargo build --release --offline --quiet --config "build.rustflags=[]" 2>&1 | grep -v warning: | head -40; cp /target/release/solution /target/solution' || exit 1
colima ssh -- sudo sh -c "cp /var/lib/docker/volumes/sd-target-$t/_data/solution /var/sd/sol-$t; chmod 755 /var/sd/sol-$t; mkdir -p /dev/shm/sdout; chmod 777 /dev/shm/sdout; cat /var/sd/$t/* >/dev/null"
colima ssh -- env SD_DATA=/var/sd/$t SD_OUT=/dev/shm/sdout SD_RUNS=$runs SD_BIN=/var/sd/sol-$t python3 -c '
import os,subprocess,time,hashlib,resource,statistics as st
env=dict(os.environ); b=os.environ["SD_BIN"]; ts=[]
for i in range(int(os.environ["SD_RUNS"])):
    t=time.perf_counter(); p=subprocess.run([b],env=env,capture_output=True); ts.append(time.perf_counter()-t)
    assert p.returncode==0,p
print(p.stderr.decode()[-1500:],end="")
d="/dev/shm/sdout"; f=os.listdir(d)[0]
print("median %.2f ms  best %.2f ms  maxrss %.1f MB  %s %s"%(st.median(ts)*1e3,min(ts)*1e3,resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss/1e3,f,hashlib.sha256(open(d+"/"+f,"rb").read()).hexdigest()[:16]))
os.remove(d+"/"+f)'
