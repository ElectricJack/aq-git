# Benchmarking aq-git

Run `scripts/bench --profile smoke` from the checkout. The harness uses Python 3.12's standard library, installed Git, and the release `aq-git` binary. It builds that binary if absent; rebuild an existing binary after code changes. Fixtures, state roots, and local publication authorities live under `/tmp/aq-git-benchmark` by default. No network service or sibling checkout is needed.

The seeded workload is declared in [reference.json](../benchmarks/fixtures/reference.json). Git fast-import generates independent branches, periodic merges, and many refs. The reference fixture has a 100,000-commit/10,000-ref repository, a second 10,000-commit/1,000-ref repository, and a separate Git repository containing 10,000 files. Smoke uses two 2,000-commit/200-ref repositories and 1,000 workspace files. Both use 1,000 distinct query keys with the declared 40% inspect, 40% ancestry, 10% tree, and 10% diff mix. The harness saves query inputs and fixture parameters. It refuses to replace an unowned fixture directory.

One Python OS process drives each client, avoiding a shared interpreter lock between clients. All backends use the same driver:

- `service` keeps one Unix-socket connection per client. Latency includes sending the request through receiving and decoding the complete JSON response.
- `git_process` invokes one installed Git process per logical query, using `cat-file commit`, `merge-base --is-ancestor`, and the same NUL-safe tree/diff options as the service. It never fetches per read.
- `git_persistent` reuses `cat-file --batch-command` processes for inspection; ancestry, tree, and diff use Git subprocesses. This comparison separates some process amortization from caching.

Before measurement, every fixed query is compared structurally against installed Git, including ancestry, exact parents/tree IDs, raw commit fields, encoded paths, and bounded summaries. Repeating the complete warm set must change neither Git spawn counters nor object exchanges. Separate 32-client bursts test one cold inspection computation and one shared scoped synchronization. Each sync caller has its own operation ID, so journal replay cannot masquerade as coalescing.

The default scenarios are:

| Scenario | Measurement |
| --- | --- |
| `read` | Warm mixed single reads at 1, 8, and 32 clients, with aggregate and operation-class percentiles. |
| `batch` | 100-item warm batches. Request throughput and logical-query throughput are reported separately. |
| `cold` | Disjoint, never previously queried keys, one execution per key. Every selected backend must drain the same finite input set under a 300-second overall budget; incomplete execution explicitly fails P6. Per-class p95 is compared with `2 × Git + 2 ms`. These are cold service caches with warm filesystem caches, not a claim of cold disk performance. |
| `agent-loop` | Explicit sync, frozen OIDs, a controlled remote advance after the observation, then concurrent 100-item decisions. Exact answers must still match the frozen inputs. Refresh time is reported separately. All backends use the same explicit service synchronization boundary. |
| `mixed` | Warm reads during full export/import, 64 independent source commits with 63 clean merge steps, and a barrier-stalled local fetch. Actual heavy-operation progress and timing are recorded. |
| `churn` | 20,000 reference keys or 6,000 smoke keys, exceeding the entry budget; records eviction, memory drift, bounds, rewarming, and recovery. |
| `overload` | 96 socket clients exceed advertised capacity; errors are recorded separately, followed by a bounded warm recovery cohort. |
| `workflow` | A 1,000/10,000-file managed checkout, checkpoint, candidate, expected-old publication to a temporary local bare repository, retirement, and explicit retention release. |

Controlled P4 and mixed-load checks restart the service with an administrator Git wrapper that barriers only the designated local fetch. Read, batch, and cold cohorts use plain installed Git, so the wrapper cannot inflate the process baseline or add service miss overhead. The service is restored and its complete working set rewarmed after each wrapper phase. Mixed reads have a control cohort in the same restarted engine immediately before heavy work. The candidate fixture forks 64 commits from one base, each adding a distinct file, so candidate load performs real merges. The export/import pipeline has at most two queued bundles. Import runs in a separate worker service; resource sampling includes both installations.

Useful commands:

```sh
scripts/bench --profile smoke --scenarios read,batch
scripts/bench --profile reference
scripts/bench --profile reference --seconds 5 --repetitions 1 --scenarios read
scripts/bench --profile reference --scenarios read --arrival-rate 1000
scripts/bench --profile reference --scenarios churn --clients 32
scripts/bench --profile reference --scenarios read,batch --backends service --clients 32 --seconds 5 --repetitions 1
scripts/bench --profile reference --seconds 5 --repetitions 1 --churn-seconds 60 --mixed-seconds 20
```

Fixed-arrival runs measure latency from intended arrival, including client queue delay. Otherwise clients run closed loop. The reference default uses five 60-second repetitions per applicable client/backend cohort, and five 1,800-second service churn cohorts at the largest selected client count. A full run therefore takes several hours. Shorter durations, fewer repetitions, selected backends, scenario-specific `--churn-seconds`/`--mixed-seconds`, and different client sets are explicit diagnostic overrides. Mixed results clip successful RPC intervals to the measured read window and require positive four-way overlap; setup, transfer-copy waits, and cleanup are excluded from that overlap.

