# Performance and functional evidence

These are diagnostics on the declared 100,000-commit/10,000-ref fixture plus a second 10,000-commit repository, using 1,000 exact keys in the 40/40/10/10 mix. The initial comparisons use five-second cohorts with two repetitions; later sections identify their actual durations and sample counts. They do **not** certify the complete five-repeat or 30-minute release gates.

## Before and after cache/batch allocation changes

Ranges span the two repetitions. Batch throughput is requests/s; each request has 100 logical queries. The optimized version below still used the earlier automatically sized I/O pool.

| Read | Clients | Before p95 ms | After p95 ms | Before p99 ms | After p99 ms | Before requests/s | After requests/s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| read | 1 | 0.238–0.242 | 0.247–0.310 | 0.281–0.284 | 0.289–0.392 | 4964–4971 | 4121–4899 |
| read | 8 | 0.802–0.812 | 0.818–0.895 | 1.179–1.195 | 1.218–1.319 | 21832–22051 | 19264–21567 |
| read | 32 | 5.128–5.197 | 5.453–5.494 | 8.768–8.823 | 9.238–9.312 | 22164–22659 | 20504–21089 |
| batch | 1 | 8.498–9.460 | 2.247–2.834 | 9.530–11.934 | 3.149–4.279 | 126–131 | 469–574 |
| batch | 8 | 12.698–13.070 | 4.510–4.691 | 15.243–15.252 | 6.214–6.427 | 765–771 | 2321–2402 |
| batch | 32 | 49.087–49.211 | 22.742–31.378 | 55.999–57.202 | 27.865–64.479 | 787–804 | 1567–1969 |

The allocation changes substantially reduced batch work, but neither initial version met every absolute 32-client latency target. Aggregate Git speedups are dominated by cached ancestry; the report does not claim every metadata class is ten times faster. Per-class and persistent-Git baseline results remain in the JSON.

## Isolated I/O worker experiment

Both experiments use the exact same optimized binary; only `TOKIO_WORKER_THREADS` differs. Those historical binaries honored the variable. Newer binaries advertise a fixed `limits.socket_io_workers` and ignore that override.

| I/O workers | Read p95 ms | Read p99 ms | Read requests/s | Batch p95 ms | Batch p99 ms | Batch requests/s |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 2.232–2.356 | 2.566–2.745 | 20796–21916 | 26.394–26.532 | 30.957–31.702 | 1790–1799 |
| 4 | 2.441–2.496 | 2.978–3.094 | 21028–21386 | 24.192–25.822 | 29.330–31.312 | 1808–1875 |

## Later comprehensive candidate: incomplete and host-contended

`reference-final-diagnostic` used explicit four-worker I/O, bounded pack settings, FIFO child admission, and event-driven Git I/O. It completed read/batch/cold cohorts and all 18 agent-loop cohorts, then failed during the second mixed setup after an idle control connection closed. Its first mixed cohort exposed two functional defects: a spurious framing timeout during a long request and local upload-pack repacking under the 1 GiB address-space limit. Those failures are preserved, not discarded. Subsequent fixes require a fresh functional diagnostic.

Independent qualification builds were observed at **2026-10-30 23:53:15 UTC**, with load **49.92** on 24 allowed CPUs (five Cargo and six rustc processes in the captured sample). No unrelated process was stopped. Exact onset before the sample is unknown; late timings are host-contended. Cache-lock contention is a hypothesis, not an established cause of every timing miss. See [host-contention.json](reference-final-diagnostic/host-contention.json).

The preceding optimized cold 32-client run recorded exactly **601** `overloaded` errors, all `Git child limit reached`: 413 ancestry, 97 tree, and 91 diff requests. At one client, inspection and ancestry met the numerical miss bound while tree/diff did not. Their p95 values were tree **11.51–14.64 ms**, diff **11.62–13.90 ms**, versus process Git tree **1.47–1.69 ms**, diff **1.94–2.15 ms**. FIFO/event-driven changes were subsequently measured in the comprehensive candidate; its detailed cold outcomes remain in that result file.

## Functional resource diagnosis

- [Pack resource evidence](pack-resource-diagnostic/results.json): the exact managed-pool export failed to create pack threads under 1 GiB. Setting two pack threads, 16 MiB window memory, and 16 MiB delta cache produced the full 22,714,518-byte bundle without changing the quota.
- [Import resource evidence](import-resource-diagnostic/results.json): fetching the validated quarantine directory failed in local upload-pack under the same 1 GiB quota. Importing the original digest-checked bundle with `bundle unbundle` succeeded in 2.30 seconds, advertised exactly the expected source/ref, and exposed the source as a commit.

## Derived ancestry index experiment

