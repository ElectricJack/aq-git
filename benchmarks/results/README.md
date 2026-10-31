# Stored benchmark evidence

Each run stores exact query inputs, a machine/tool declaration, threshold results, the service log, and a SHA-256 verified raw-sample archive. `reference_acceptance_claim` remains false for the diagnostic runs below. They were performed during implementation, with other builds/tests potentially active.

| Run | Scope and limitations |
| --- | --- |
| `smoke-initial` | One-second small fixtures with Python threads; interpreter lock contention confounds scaling. Retained for diagnosis only. |
| `smoke-process-clients` | Two-second small fixtures with independent client processes. Captures early latency misses and a nominal-capacity overload later fixed. Before binding-cache and admission changes. |
| `all-scenarios-smoke` | Incomplete harness trial; mixed setup failed on an unfetched writer commit. Failure retained. |
| `all-scenarios-smoke-v2` | One-client, half-second functional scenario trial. Mixed throughput included cleanup time; churn flag did not require an eviction. See limitations in JSON. |
| `all-scenarios-smoke-v3` | Eight functional scenarios, including local publication and 96-client overload recovery. Controlled full-admission sync barrier and plain-Git cold isolation were added afterward; the old sync and cold misses remain recorded. |

Extract `raw-samples.tar.gz` with `tar -xzf raw-samples.tar.gz`. `raw-checksums.json` records the uncompressed CSV checksums. Raw failures remain in the archive even when summary error examples are bounded.
