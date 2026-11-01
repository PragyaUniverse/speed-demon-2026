# Track A: performance log

Every number below was measured locally unless a run ID is given. "Mac" is `tools/verify.py` natively on an Apple M4 Pro. "VM" is `tools/vm.sh`, a Linux VM on the same machine, 12 vCPUs, output on tmpfs like the board's. "Harness" is the organisers' `run_submission.py` in that VM (`tools/score.sh`), which also reads the real `memory.peak`. The VM has no AVX2, so its numbers use the slower SWAR `until_space`; the board's will differ. Reference on the box: 32.05 s, 228 MB. Run IDs are filled in as board runs happen, and none has been measured yet.

| Step | Change | Run ID | T local | M (MB) | Result |
|---|---|---|---|---|---|
| 0 | Reference through `check.py` | n/a | 11.93 s Mac | | Confirmed setup and hash |
| 1 | Simple Rust: `HashMap`, full sort, `read_to_string` | pending | 370 ms Mac | 303 RSS | Correct baseline, 32x the Python on the same machine |
| 2 | mmap, 12 workers, packed per-hour records, shared interner, select instead of sort | pending | 20.5 ms VM | 33.3 harness | About 18x faster. Most of the win is parallelism plus never allocating per line |
| 3 | Try `MAP_POPULATE` | | 23.7 vs 20.5 ms VM | | **Slower, reverted.** Workers fault file pages about as cheaply in parallel, and populate serialises them first |
| 4 | Try writing through a shared mapping of the output, to avoid the inode lock in `pwrite` | | 35 vs 20 ms VM, 26 to 91 ms spread | 25 | **Much slower and erratic, reverted.** Page faults on a shared tmpfs mapping contended badly in the VM. Worth retrying on the box only if a board run shows the serial write is a real cost |
| 5 | Free each hour's buffers as it finishes, with `M_MMAP_THRESHOLD` so frees really unmap | | 21 vs 17 ms in the output phase | 24 | Memory down, time up: each `munmap` is a TLB shootdown across all 12 threads, about 120 of them. **Replaced by step 6** |
| 6 | Segment arenas: chained blocks instead of per-hour `Vec`s, one `madvise` per segment when its last hour is done, 24 calls in total | pending | 17.2 ms VM | 21.6 harness | Faster **and** smaller than step 2: no reallocation copying and almost no shootdowns. Works because the log is roughly time-ordered, so a segment's hours finish together |

Things kept from the measurements: two-digit division in the number formatter, and `x86-64-v3` with static linking in `.cargo/config.toml`. The static binary skips the dynamic loader, whose cost is not negligible when the whole run takes milliseconds. Its benefit is unmeasured so far, because the local VM is arm64.