The [commit-graph diagnostic](commit-graph-diagnostic/results.json) used a private managed-style repository and left the raw Git baseline fixture unchanged. Sixteen exact ancestry answers matched before and after indexing. Median latency fell from **140.20 ms to 1.97 ms**; the derived graph occupied about 6 MiB. Under the unchanged 1 GiB limit, the first indexed fetch took **575 ms**, a repeated fetch **95 ms**, and verification **346 ms**. Production now requests this optional index only during explicit synchronization and reports fetch duration/index presence. These results are an isolated diagnostic, not a replacement for the final cold-path matrix.

## Binary and workload identity

| Run | Binary SHA-256 | Canonical query-key SHA-256 |
| --- | --- | --- |
| [reference-diagnostic-baseline](reference-diagnostic-baseline/results.json) | `1ad9e075006dd932df210d5d6e2de44702ea101cfc2487ba2e6a51364ae652ea` | `942eaca06b56e9e99e257bab1f02386eab9a25b8a10996233f5c83254da36cc8` |
| [reference-diagnostic-optimized](reference-diagnostic-optimized/results.json) | `565031c6204baf1e6c1fc34297f5a7602350073868f456832478cd2c047faddf` | `942eaca06b56e9e99e257bab1f02386eab9a25b8a10996233f5c83254da36cc8` |
| [reference-diagnostic-io2](reference-diagnostic-io2/results.json) | `565031c6204baf1e6c1fc34297f5a7602350073868f456832478cd2c047faddf` | `942eaca06b56e9e99e257bab1f02386eab9a25b8a10996233f5c83254da36cc8` |
| [reference-diagnostic-io4](reference-diagnostic-io4/results.json) | `565031c6204baf1e6c1fc34297f5a7602350073868f456832478cd2c047faddf` | `942eaca06b56e9e99e257bab1f02386eab9a25b8a10996233f5c83254da36cc8` |
| [reference-final-diagnostic](reference-final-diagnostic/results.json) | `3ea7dd52ed2ee77de8593c30320d2abbb3d23fb4695dea94e206f5fe16747db3` | `942eaca06b56e9e99e257bab1f02386eab9a25b8a10996233f5c83254da36cc8` |

All canonical query-key hashes above match; request IDs are excluded from that comparison.

## Latest focused verification (before subsequent concurrency-audit fixes)

Binary SHA-256: `32ec6198cf86e0a507331c142330595c0a69bf0005c0099460b163015ddb9a8e`. Initial host load was 0.43/0.66/2.38; no compiler jobs appeared in the initial process sample. This version includes the bounded batch lookup, optional explicit-sync commit graph, and trusted Git exec helper. Later correctness fixes require their own final validation.

| Service workload | p95 ms | p99 ms | Requests/s | Errors |
| --- | ---: | ---: | ---: | ---: |
| 32-client warm, first 5 s | 2.327 | 2.881 | 22525 | 0 |
| 32-client warm, second 5 s | 2.508 | 3.140 | 20982 | 0 |
| 32-client batch, first 5 s | 11.764 | 16.816 | 4065 | 0 |
| 32-client batch, second 5 s | 12.806 | 18.628 | 3742 | 0 |
| 32-client warm, actual 60.27 s | 2.441 | 2.996 | 21420 | 0 |
| 32-client mixed, 20.10 s | 2.505 | 3.046 | 20658 | 0 |

The real 60-second read cohort provides duration-qualified P3 evidence for one repetition. The matched 32-client process-Git cohort gives aggregate numerical P2 speedups of **186× p95 and 54.3× throughput**. These aggregate ratios remain dominated by ancestry. Five repetitions and the complete reference matrix remain pending.

At one client, all four observed numerical P6 bounds passed on a common input list: inspect **0.430 vs Git 1.691 ms**, ancestry **20.276 vs 227.264 ms**, tree **3.060 vs 1.712 ms**, and diff **3.409 vs 1.797 ms**. However, the historical wall-time cutoff executed 1,000 service keys versus only 146 process-Git keys. These are different actual sample sets and **do not verify P6**; complete finite-workload comparison is pending. The 32-client cold diagnostics retain higher queued tree/diff p95 (**21.84/23.94 ms** versus Git **8.90/8.55 ms**); P6 specifies one client. Cold input files and reconstruction/key checksums are retained; the first and second 32-client cold slices match exactly.

All eight focused scenarios completed. Mixed load achieved **10.625 seconds of actual four-way overlap**, with successful exports, imports, real candidates, and stalled fetch; its read-throughput ratio was **0.928**. The 10,000-file managed workflow published successfully, and the agent-loop reported no errors. Sixty-second churn observed eviction, respected the cache budget, and recovered; this does not establish 30-minute memory stability. The 96-client overload cohort recorded **87,745 capacity errors**, followed by successful recovery. Those deliberate overload errors remain in the raw samples.

