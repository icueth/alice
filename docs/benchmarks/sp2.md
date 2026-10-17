# SP2 benchmarks (automatic capture)

**Status: skeleton.** Every `TODO(confirm)` below is filled in from the named test or task before SP2 is accepted. The
distillation evaluation gets its own page, `docs/benchmarks/sp2-distill.md`, written by its live test (TODO(confirm):
Task 13 creates it; link it here then).

Machine for every number unless a section says otherwise: TODO(confirm) (model, cores, memory, OS), with the load
average at the time of the run.

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
| `stop`, daemon up (notify) | 200 | TODO(confirm) | 6.6-7.5 ms (reported) | TODO(confirm) |
| `session-start` with the briefing, daemon up | 200 | TODO(confirm) | TODO(confirm) | TODO(confirm) |
| `stop`, daemon down, spooled (`ALICE_NO_AUTOSTART=1`) | 200 | TODO(confirm) | 16-17 ms (reported) | TODO(confirm) |
| `session-start`, daemon down, spooled | 200 | TODO(confirm) | TODO(confirm) | TODO(confirm) |
| `stop`, daemon down, spooled and a daemon started in the background | 10 | TODO(confirm) | 21.8 ms (reported) | TODO(confirm) |

The "(reported)" values come from Task 7's own run, whose report is not in this repository. TODO(confirm): run the test
above and replace them with its numbers, with the machine and the load. The hook's hard deadline is 90 ms: a briefing
that has not arrived by then is not printed.

## Capture throughput (Task 6a)

**Test:** `a_forty_megabyte_transcript_is_captured_in_bounded_passes` (`crates/alice-daemon/tests/capture_flow.rs`): a
40 MB transcript of 4,000 turns on a tenant. The first upload sends the whole file, of which one pass reads 8 MiB
(`MAX_PASS_BYTES`); the rest follows from each returned cursor in bodies of at most the route's 8 MiB limit, one pass
each. Every turn arrives, at least 5 passes and fewer than 50.

| Measure | Value |
|---|---|
| Transcript size | 40 MB |
| Turns | 4,000 (one tool call each, with a 9,500-character result) |
| Chunks stored | TODO(confirm) |
| Passes (uploads) | TODO(confirm) |
| Wall time, first byte to last chunk committed | TODO(confirm) |
| Throughput | TODO(confirm) MB/s |
| Largest writer slice (chunks / bytes) | TODO(confirm) (limit 50 chunks / 32 KiB) |
| Peak daemon RSS during the run | TODO(confirm) |

**Concurrency:** `twenty_sessions_at_once_capture_everything_once_beside_remembers`: 20 sessions of 5 turns uploaded at
once, in two bodies each, while 5 writers `remember` 10 memories each.

| Measure | Value |
|---|---|
| `remember` ack p95 during capture | TODO(confirm) (the test asserts under 500 ms; the M2 budget for `remember` alone is 50 ms) |
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

Full results: `sp2-distill.md` (TODO(confirm): link it once Task 13 has written it). The set: 30 synthetic sessions (about a third in Thai or mixed), 50 gold
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
| Sessions | 3: s1 and s2 as in the baseline run (s1 cut at its baseline length); s3 as a 24 MiB prefix of its main transcript (34 %) with the 74 of its 102 subagent transcripts spawned in that prefix |
| Turns captured, per session | s1 43 (32 main, 11 subagent), s2 2, s3 182 (108 main, 74 subagent) |
| Chunks, per session | s1 801, s2 35, s3 3,403 (4,239 in all; 83 % from subagents) |
| Chunks per day, extrapolated to the owner's usual week | About 2,100 per day of session time (4,239 chunks over the 1.97 days the sessions span; per session 731, 650 and 4,126). About 1,060 per calendar day (over the 4 days the sessions touch). With one such session at a time, that is about 7,400 to 15,000 a week. (estimate in the spec: ~1,000) |
| Candidates asked about, per session | s1 81, s2 26, s3 127 (234 in all). Once a session's cap of 16 was full, the rest of it was not sent to Jev: about half of the s1 and s3 main transcripts, and nearly all of their subagent text |
| Memories kept, per session, by kind | s1 15: warning 6, fact 4, bugfix 3, decision 1, preference 1 (user-wide). s2 6: fact 3, howto 1, decision 1, warning 1. s3 16: fact 6, decision 6, warning 3, handoff 1 (37 in all) |
| Duplicates the judge caught | 1 merged and 1 superseded (both in s3), 1 related link (s1); 0 contradicts; 0 reviews |
| Jev spend, total and per session | 0.0122 USD in all: s1 0.0047, s2 0.0012, s3 0.0063. About 60 % distillation, 40 % judge (estimate: 0.001-0.003 USD per session) |
| Capture pass latency, p50 / p95 | 39 ms / 128 ms over 107 uploads (bodies up to 8 MiB, loopback, release build; max 192 ms) |

Baseline before Tasks 12b/11b (Part B): 163 kept (s1 23, s2 5, s3 135), 2 duplicates, $0.0419. Of the 60 good candidates the old per-run cap cut, 6 are now kept. Reviewer-verdict or status narration kept: 0 by the title pattern (3 status reports on a manual read). Titles of 25 characters or fewer: 1.

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
