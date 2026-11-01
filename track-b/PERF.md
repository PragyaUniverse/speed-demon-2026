# Track B: performance log

"Mac", "VM" and "Harness" mean the same as in Track A's log. The VM has no AVX2, so its tokeniser is a byte-at-a-time fallback and overstates the scan. Reference on the box: 56.35 s, 2,200 MB. Run IDs are filled in as board runs happen, and none has been measured yet.

Before writing any code I measured the seed-1 data: 53,444 distinct BOOL terms with 21M (term, document) pairs, but anchor occurrences for the phrases total only 835K. A global index would need about 84 MB of lists, so the design avoids one.

| Step | Change | Run ID | T local | M (MB) | Result |
|---|---|---|---|---|---|
| 1 | Per-chunk engine with anchored phrase verification, 4 MB chunks | pending | 166 ms VM | 68 harness | All hashes correct first time, against the worked example, tiny, sample and full |
| 2 | 2 MB chunks | pending | 160 ms VM | 48 | Same time, a lot less memory |
| 3 | 1 MB chunks | pending | 163 ms VM | 41 | Below the 44 MB point where memory stops earning, at no measurable cost in the VM. To be rechecked on the box, where query evaluation is a bigger share of the run |