Results contain the declared machine's CPU model/affinity, filesystem, tool versions, load average, available CPU controls, exact inputs, all failures, per-class percentiles, comparative speedups, and threshold results. A sampler records guard, engine, and Git-child RSS and CPU every 100 ms; per-process high-water marks and sampled simultaneous peak RSS are distinguished. Failed work never contributes to successful throughput. Summaries retain bounded error examples and the exact error count; raw samples retain every error.

The implementation's actual cache limit is declared in each result. Its default is 16 MiB and 4,096 entries, while the seeded manifest's initial requested budget is 128 MiB. This override is explicit. Cache accounting conservatively includes value heaps, duplicated keys, and map overhead; process RSS is measured independently.

Explicit `repo.sync` requests Git's optional commit-graph refresh during fetch. Observation records report fetch duration and whether a local graph was present at completion; exact reads never build the index, and removing it cannot change answers. The raw process-Git fixture remains unmodified. The separate [commit-graph experiment](../benchmarks/results/commit-graph-diagnostic/results.json) records 16 matching ancestry pairs, first/repeat refresh costs, the 6 MiB derived graph, and successful verification under the unchanged 1 GiB limit. Index gains must be distinguished from persistent-process and immutable-result cache gains.

Packing is explicitly bounded to two threads, 16 MiB window memory, and 16 MiB delta cache. The [pack diagnostic](../benchmarks/results/pack-resource-diagnostic/results.json) preserves the exact managed-pool default failure and successful bounded configuration under the same 1 GiB address-space limit. The [import diagnostic](../benchmarks/results/import-resource-diagnostic/results.json) preserves the local upload-pack failure and successful original-bundle unbundle comparison. `advice.graftFileDeprecated=false` suppresses stderr noise only; grafts remain disabled through `GIT_GRAFT_FILE=/dev/null`.

The control client refreshes idle connections and retries an ambiguous connection loss at most once with the identical request, including its original mutation operation ID and payload. Domain errors such as overload are reported without retry. `python3 -m unittest discover -s benchmarks -p test_driver.py` verifies that behavior.

Raw CSVs are checksum-verified and stored in `raw-samples.tar.gz`, alongside `raw-checksums.json`. Extract them with `tar -xzf raw-samples.tar.gz`; the JSON manifest lists each file's SHA-256 and size. `results.json` also records the archive checksum. Decision-loop and barrier-burst inputs/results remain readable JSON.

`acceptance_matrix` evaluates required evidence; `reference_acceptance_claim` is true only when every required condition is met. A smoke run or a shortened reference-sized cohort cannot satisfy the 60-second/five-repetition and 30-minute gates. Unselected scenarios are recorded as `not_run`. Existing `smoke-initial` results used a thread-based driver and are retained as diagnostics with explicit limitations; they must not establish scaling. Implementation-time measurements may overlap compilation and are also diagnostic. Release claims require the complete matrix on the declared reference machine, including every missed target.

The [latest evidence summary](../benchmarks/results/evidence-summary.md) records two passing short warm/batch cohorts, partial one-client P6 diagnostics (historically unequal actual sample sets), successful overlapping mixed work and the 10,000-file workflow, and one actual 60-second P3 cohort. Full repetition and long-churn requirements remain pending. New per-gate fields distinguish numerical diagnostic checks from duration-qualified P3/P5; historical raw reports retain their original labels.

Historical short cold cohorts stopped at the wall-time limit and may have executed different subsets despite receiving the same input list. They do not establish P6. Current cold cohorts drain all keys independently of the timed warm-cohort duration and report expected/completed key counts; exhausting the finite-workload budget is an explicit failure.

Finite cold measurements retain a canonical decoded-answer SHA-256 for every input index, calculated after stopping each request latency clock. Service and process-Git must both complete the entire same input list and match every digest before P6 can pass; a mismatch also fails the harness exit status. Cold keys are generated and saved before measurement, without prewarming service answers.

The final audited binary completed all 1,000 cold keys per adapter with matching decoded-answer digests, and passed one actual 60-second 32-client warm-read cohort. Its sustained 60-second batch p95 missed 25 ms (37.919 ms observed); this remains an open P1 gate. The final 20-second mixed diagnostic passed numerically without errors, with full four-way overlap recorded. See the evidence summary for exact hashes, raw archives, temporal batch degradation and unresolved cause; full five repetitions and long churn remain pending.

The subsequent encoded-length optimization passed the exact read60→batch60 sequence and an immediate second batch60 on the same aged service. The earlier sustained batch miss remains preserved; an unchanged-binary fresh-service profile also passed, so its cause remains unresolved. `--aged-batch-followup` requests this diagnostic follow-up only after a passing primary batch, recording it separately without counting it as the declared repetition matrix. Full qualification remains pending.
