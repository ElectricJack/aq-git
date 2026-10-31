# Bounded warm batch diagnostic

Unmodified release binary (root-reported SHA256 1d3edfa5f6137d06abe1be5c176855f91cea1a1bca19a2d9c46015c3537d056f), existing stopped warmed state `/tmp/aq-git-benchmark/state-20261031T012210Z`, canonical final-audited-warm/queries.json, canonical benchmark `measure`, 32 process clients, 100-item batches, 60 seconds. No concurrent team builds/measurements; host CPU PSI remained nonzero. No production edits, no fixture history regeneration. perf was unavailable on PATH.

196,614 successful batches / 60.046s = 3274.41/s; p50 8.899ms, p95 14.607ms, p99 19.871ms; zero errors. Original failure (p95 37.919ms) did not reproduce.

Engine thread count stabilized at37 (main+4 Tokio I/O+32 blocking). First four runtime threads consumed 54.79–55.30 CPU seconds each over ~60 seconds (~92% occupancy). Each blocking thread consumed ~10–11.3 seconds. I/O CPU saturation is directly observed; exact encoding/drop/decoding split is not sampled. Source places frame parsing, response encoding, and response Value destruction on I/O threads. Moving response encoding and destruction into the existing bounded blocking closure is therefore a targeted candidate; no claim that it alone caused the original failure.

Engine RSS at10/20/30/40/50/60s: 57864/58232/57272/57652/57072/57312 KiB. Engine RssAnon roughly53–54MiB throughout. Aggregate sampled peak65196KiB, final61608KiB; last-decile drift -291KiB. VmData rose144492→154624KiB (virtual reservations), without corresponding steady RSS growth. No demonstrated duration-dependent resident leak.

Cache remained1000 entries/4,057,762 bytes; hits+19,661,400, no added misses, evictions, object exchanges, or Git spawns. Coordination entries1→2. Four-I/O-thread CPU bottleneck, fixed thread population, and stable logical cache exclude Git command runtime/cache growth as causes of this run's throughput ceiling. Allocator fragmentation or original host contention remains unproven.

Artifacts: profile.py (driver), result.json, threads.json (per-second per-thread ticks/status/CPU PSI), samples.csv.*.csv and samples.csv.resources.csv (canonical samples), service.log. Owned service shut down cleanly.

This diagnostic started a fresh service and measured batches without first running the failing sequence's 60-second read phase. It therefore cannot distinguish service-history/allocator effects from host-load effects in the earlier miss.
