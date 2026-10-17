# SP2 benchmarks (automatic capture)

Measured on 2026-10-17 on `feat/sp2-capture` (based on 62c6369), with release builds. Each test was run twice on the
same build, and this page gives the second run, made at the lower load. The first run (load average 18 to 31) met every
bound too; its worst hook p95 was 57.0 ms, for the runs that start the daemon. Nothing was averaged. The distillation
evaluation has its own page, [sp2-distill.md](sp2-distill.md).

**Machine:** Apple M4 Pro, 14 cores (10 performance and 4 efficiency), 24 GB RAM, macOS 26.6.2 (25G83). Other builds
shared the machine during the runs: the 1-minute load average was 7.2 when they started and 6.2 when they ended.

## `alice hook` latency (Task 7)

**Budget:** p95 under 100 ms from process start to exit, with the daemon up and with it down, for every event
(`session-start` with its briefing included).

**Test:** `hook_finishes_under_100ms` in `crates/alice/tests/cli.rs` (ignored; release build): 200 runs each of
`alice hook stop` and `alice hook session-start` (which prints the briefing) with the daemon up, 200 runs of each with
it down (`ALICE_NO_AUTOSTART=1`, so the notice is spooled and no daemon is started), and 10 runs of `stop` with the
daemon down that start it in the background. It prints the numbers this table needs.

```
nice -n 15 cargo test --release -p alice-memory -j 4 --test cli hook_finishes_under_100ms -- --ignored --nocapture
```

| Case | Runs | p50 | p95 | max |
|---|---|---|---|---|
| `stop`, daemon up (notify) | 200 | 5.5 ms | 9.5 ms | 14.8 ms |
| `session-start` with the briefing, daemon up | 200 | 5.9 ms | 7.6 ms | 15.3 ms |
| `stop`, daemon down, spooled (`ALICE_NO_AUTOSTART=1`) | 200 | 13.9 ms | 15.1 ms | 17.2 ms |
| `session-start`, daemon down, spooled | 200 | 13.2 ms | 15.0 ms | 17.6 ms |
| `stop`, daemon down, spooled and a daemon started in the background | 10 | 17.5 ms | 21.5 ms | 21.5 ms |

The briefing was printed in 200 of 200 `session-start` runs with the daemon up. With 10 runs, the last row's p95 is
its maximum. The hook's hard deadline is 90 ms: a briefing that has not arrived by then is not printed. Measured on
macOS only; Linux is not measured yet.

## Capture throughput (Task 6a)

**Test:** `a_forty_megabyte_transcript_is_captured_in_bounded_passes` (`crates/alice-daemon/tests/capture_flow.rs`): a
40 MB transcript of 4,000 turns on a tenant, uploaded through `Service::capture_upload` (in process, no HTTP). The first
upload sends the whole file, of which one pass reads 8 MiB (`MAX_PASS_BYTES`); the rest follows from each returned
cursor in bodies of at most the route's 8 MiB limit, one pass each. Every turn arrives, in at least 5 passes and fewer
than 50.

```
nice -n 15 cargo test --release -p alice-daemon -j 4 --test capture_flow a_forty_megabyte_transcript_is_captured_in_bounded_passes -- --nocapture
```

| Measure | Value |
|---|---|
| Transcript size | 40,352,230 bytes (40 MB) |
| Turns | 4,000 (one tool call each, with a 9,500-character result) |
| Chunks stored | 4,001 |
| Passes (uploads) | 5 |
| Wall time, first upload to the last answer | 0.53 s |
| Throughput | 76 MB/s |

What the wall time covers: reading, rendering, turn keys, redaction and the writer's slices, each slice committed before
the answer. Each 9,500-character result is trimmed to 800 characters, so a turn renders to about one chunk. Embedding
runs later, in the background (the tests' fake embedder here), and is not included. The writer's slices stay at most 50
chunks / 32 KiB by construction (`INGEST_SLICE`, `INGEST_SLICE_BYTES`); this test does not measure them, nor the
daemon's memory.

**Concurrency:** `twenty_sessions_at_once_capture_everything_once_beside_remembers`: 20 sessions of 5 turns uploaded at
once, in two bodies each, while 5 writers `remember` 10 memories each.

```
nice -n 15 cargo test --release -p alice-daemon -j 4 --test capture_flow twenty_sessions_at_once_capture_everything_once_beside_remembers -- --nocapture
```

| Measure | Value |
|---|---|
| `remember` ack p95 during capture | 8.4 ms over 50 `remember`s (the test asserts under 500 ms; the M2 budget for `remember` alone is 50 ms) |
| Time until all 20 sessions are captured | 22 ms |
| Turns stored twice or lost | 0 (asserted) |

## Tenant script (Task 15)

Each test runs `hook.sh` under `sh` against an in-process tenant over loopback HTTP, so no network time is included.
The harness checks for the hook's exit every 5 ms, so times are within about 5 ms.

```
nice -n 15 cargo test --release -p alice-daemon -j 4 --test capture_scripts -- --nocapture <test name>
```

| Measure | Test | Value |
|---|---|---|
| `SessionStart` hook with the server stalled | `the_briefing_gives_up_within_two_seconds` | 2,100 ms (bound: under 2.5 s; `curl --max-time 2`) |
| `SessionStart` hook with the briefing printed | `session_start_prints_the_briefing_as_plain_text` | 83 ms |
| `Stop` hook (the upload runs in the background) | `a_transcript_grown_in_steps_is_captured_once` | 38, 50 and 68 ms (3 runs) |
| A 5 MiB backlog in 2 MiB bodies | `a_backlog_over_two_mebibytes_drains_in_several_calls` | 5,262,009 bytes in 3 uploads, all stored 3.4 s after the `Stop` hook (2 s of it the settle wait after a stop event) |

## Distillation evaluation (Task 13)

Full results, the protocol and the caveats: [sp2-distill.md](sp2-distill.md). The set: 30 synthetic sessions (10 in
Thai or mixed), 50 gold items, 6 sessions with nothing worth keeping, 4 where the agent already saved the key items.
Thresholds were declared before any run (SP2 spec §11.3). Run 2 followed the one tuning round the spec allows; the
numbers below are copied from that page, which its live test wrote.

| Metric | Threshold | Run 1 | Run 2 (after tuning) |
|---|---|---|---|
| Precision | ≥ 0.80 | 0.70 (40/57): fails | 0.90 (35/39) |
| Recall | ≥ 0.60 | 0.72 (36/50) | 0.70 (35/50) |
| Doubles (of the 10 already-saved items) | ≤ 10 % | 4: fails | 0 |
| Flood (memories from the "nothing" sessions) | ≤ 2 in all | 3: fails | 1 |
| Spend for the whole run | < 0.10 USD | 0.0068 USD (73 requests) | 0.0059 USD (53 requests) |

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

SP2 adds one budget, `alice hook` (above), and changes none of M2's. The 1M-chunk M2 benchmarks (`alice bench`, results
in [m2.md](m2.md)) were not run again for this page. What was checked here:

| Budget | Limit | Value |
|---|---|---|
| `remember` ack p95 | < 50 ms | 8.4 ms while 20 sessions were captured at once (above; in process, 50 calls) |
| Search quality (`crates/alice-core/tests/search_quality.rs`) | unchanged thresholds | passes, thresholds unchanged |
| Search quality with the real model (`crates/alice-embed/tests/real_search_quality.rs`) | unchanged thresholds | not run for this page (ignored test; needs the model) |