Historical result files used `p3_pass`/`p5_pass` for numerical short-cohort checks. They remain unchanged as raw evidence; a true value there does not certify duration or repetition requirements. New reports separate numerical diagnostic fields from duration-qualified fields and require five mixed repetitions for matrix P5.

## Final audited binary: complete cold verification and sustained batch miss

Binary SHA-256: `1d3edfa5f6137d06abe1be5c176855f91cea1a1bca19a2d9c46015c3537d056f`. Both cold adapters completed **all 1,000 identical keys** with **1,000 matching canonical decoded-answer digests** and no errors. All one-client P6 numerical bounds passed: inspect **0.454 vs Git 1.732 ms**, ancestry **24.674 vs 276.454 ms**, tree **3.446 vs 1.834 ms**, diff **3.735 vs 2.176 ms**. This replaces the earlier unequal-subset diagnostic as complete-key evidence, while five repetitions remain pending.

The final 60.27-second 32-client warm read passed: **21,760 requests/s**, p95 **2.404 ms**, p99 **2.955 ms**, zero errors. The 60.49-second batch cohort **missed the 25 ms p95 target**: p95 **37.919 ms**, p99 **92.407 ms**, **2,157 batch requests/s**, zero errors. The p99 stayed below its 100 ms target. Short-run batch improvements do not establish sustained P1 acceptance.

[Window analysis](final-audited-warm/batch-window-analysis.json) shows approximate client-elapsed p95 growing from **13.97 ms** in the first five seconds to **194.20 ms** in the last five; these windows reconstruct elapsed time from recorded request latencies and exclude request-building overhead. Exact sampled service-tree RSS grew from **21,836 to 86,896 KiB**, peaking at **96,688 KiB**. Thread counts were not sampled. The cause is unresolved; allocator behavior, thread churn, lock contention, and host scheduling remain hypotheses. [Host observations](final-audited-warm/host-observations.json) preserve available load/process evidence. No compiler jobs appeared in the captured concise count.

Final-binary mixed load also completed without read or heavy-operation errors: p95/p99 **3.481/5.614 ms**, read-throughput ratio **0.861**, and **6.876 seconds of actual four-way overlap**. The 20-second numerical diagnostic passed; duration/repetition-qualified P5 remains false. Initial loads for cold, warm, and mixed were respectively **2.37/1.48/1.86**, **1.19/1.36/1.77**, and **24.37/9.23/4.51**. These records do not claim an uncontended machine throughout.

## Subsequent serialized-length optimization and aged-service follow-up

Binary SHA-256: `4bb898ccb2d24cda3688490907078139c771d83d41cf03d19effd4b25b41c507`. Cache entries now retain their exact encoded length; batch bounds sum exact wrapper/separator lengths instead of serializing the entire result solely to measure its size. This removes confirmed duplicate work. An independent [unchanged-binary profile](warm-batch-profile/REPORT.md) did not reproduce the earlier miss, so that miss cannot be attributed conclusively to this duplicate work.

The exact fresh-service **read60 → batch60** sequence passed with zero errors. Read p95/p99 were **2.423/3.131 ms**, at **21,748 requests/s**. First batch p95/p99 were **13.912/20.228 ms**, at **3,716 batch requests/s**. An immediate **second 60-second batch on the same aged service**, without restart or rewarming, also passed: **20.549/46.240 ms**, **3,005 batch requests/s**, zero errors. Both batches meet the 25/100 ms targets; the aged follow-up is slower and remains recorded separately.

First-batch service-tree RSS was **24,212 → 79,344 KiB**, peak **83,148 KiB**, last-decile drift **+240 KiB**. The follow-up was **79,344 → 80,664 KiB**, peak **87,452 KiB**, last-decile drift **−1,335 KiB**. These short cohorts do not establish long-run memory stability. Initial recorded load was **10.96/13.52/9.61**; no uncontended-host claim is made. See [window analysis](final-batch-length-optimized/batch-window-analysis.json) and the full raw samples. The original sustained miss remains preserved above; full five repetitions and long churn remain pending.

## Open release gates

- Full five 60-second repetitions and five 30-minute churn cohorts have not run.
- A sustained batch P1 miss remains in historical evidence; the subsequent optimized read60/batch60 sequence and aged batch60 both pass. Full five-repeat P1 qualification remains pending. Mixed isolation passes short numerical diagnostics, with full-duration/repetition qualification pending.
- Final audited P6 now has complete identical-key execution and answer agreement; full repeated qualification remains pending. Historical unequal-subset results, misses, and 32-client queueing remain recorded.
- Latest focused reference churn, overload recovery, and 10,000-file workflow completed; the earlier interrupted comprehensive run remains preserved. Long churn qualification remains pending.
- Fixed-arrival counterparts and a controlled, uncontended reference qualification remain pending.
- Warm zero-Git/exchange checks and controlled 32-client cold-read/shared-sync bursts passed in these diagnostics, but they alone do not establish P1–P7.

