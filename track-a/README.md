# Track A: Ingest

**Approach.** The log is mapped, not read, so the input never counts toward memory. Workers claim 24 newline-aligned file segments. Each line is parsed with constant-time word tricks and becomes one packed `u32` (endpoint slot, error bit, latency), chained into per-hour blocks in that segment's own arena. Endpoints go through a lock-free table that is effectively read-only after the first few thousand lines. A second parallel phase takes one hour at a time, lays its latencies out per endpoint, selects p50/p95/p99 with `select_nth_unstable`, and writes the hour's text directly at its final file offset. Because the log is roughly time-ordered, a segment's last hour finishes early, and its arena goes back to the kernel while the output is still growing.

**Dependencies.** None. `mmap` and `madvise` are declared by hand; `std` links libc anyway.

**Assumptions from the spec.** Exactly 1,842 endpoints (the 4,096-slot table stays under half full), timestamps inside the fixed 72-hour span, well-formed lines.

**With 24 more hours.** A hand-written AVX2 compare for endpoint equality instead of `memcmp`, a test of whether profile-guided optimisation pays when the whole run takes milliseconds, and shrinking the per-worker scratch that sets the peak at the start of phase 2.
