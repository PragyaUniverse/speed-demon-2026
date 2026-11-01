# Track C: Join

**Approach.** The parser walks one field at a time. Numbers, the amount's sign and the CRLF-or-LF terminator are handled without branches, because all three are unpredictable on this data. Quoted text uses AVX2 to jump to the next quote and treats `""` as a rare slow path. A worker can start at any file offset without tracking quote state: per spec section 2.2 generated text has no digits and no non-ASCII bytes, so an LF followed by a digit or a BOM is always a record start and an LF inside a quoted note never is. A BOM is skipped only at a record start, so a U+FEFF inside quotes stays data. The join is a flat `region_id -> group` array.

**names_distinct.** Names are compared by their raw bytes between quotes, which is exact because `""` is the only escape and unquoted text cannot contain a quote. Each name gets a dense id from a shared table of 2^18 slots, matching on all 64 bits of a hash, with about a 1e-10 chance of any false match in a run. A bit matrix records (name id, group). Each cache line of that matrix has one owning worker, and pairs travel to the owner in batches, so the matrix stays in one core's L2 instead of bouncing between cores on every record.

Huge pages are disabled for the process: on the box they tripled peak memory.

**Dependencies.** None.

**With 24 more hours.** Parallelise the region load, try a prefetching batch around the name table, and retest the owner scheme against plain atomics on the real box, where cache-to-cache costs differ from the VM's.