## Raw samples

- [reference-diagnostic-baseline raw archive](reference-diagnostic-baseline/raw-samples.tar.gz), [per-file checksums](reference-diagnostic-baseline/raw-checksums.json); archive SHA-256 `2a2b5b8f5b307461150af48e66988fc33806383ac0bc5fa737e9962717fb2c38`.
- [reference-diagnostic-optimized raw archive](reference-diagnostic-optimized/raw-samples.tar.gz), [per-file checksums](reference-diagnostic-optimized/raw-checksums.json); archive SHA-256 `fe84e59a39c70f0c631b5a9de679900ca2626a5c0d816aa7973b9ddaf2731678`.
- [reference-diagnostic-io2 raw archive](reference-diagnostic-io2/raw-samples.tar.gz), [per-file checksums](reference-diagnostic-io2/raw-checksums.json); archive SHA-256 `8bc99b48c031f65b7f14cecdf393ccc0dd1676feedc3390bbc8a1372d1389048`.
- [reference-diagnostic-io4 raw archive](reference-diagnostic-io4/raw-samples.tar.gz), [per-file checksums](reference-diagnostic-io4/raw-checksums.json); archive SHA-256 `6e6ad86baac9e5fff55dc80623e043918df8de07be2c1e64734c124174662559`.
- [reference-final-diagnostic raw archive](reference-final-diagnostic/raw-samples.tar.gz), [per-file checksums](reference-final-diagnostic/raw-checksums.json); archive SHA-256 `9ef4032a3454f45f15f72d98cc4dcd04c4071daa8fa6b75eb247381f9780fedd`.

- [reference-focused-final results](reference-focused-final/results.json), [raw archive](reference-focused-final/raw-samples.tar.gz), [checksums](reference-focused-final/raw-checksums.json); archive SHA-256 `f06e176c379820bec468eb96cec3d11e75de38284c68ba36ac1a765ea95a5829`.

- [reference-focused-repeat results](reference-focused-repeat/results.json), [raw archive](reference-focused-repeat/raw-samples.tar.gz), [checksums](reference-focused-repeat/raw-checksums.json); archive SHA-256 `0e1d81e6789dc920c03d26bfec3047881c74cdad4efe25d5f53f6be21c570ada`.

- [reference-focused-cold-one results](reference-focused-cold-one/results.json), [raw archive](reference-focused-cold-one/raw-samples.tar.gz), [checksums](reference-focused-cold-one/raw-checksums.json); archive SHA-256 `1e40fd16ff7ae46ecd836eca858a97a629c7d1f650ca317a63453fc08955f993`.

- [reference-p3-sixty-seconds results](reference-p3-sixty-seconds/results.json), [raw archive](reference-p3-sixty-seconds/raw-samples.tar.gz), [checksums](reference-p3-sixty-seconds/raw-checksums.json); archive SHA-256 `5674e1b4d67401fe4734bb33ecaee33587a7fbc806018b85912673140252982f`.

- [final-audited-cold-one results](final-audited-cold-one/results.json), [raw archive](final-audited-cold-one/raw-samples.tar.gz), [checksums](final-audited-cold-one/raw-checksums.json); archive SHA-256 `7561ff869c8992cc62f81f3f749875b2abba3216d40c2afbb67f7366974d2ce5`.

- [final-audited-warm results](final-audited-warm/results.json), [raw archive](final-audited-warm/raw-samples.tar.gz), [checksums](final-audited-warm/raw-checksums.json); archive SHA-256 `512ece20c919ab47032b1eb7c81a26d0c7ca61eaa70058fd0f501b6276259fb8`.

- [final-audited-mixed results](final-audited-mixed/results.json), [raw archive](final-audited-mixed/raw-samples.tar.gz), [checksums](final-audited-mixed/raw-checksums.json); archive SHA-256 `abaae8c2518ddddb4693be5e3b1dea20110617752419ba8553023b6ee0d6a81f`.

- [final-batch-length-optimized results](final-batch-length-optimized/results.json), [raw archive](final-batch-length-optimized/raw-samples.tar.gz), [checksums](final-batch-length-optimized/raw-checksums.json); archive SHA-256 `fe0ce0d58470e4989e7fede5ea734e960a6ae186c801d950c16facc9ebf8ffa4`.
