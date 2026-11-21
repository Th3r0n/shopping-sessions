# Benchmarks

Local machine: i7-12700K (Docker Desktop on WSL2, 15.5 GiB VM, kernel 6.6, THP `madvise`); container pinned to
CPUs 0-7 (4 P-cores with hyperthreading), `--cpus 8 --memory 14g`, data in the `ssdata` volume, page cache warm.
`scripts/bench.py` (median of 5) unless noted. Scorer time has been about 1.17-1.3 × the local container time.

Phase columns: wall seconds of the phase, then (user + sys) / (wall × 8) = parallel efficiency, and sys share.

## v1-baseline (e196257, scored 1.10 s)

### p20 (21.50 M event lines, 5.15 M clicks, 0.89 M links)

Container median **0.938 s** (best 0.897), fixed cost (`SS_NOOP=1`) ~0.25 s, internal 0.642 s, peak RSS 2.74 GB.

| phase | wall | user | sys | faults | efficiency | sys share |
| --- | --- | --- | --- | --- | --- | --- |
| links parsed | 0.008 | 0.014 | 0.005 | 4.8 k | 30 % | 26 % |
| events + clicks parsed (names on thread 0: 0.272 s) | 0.330 | 2.087 | 0.383 | 57 k | 94 % | 16 % |
| deduplicated | 0.126 | 0.771 | 0.063 | 1.7 k | 83 % | 8 % |
| names resolved (2b) | 0.034 | 0.230 | 0.021 | 0 | 92 % | 8 % |
| sessionized | 0.115 | 0.856 | 0.041 | 33 k | 97 % | 5 % |
| written | 0.025 | 0.015 | 0.127 | 31 k | 71 % | 89 % |

### p60 (64.1 M event lines, 15.0 M valid clicks, 2.66 M links)

Container median **2.583 s** (best 1.989, worst 2.987: the VM is short of memory at 60M), fixed ~0.29 s,
internal 2.148 s, peak RSS 7.9 GB, so ~0.15 s of exit teardown.

| phase | wall | user | sys | faults | efficiency | sys share |
| --- | --- | --- | --- | --- | --- | --- |
| links parsed | 0.014 | 0.043 | 0.026 | 12 k | 62 % | 38 % |
| events + clicks parsed (names on thread 0: 0.955 s) | 1.130 | 6.889 | 0.946 | 170 k | 87 % | 12 % |
| deduplicated | 0.379 | 2.221 | 0.195 | 3.7 k | 80 % | 8 % |
| names resolved (2b) | 0.129 | 0.849 | 0.153 | 0 | 97 % | 15 % |
| sessionized | 0.374 | 2.684 | 0.289 | 62 k | 99 % | 10 % |
| written | 0.118 | 0.126 | 0.549 | 92 k | 71 % | 81 % |

### /output on disk vs tmpfs (p20, 8 interleaved rounds)

Named volume (disk) median 1.154 s, tmpfs 0.832 s. The process lifetime (`docker inspect` StartedAt to FinishedAt)
is the same in both modes (0.66-0.72 s): the ~0.3 s is spent by Docker after the process exits, and it grows with
the output size (dev, 6 MB of output: +0.04 s). A plain `dd` of 115 MB to the same volume does not show it, so the
cause is not understood. A/B decisions use tmpfs (less noise); a smaller output (dictionaries) is a cheap hedge in
case the scorer behaves the same.

## Oracles

`../data/v1-p20` (equals `../data/out-p20`) and `../data/v1-p60` (equals the earlier `../data/v24-p60`).

## Prototype front-end (proto/, adapted to base 36 IDs), p20, one thread, i7-12700K

Levels: 0 stage 1 only 8.8 ns/line; 1 + lines and quotes 12.8; 2 + both IDs (base 36) 18.9; 3 + ts and ingest 29.0;
4 + validation, batch, dedup bitmap (2^29 bits), names (filter + 64 MiB table, 40% renamed) 45.4; 5 + 16-byte
scatter into 1024 partitions 54.1 (1.16 s for 21.5 M lines).

Current binary, `NCPU=1`, same scope (events parsed - names built 0.085 s, + deduplicated 0.46 s + names resolved
0.12 s), still including the 5.1 M click lines and the mmap faults: 2.1-2.5 s, so about 1.7-1.9 times the prototype.
The prototype leaves out the dedup resolution pass, real name probes, exotic interning and file boundaries.

## Tokenizer, p20, one thread (proto levels 10/11 run src/input.rs on the same buffer)

Current `simd_range` 19.4 ns/line (with ts::ingest + ts::parse: 30.4); prototype stages 1-2 12.8 ns/line (with
both base 36 IDs and the SSE F1 path: 29.0; with src/ts.rs instead of f1_fast: 32.9). So step 2 would save about
10 ns/line, ~0.2 s of CPU at 20M (~0.045 s wall, ~5%), not the 8-10% expected: postponed to the tuning step.
