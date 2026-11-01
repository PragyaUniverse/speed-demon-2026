#!/bin/sh
# Score a track locally with the organisers' own harness (Colima, arm64): tools/score.sh a [runs]
t=$1; runs=${2:-5}
case $t in a) o=summary.txt; h=495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3;; b) o=results.txt; h=f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927;; c) o=aggregates.txt; h=d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536;; d) o=distances.txt; h=a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e;; esac
root=$(cd "$(dirname "$0")/../.." && pwd)
# Inputs live on the VM's own disk (/var/sd/<track>); warm them there, as the box's vmtouch does,
# so /data is not charged to the run.
colima ssh -- sh -c "cat /var/sd/$t/* > /dev/null"
python3.13 -I "$root/refs/public/harness/run_submission.py" --context "$root/repo/track-$t" --track "$t" --data "/var/sd/$t" --output "$o" --expected-sha256 "$h" --timeout-s 300 --timer "$root/refs/public/harness/timer/sd-timer-arm64" --work "$root/.work" --cgroups colima --runs "$runs" --results "$root/.work/result-$t.json" >/dev/null
python3.13 -I -c "
import json,sys
r=json.load(open(sys.argv[1]))
print(r.get('T_s'), round(r.get('M_bytes',0)/1e6,1), r.get('gates')); print([(round(x.get('wall_s',0),4), round(x.get('peak_bytes',0)/1e6,1)) for b in r.get('batches',[]) for x in b])
" "$root/.work/result-$t.json"
