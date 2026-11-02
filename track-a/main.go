// Track A in Go: a control for the language comparison. Workers parse segments into per-hour packed
// records with local endpoint tables; hours are then finished in parallel and written in order.
package main

import (
	"bytes"
	"os"
	"slices"
	"strconv"
	"sync"
	"sync/atomic"
)

const hours, segments, chunk = 72, 48, 1 << 18

type rec struct{ latency, tag uint32 }

var days = [4]string{"2026-10-30T", "2026-10-31T", "2026-11-01T", "2026-11-02T"}

func env(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}

func lineStart(f *os.File, size, from int64) int64 {
	if from <= 0 || from >= size {
		return min(max(from, 0), size)
	}
	var probe [256]byte
	n, _ := f.ReadAt(probe[:], from)
	if i := bytes.IndexByte(probe[:n], '\n'); i >= 0 {
		return from + int64(i) + 1
	}
	return from + int64(n)
}

func main() {
	data, out := env("SD_DATA", "/data"), env("SD_OUT", "/out")
	f, err := os.Open(data + "/requests.log")
	if err != nil {
		panic(err)
	}
	st, _ := f.Stat()
	size := st.Size()
	const workers = 12
	var claim atomic.Int64
	names := make([][]string, workers)
	parts := make([][][]rec, workers)
	var wg sync.WaitGroup
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			local := map[string]uint32{}
			byHour := make([][]rec, hours)
			buf := make([]byte, chunk)
			for {
				i := claim.Add(1) - 1
				if i >= segments {
					break
				}
				pos, end := lineStart(f, size, i*size/segments), lineStart(f, size, (i+1)*size/segments)
				for pos < end {
					want := min(end-pos, chunk)
					if _, err := f.ReadAt(buf[:want], pos); err != nil {
						panic(err)
					}
					whole := bytes.LastIndexByte(buf[:want], '\n') + 1
					for p := buf[:whole]; len(p) > 0; {
						day := int(p[9]&15) + 1 - int(p[8]&1)
						bucket := day*24 + int(p[11]&15)*10 + int(p[12]&15) - 18
						q := p[28:]
						q = q[bytes.IndexByte(q, ' ')+1:]
						n := bytes.IndexByte(q, ' ')
						id, ok := local[string(q[:n])]
						if !ok {
							id = uint32(len(local))
							local[string(q[:n])] = id
						}
						isErr := uint32(0)
						if q[n+1] == '5' {
							isErr = 1
						}
						q = q[n+5:]
						v, k := uint32(0), 0
						for ; q[k] != '\n'; k++ {
							v = v*10 + uint32(q[k]-'0')
						}
						byHour[bucket] = append(byHour[bucket], rec{v, id<<1 | isErr})
						p = q[k+1:]
					}
					pos += int64(whole)
				}
			}
			list := make([]string, len(local))
			for s, id := range local {
				list[id] = s
			}
			names[w], parts[w] = list, byHour
		}(w)
	}
	wg.Wait()
	global := map[string]int{}
	for _, list := range names {
		for _, s := range list {
			global[s] = 0
		}
	}
	order := make([]string, 0, len(global))
	for s := range global {
		order = append(order, s)
	}
	slices.Sort(order)
	for i, s := range order {
		global[s] = i
	}
	remap := make([][]int32, workers)
	for w, list := range names {
		remap[w] = make([]int32, len(list))
		for i, s := range list {
			remap[w][i] = int32(global[s])
		}
	}
	texts := make([][]byte, hours)
	var next atomic.Int64
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			counts, errors := make([]int32, len(order)), make([]int32, len(order))
			starts := make([]int32, len(order)+1)
			var lat []uint32
			for {
				h := int(next.Add(1) - 1)
				if h >= hours {
					return
				}
				clear(counts)
				clear(errors)
				total := int32(0)
				for o := range workers {
					for _, r := range parts[o][h] {
						g := remap[o][r.tag>>1]
						counts[g]++
						errors[g] += int32(r.tag & 1)
						total++
					}
				}
				run := int32(0)
				for i, c := range counts {
					starts[i] = run
					run += c
				}
				if int(total) > cap(lat) {
					lat = make([]uint32, total)
				}
				lat = lat[:total]
				for o := range workers {
					for _, r := range parts[o][h] {
						g := remap[o][r.tag>>1]
						lat[starts[g]] = r.latency
						starts[g]++
					}
				}
				text := make([]byte, 0, 1<<18)
				for g, name := range order {
					n := int(counts[g])
					if n == 0 {
						continue
					}
					group := lat[int(starts[g])-n:starts[g]]
					slices.Sort(group)
					at := func(p int) uint64 { return uint64(group[(p*n+99)/100-1]) }
					e := uint64(errors[g])
					text = append(text, days[(h+18)/24]...)
					hh := (h + 18) % 24
					text = append(text, byte('0'+hh/10), byte('0'+hh%10))
					text = append(text, ":00:00Z "...)
					text = append(text, name...)
					for _, v := range []uint64{uint64(n), at(50), at(95), at(99), e, e * 1000000 / uint64(n)} {
						text = append(text, ' ')
						text = strconv.AppendUint(text, v, 10)
					}
					text = append(text, '\n')
				}
				texts[h] = text
			}
		}()
	}
	wg.Wait()
	o, err := os.Create(out + "/summary.txt")
	if err != nil {
		panic(err)
	}
	for _, t := range texts {
		o.Write(t)
	}
	o.Close()
}
