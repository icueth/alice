# SP2 benchmarks (automatic capture)

**Status: skeleton.** Every `TODO(confirm)` below is filled in from the named test or task before SP2 is accepted. The
distillation evaluation has its own page, [sp2-distill.md](sp2-distill.md), written by its live test.

Machine for every number unless a section says otherwise: TODO(confirm) (model, cores, memory, OS), with the load
average at the time of the run.

## `alice hook` latency (Task 7)

**Budget:** p95 under 100 ms from process start to exit, with the daemon up and with it down, for every event
(`session-start` with its briefing included).

**Test:** `hook_finishes_under_100ms` in `crates/alice/tests/cli.rs` (ignored; release build): 200 runs of
`alice hook stop` with the daemon up, and 200 with it down (`ALICE_NO_AUTOSTART=1`, so the notice is spooled and no
daemon is started).

```
nice -n 15 cargo test --release -p alice-memory -j 4 --test cli hook_finishes_under_100ms -- --ignored --nocapture
```

| Case | Runs | p50 | p95 | max |
|---|---|---|---|---|
| Daemon up (notify) | 200 | TODO(confirm) | 6.6-7.5 ms (reported) | TODO(confirm) |
| Daemon down, spooled (`ALICE_NO_AUTOSTART=1`) | 200 | TODO(confirm) | 16-17 ms (reported) | TODO(confirm) |
| Daemon down, spooled and a daemon started in the background | TODO(confirm) | TODO(confirm) | 21.8 ms (reported) | TODO(confirm) |
| `session-start` with the briefing, daemon up | TODO(confirm) | TODO(confirm) | TODO(confirm) | TODO(confirm) |

The p95 values come from Task 7's report as recorded in the SP2 ledger; the full table, the machine and the load are in
`help-7-8-report.md` (TODO(confirm): copy them here). The hook's hard deadline is 90 ms: a briefing that has not arrived
by then is not printed.

## Capture throughput (Task 6a)

**Test:** `a_forty_megabyte_transcript_is_captured_in_bounded_passes` (`crates/alice-daemon/tests/capture_flow.rs`): a
40 MB transcript captured through uploads of at most 8 MiB each, every turn arriving once.

| Measure | Value |
|---|---|
| Transcript size | 40 MB |
| Turns | TODO(confirm) |
| Chunks stored | TODO(confirm) |
| Passes (uploads) | TODO(confirm) |
| Wall time, first byte to last chunk committed | TODO(confirm) |
| Throughput | TODO(confirm) MB/s |
| Largest writer slice (chunks / bytes) | TODO(confirm) (limit 50 chunks / 32 KiB) |
| Peak daemon RSS during the run | TODO(confirm) |

**Concurrency:** `twenty_sessions_at_once_capture_everything_once_beside_remembers`: 20 sessions notified at once while
agents `remember`.

| Measure | Value |
|---|---|
| `remember` ack p95 during capture | TODO(confirm) (budget 50 ms) |
| Time until all 20 sessions are captured | TODO(confirm) |
| Turns stored twice or lost | 0 (asserted) |

## Tenant script (Task 15)

| Measure | Value |
|---|---|
| SessionStart hook wall time with the server stalled (`the_briefing_gives_up_within_two_seconds`) | TODO(confirm) (bound: under 2.5 s; `curl --max-time 2`) |
| SessionStart hook wall time with the server answering | TODO(confirm) |
| Stop hook wall time (the upload runs in the background) | TODO(confirm) |
| Upload of a 5 MiB backlog (2 MiB bodies) | TODO(confirm) |

## Distillation evaluation (Task 13)

Full results: [sp2-distill.md](sp2-distill.md). The set: 30 synthetic sessions (about a third in Thai or mixed), 50 gold
items, 6 sessions with nothing worth keeping, 4 where the agent already saved the key items. Thresholds were declared
before any run (SP2 spec §11.3).

| Metric | Threshold | First run | Re-run (if any) |
|---|---|---|---|
| Precision | ≥ 0.80 | TODO(confirm) | TODO(confirm) |
| Recall | ≥ 0.60 | TODO(confirm) | TODO(confirm) |
| Doubles (of the already-saved items) | ≤ 10 % | TODO(confirm) | TODO(confirm) |
| Flood (memories from the "nothing" sessions) | ≤ 2 in all | TODO(confirm) | TODO(confirm) |
| Spend for the whole run | < 0.10 USD | TODO(confirm) | TODO(confirm) |

## The owner's real sessions (Task 20)

Aggregates only: no text from the sessions is ever committed. The sessions were copied read-only, captured and
distilled in a throwaway home, and the copies deleted afterwards.

| Measure | Value |
|---|---|
| Sessions | TODO(confirm) (3-5) |
| Turns captured, per session | TODO(confirm) |
| Chunks, per session | TODO(confirm) |
| Chunks per day, extrapolated to the owner's usual week | TODO(confirm) (estimate in the spec: ~1,000) |
| Candidates asked about, per session | TODO(confirm) |
| Memories kept, per session, by kind | TODO(confirm) |
| Duplicates the judge caught | TODO(confirm) |
| Jev spend, total and per session | TODO(confirm) (estimate: 0.001-0.003 USD per session) |
| Capture pass latency, p50 / p95 | TODO(confirm) |

## Budgets that still hold

SP2 changes none of the M2 budgets. Re-checked after the SP2 merge:

| Budget | Limit | Value |
|---|---|---|
| `remember` ack p95 | < 50 ms | TODO(confirm) |
| `recall` fast p95 at 1M chunks | < 200 ms | TODO(confirm) |
| `context` p95 | < 150 ms | TODO(confirm) |
| `alice mcp` start | < 20 ms | TODO(confirm) |
| Daemon RSS at 1M chunks | < 800 MB | TODO(confirm) |
| Search quality (`search_quality.rs`, `real_search_quality.rs`) | unchanged thresholds | TODO(confirm) |
