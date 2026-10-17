# Distillation evaluation (SP2, Task 13)

Measured on 2026-10-17. Runs 1 and 2 ran on `feat/sp2-eval` (based on e254f6e); run 3 ran on `feat/sp2-flood`
(a19faa6).

- **Runs:** three live runs. Run 1 came before, and run 2 after, the single tuning round that spec §11.3 allows. Run 3
  re-recorded the evaluation after two product fixes (Tasks 12b and 11b) that followed the owner's real-session check; it
  is not a second tuning round. Each run is a single execution; nothing was repeated or averaged.
- **The set:** synthetic. It was written for this evaluation by the engineer, not taken from users, holds no real user
  data, and its labels are **one author's** judgement.
- **Thresholds:** declared in spec §11.3 before the first run, and unchanged since.

| | |
|---|---|
| Machine | Apple M4 Pro (14 cores), 24 GB RAM, macOS 26.6.2 |
| Distiller and judge | TypeSafe Jev `jev-1.13.0` (`https://api.typesafe.ai/v1/systemone`), the real pipeline through `Service`: capture, the distill worker, the judge worker |
| Test home | `judge.provider = "jev"`, `judge.daily_budget_usd = 1.0`, `judge.timeout_ms = 60000`, `distill.provider = "jev"`, `distill.budget_share = 1.0`, `distill.min_confidence = 0.7` (the default) |
| Embedder | the tests' `FakeEmbedder` (hashed words and trigrams); it only decides which older memories the judge compares |
| Test | `crates/alice-daemon/tests/distill_eval.rs`; the set in `tests/fixtures/distill/` (checked by `distill_fixtures.rs`); the recorded answers of run 3 in `tests/fixtures/distill/recorded.jsonl`; the flood set (Task 12b) and its recorded answers in `tests/fixtures/distill/flood/` |

```
# free: a scripted Jev; the pipeline runs twice and must send the same request bodies
ALICE_DISTILL_EVAL_DRY=1 cargo test -p alice-daemon --test distill_eval distill_eval_live -- --ignored --nocapture
# the live run (TYPESAFE_API_KEY in the environment; rewrites recorded.jsonl, bodies only)
cargo test -p alice-daemon --test distill_eval distill_eval_live -- --ignored --nocapture
# CI: every request answered from recorded.jsonl by the SHA-256 of its exact body
cargo test -p alice-daemon --test distill_eval
# the flood set: the same dry check and live run (the two live runs of one re-record share $0.10;
# ALICE_DISTILL_EVAL_STOP_USD gives the second what the first left)
ALICE_DISTILL_EVAL_DRY=1 cargo test -p alice-daemon --test distill_eval distill_eval_flood_live -- --ignored --nocapture
ALICE_DISTILL_EVAL_STOP_USD=0.093 cargo test -p alice-daemon --test distill_eval distill_eval_flood_live -- --ignored --nocapture
```

## The set

There are 30 Claude Code sessions in the transcript format, about 1 MB in all.

| Set | Sessions | Labelled items | Thai or mixed |
|---|---|---|---|
| gold: 1 to 4 items worth keeping | 20 | 50 | 6 sessions, 16 items |
| nothing: chit-chat, progress narration, plans for the next minutes | 6 | 0 | 2 sessions |
| already: the agent saved every item itself (`mcp__alice__remember`) | 4 | 10 | 2 sessions, 4 items |

- **Item kinds:** each item has a key phrase and the kinds a correct memory may have. Counted by first kind, the 50 gold
  items are bugfix 14, decision 9, howto 7, preference 6, warning 6, fact 5 and handoff 3. 22 of them accept more than
  one kind.
- **Placement:** every phrase sits once, in a passage the distiller proposes (`distill_fixtures.rs` checks this).

## Protocol

Every session is run on its own: one daemon per session, one session at a time. So what the judge compares never depends
on scheduling.

1. **The agent's own memories:** in an already session, the transcript's `remember` calls (title, content and kind) are
   stored first through `Service::remember`. They go in the session's project, with origin `agent`, and are judged.
2. **Capture:** the transcript is uploaded with `Service::capture_transcript` as a session end, so a handoff can be
   offered.
3. **Settling:** the test waits until the session is distilled and every memory is judged. A distilled duplicate of an
   older memory is trashed.
4. **Reading:** the test reads the memories with origin `distill`. A memory counts while it is active. One the judge
   trashed (a duplicate of an older memory) or superseded (a newer distilled memory updates it) does not count; recall
   hides both, and the report counts each. Any other status (pending, archived and the rest) fails the test, naming the
   memory.

**Matching.** A memory matches a labelled item of its own session when two things hold:

- its body, without the `Context:` line distillation adds, holds the item's phrase;
- its kind is one of the item's kinds.

Each memory is matched on its own. For example, s29's summary is cut into two chunks, and memories are never joined
across that cut.

**Metrics:**

- *precision* = matching memories / all live distilled memories of the 30 sessions;
- *recall* = gold items that a live memory matches / 50;
- *doubles* = already items that a live distilled memory matches, beside the agent's own copy;
- *flood* = live distilled memories of the nothing sessions;
- *spend* = the sum of the cost ledgers, the judge's and distillation's.

Per kind, precision goes by the memory's kind and recall by the item's first kind. Per language, both go by the session.

## Results

| Metric | Threshold | Run 1 | Run 2 (after tuning) | Run 3 (after Tasks 12b and 11b) |
|---|---|---|---|---|
| Precision, all 30 sessions | ≥ 0.80 | **0.70** (40/57; 95% Wilson 0.57–0.80): fails | **0.90** (35/39; 0.76–0.96) | **0.90** (36/40; 0.77–0.96) |
| Precision, the 20 gold sessions | — | 0.73 (36/49) | 0.92 (35/38) | 0.92 (36/39) |
| Recall | ≥ 0.60 | 0.72 (36/50; 0.58–0.83) | 0.70 (35/50; 0.56–0.81) | 0.72 (36/50; 0.58–0.83) |
| Doubles | ≤ 10 % (1 of 10) | **4**: fails | 0 | 0 |
| Flood | ≤ 2 | **3**: fails | 1 | 1 |
| Spend | < $0.10 | $0.0068 (73 requests) | $0.0059 (53 requests) | $0.0065 (55 requests) |
| Distilled memories / trashed by the judge | — | 63 / 6 | 40 / 1 | 42 / 2 |
| Jev latency, median / max | — | 312 / 682 ms | 323 / 694 ms | 349 / 1,247 ms |

Runs 2 and 3 meet every threshold, and the CI replay pins run 3. Recall passes as a point estimate, but its interval does
not rule out a recall under 0.60.

**Per kind** (precision by the memory's kind; recall by the item's first kind):

| Kind | Run 1 precision | Run 1 recall | Run 2 precision | Run 2 recall | Run 3 precision | Run 3 recall |
|---|---|---|---|---|---|---|
| bugfix | 0.73 (11/15) | 0.71 (10/14) | 0.82 (9/11) | 0.64 (9/14) | 0.90 (9/10) | 0.64 (9/14) |
| decision | 0.82 (9/11) | 1.00 (9/9) | 0.89 (8/9) | 0.89 (8/9) | 0.89 (8/9) | 0.89 (8/9) |
| fact | 0.57 (4/7) | 0.60 (3/5) | 0.75 (3/4) | 0.60 (3/5) | 0.75 (3/4) | 0.60 (3/5) |
| handoff | 0.50 (2/4) | 0.67 (2/3) | 1.00 (3/3) | 1.00 (3/3) | 1.00 (3/3) | 1.00 (3/3) |
| howto | 0.60 (6/10) | 0.86 (6/7) | 1.00 (6/6) | 0.86 (6/7) | 1.00 (6/6) | 0.86 (6/7) |
| preference | 1.00 (5/5) | 0.50 (3/6) | 1.00 (3/3) | 0.50 (3/6) | 0.80 (4/5) | 0.67 (4/6) |
| warning | 0.60 (3/5) | 0.50 (3/6) | 1.00 (3/3) | 0.50 (3/6) | 1.00 (3/3) | 0.50 (3/6) |

**Per language:**

| Language | Sessions | Run 1 precision | Run 1 recall | Run 1 doubles, flood | Run 2 precision | Run 2 recall | Run 2 doubles, flood | Run 3 precision | Run 3 recall | Run 3 doubles, flood |
|---|---|---|---|---|---|---|---|---|---|---|
| English | 20 | 0.73 (27/37) | 0.74 (25/34) | 2, 1 | 0.92 (24/26) | 0.71 (24/34) | 0, 0 | 0.93 (25/27) | 0.74 (25/34) | 0, 0 |
| Thai or mixed | 10 | 0.65 (13/20) | 0.69 (11/16) | 2, 2 | 0.85 (11/13) | 0.69 (11/16) | 0, 1 | 0.85 (11/13) | 0.69 (11/16) | 0, 1 |

**Per session.** Each run's cell gives questions asked, live memories (trashed in brackets), matching memories, and
items found. Items found are gold items matched, or, in an already session, doubles. No memory of this set was
superseded in run 2 or run 3; the replays check it, and run 1 came before that check.

| Session | Set | Language | Items | Run 1 | Run 2 | Run 3 |
|---|---|---|---|---|---|---|
| s01 | gold | English | 3 | 11, 3 (0), 3, 3 | 11, 3 (0), 3, 3 | 11, 3 (0), 3, 3 |
| s02 | nothing | English | 0 | 6, 0 (0), 0, 0 | 6, 0 (0), 0, 0 | 6, 0 (0), 0, 0 |
| s03 | gold | Thai or mixed | 3 | 6, 4 (0), 2, 2 | 6, 2 (0), 2, 2 | 6, 1 (0), 1, 1 |
| s04 | gold | English | 3 | 13, 2 (0), 2, 2 | 13, 2 (0), 2, 2 | 12, 2 (0), 2, 2 |
| s05 | already | English | 3 | 9, 0 (2), 0, 0 | 9, 0 (0), 0, 0 | 9, 0 (0), 0, 0 |
| s06 | gold | English | 2 | 8, 1 (0), 1, 1 | 8, 1 (0), 1, 1 | 8, 1 (0), 1, 1 |
| s07 | gold | Thai or mixed | 3 | 7, 4 (0), 3, 3 | 7, 3 (0), 3, 3 | 7, 3 (0), 3, 3 |
| s08 | nothing | Thai or mixed | 0 | 5, 2 (0), 0, 0 | 5, 1 (0), 0, 0 | 5, 1 (0), 0, 0 |
| s09 | gold | English | 3 | 12, 4 (0), 3, 3 | 12, 2 (0), 2, 2 | 11, 2 (0), 2, 2 |
| s10 | gold | English | 2 | 5, 2 (0), 2, 2 | 5, 2 (0), 2, 2 | 5, 2 (0), 2, 2 |
| s11 | already | Thai or mixed | 2 | 5, 2 (1), 2, 2 | 5, 0 (0), 0, 0 | 5, 0 (1), 0, 0 |
| s12 | gold | English | 2 | 9, 2 (0), 1, 1 | 9, 1 (0), 1, 1 | 9, 1 (0), 1, 1 |
| s13 | gold | Thai or mixed | 3 | 6, 3 (0), 2, 2 | 6, 2 (0), 2, 2 | 6, 3 (0), 3, 3 |
| s14 | nothing | English | 0 | 7, 0 (0), 0, 0 | 7, 0 (0), 0, 0 | 7, 0 (0), 0, 0 |
| s15 | gold | English | 2 | 7, 1 (0), 0, 0 | 7, 1 (0), 0, 0 | 7, 1 (0), 0, 0 |
| s16 | gold | English | 4 | 13, 3 (0), 3, 3 | 13, 3 (0), 3, 3 | 12, 3 (0), 3, 3 |
| s17 | gold | Thai or mixed | 2 | 5, 1 (0), 1, 1 | 5, 1 (0), 1, 1 | 5, 1 (0), 1, 1 |
| s18 | already | English | 3 | 6, 3 (1), 2, 2 | 6, 0 (0), 0, 0 | 6, 0 (0), 0, 0 |
| s19 | gold | English | 3 | 8, 2 (0), 2, 2 | 8, 2 (0), 2, 2 | 8, 2 (0), 2, 2 |
| s20 | nothing | English | 0 | 4, 0 (0), 0, 0 | 4, 0 (0), 0, 0 | 4, 0 (0), 0, 0 |
| s21 | gold | Thai or mixed | 3 | 13, 3 (0), 2, 2 | 12, 3 (0), 2, 2 | 11, 3 (0), 2, 2 |
| s22 | gold | English | 1 | 6, 1 (0), 1, 1 | 6, 1 (0), 1, 1 | 6, 1 (0), 1, 1 |
| s23 | nothing | Thai or mixed | 0 | 6, 0 (0), 0, 0 | 5, 0 (0), 0, 0 | 5, 0 (0), 0, 0 |
| s24 | gold | English | 2 | 6, 1 (0), 1, 1 | 6, 1 (0), 1, 1 | 6, 1 (0), 1, 1 |
| s25 | already | Thai or mixed | 2 | 7, 0 (1), 0, 0 | 7, 0 (1), 0, 0 | 7, 0 (1), 0, 0 |
| s26 | gold | English | 3 | 7, 4 (0), 3, 3 | 7, 3 (0), 3, 3 | 7, 3 (0), 3, 3 |
| s27 | gold | Thai or mixed | 2 | 6, 1 (0), 1, 1 | 6, 1 (0), 1, 1 | 6, 1 (0), 1, 1 |
| s28 | nothing | English | 0 | 4, 1 (0), 0, 0 | 4, 0 (0), 0, 0 | 4, 0 (0), 0, 0 |
| s29 | gold | English | 3 | 17, 6 (1), 2, 2 | 15, 3 (0), 2, 2 | 13, 4 (0), 3, 3 |
| s30 | gold | English | 1 | 8, 1 (0), 1, 1 | 6, 1 (0), 1, 1 | 6, 1 (0), 1, 1 |

## Run 1: what failed

Run 1 failed three thresholds: precision by 0.10, doubles by 3 and flood by 1. The causes, read from the recorded answers:

- **Precision (17 unmatched memories).**
  - 9 are user prompts that only ask, pick an offered option or say thanks: questions taken as howto, a pick taken as a
    decision, thanks taken as a handoff.
  - Two are compaction-summary sections with no lasting knowledge ("Key Technical Concepts", "Files and Code Sections").
  - One handoff list was cut into single bullets. The "Done" bullet won over the labelled "Next" bullet under the
    one-handoff rule.
  - The rest are narration, one user rule stored with a kind its item does not accept, and one plausible warning the
    labels do not have (s29).
- **Flood (3).** A user's bug report and the assistant's fix narration in s08, and a user's "thanks" in s28 taken as a
  handoff.
- **Doubles (4).** By cause, below.

## The tuning round

The changes below were made in one round, each to one of the knobs spec §11.3 allows (candidate rules, option wording,
`distill.min_confidence`). The set, the labels and the thresholds did not change. The wording uses no text from the set.

- **(a) Asks, picks and thanks,** by option wording only. No candidate rule was added. `ephemeral` was "progress
  narration, a plan for the next minutes, a greeting, or a result that matters only right now". It became "a question, a
  request or thanks to the assistant, or a pick among choices it offered, with no lasting rule or fact in it; progress
  narration; a plan for the next minutes; a greeting; or a result that matters only right now".
- **(c) Already saved.** `already_saved` was "says the same as something already saved in this session". It became "the
  same knowledge as an item listed under "Already saved in this session", even when worded differently or with more
  detail". "Already saved in this session" is the request state's own heading over the titles the session saved.
- **(d) Heading-led bullet groups,** a candidate rule. A heading line followed by list items now keeps the whole group in
  one passage, so a handoff holds what is done and what comes next together.
  - A heading line is either a heading (markdown, a numbered section head, or bold), or one line of at most 80
    characters that does not end in . ! ? or their full-width forms, with or without a colon. A Thai line has no
    sentence mark, so any Thai line of 80 characters or fewer before a list heads it.
  - A sentence before a list does not head it.
  - This changed the passages of 4 sessions (s21, s23, s29, s30). Questions went from 232 to 226.
- **`distill.min_confidence` stays at 0.70.** It was decided before run 2, by a rule the controller fixed in advance:
  replay run 1's recorded answers offline at 0.75 through the full keep rules (knowledge option, probability, at most 8
  per run, one handoff), and raise the threshold to 0.75 only if that keeps at least 33 of 50 gold items found.
  - The offline replica was checked first at 0.70: it reproduces what run 1 really stored, the same memories in every
    session (63) and 36/50 found.
  - At 0.75 it keeps 49 memories and finds only **32/50**, so the threshold stays at 0.70.
- **Guard: no labelled item lost its passage.** After the changes, the free dry run's script keeps exactly the passages
  that hold a labelled phrase. It still finds all 50 gold items and all 10 already items. So every labelled item is still
  a candidate and still reaches a stored body.
- **Unit tests:**
  - the distill module's 18 pass, including the new `a_heading_line_keeps_its_bullet_group_together`;
  - two existing tests changed only where they pinned exactly what this round changed: the option wording, and a
    summary heading's two bullets, now one passage;
  - `distill_flow.rs` passes 36 of 36.

**What each change did in run 2:**

- **(a)** removed 8 of the 9 user prompts. The one left is a pick that gives its reason (s21, decision 0.97).
- **(c)** made `already_saved` the answer for 6 of the 10 already items. Three more were answered with a knowledge
  option below 0.70, so they were never kept. The tenth, s25's howto, was kept and then trashed by the judge.
- **(d)** recovered s29's labelled handoff.
- **Recall:** the new wording also moved two summary bullets under 0.70 (s09's decision 0.74 → 0.64, s29's bugfix 0.76 →
  0.66). With the s29 handoff gained, recall went from 36 to 35.

## Doubles by cause

**(a)** preference_user routed to `global`: the distilled copy was never compared with the project's explicit copy.
Task 11b now compares it with its source project too.
**(b)** A distilled "updates" of an explicit memory, left live as `RelatesTo`. Task 11b now trashes it as `duplicate_of`
the explicit memory when that `updates` is at or above the review threshold (0.5).

| Run | Session | Item | Cause | What happened |
|---|---|---|---|---|
| 1 | s11 | «ตอบผมเป็นภาษาไทยเสมอ» (preference) | (a) | kept as `preference_user` (0.73; `already_saved` 0.27) and written to `global`, while the agent's copy is in the project. The judge compares within a project, so the pair never met |
| 1 | s18 | «never run anything that writes to the production warehouse» (preference) | (a) | the same: `preference_user` 0.72 (`already_saved` 0.24), written to `global` |
| 1 | s11 | «…toMinutes()» (bugfix) | (b) | kept as bugfix (0.81). The judge read it as `updates` of the agent's memory (forward 0.88, reverse 0.81, averaging about 0.85), so it was linked, not trashed |
| 1 | s18 | «partitioned by business_date» (fact) | (b) | kept as fact (0.84). The judge read `updates` (forward 0.64, reverse about 0.5), so it was linked and stayed live |
| 2 | — | none | — | 6 of the 10 items answered `already_saved`. s11's preference (0.56), s11's bugfix (0.62) and s18's fact (0.66) were answered with knowledge but below 0.70. s25's howto (0.87) was kept and then trashed as a duplicate (reverse 0.84) |
| 3 | — | none | — | 6 of the 10 items answered `already_saved`. s11's bugfix (0.58) and s18's fact (0.59) were answered with knowledge below 0.70, so they were never kept. s11's preference was kept as `preference_user` (0.73) and routed to `global`; Task 11b's comparison with the source project found the agent's copy (duplicate, forward 0.80, reverse 0.79) and trashed it. That is cause (a), now caught. s25's howto (0.91) was kept and then trashed as a duplicate (forward 0.55, reverse 0.78) |

**What the judge caught.** In run 1 it trashed 6 distilled memories as duplicates of older ones:

- 2 held an already item (s18's decision, s25's howto);
- 3 were other near-copies of the agent's memories (s05 ×2, s11's bug report);
- 1 was a summary section in s29.

In run 2 it trashed s25's howto again. In run 2, three items avoided being doubles only because their confidence was just
under 0.70, so causes (a) and (b) still applied then.

**Task 11b (made before run 3).** Both causes are now product rules in the judge:

- a distilled memory routed to `global` is also compared with its source project (with no supersede across projects);
- a distilled newcomer whose strongest relation to an explicit memory is `updates` at or above the review threshold
  (0.5) is trashed as `duplicate_of` that memory, with no review item.

The (b) rule fires at the review threshold, not at auto (0.85): run 1's two (b) doubles averaged `updates` at about 0.845
and about 0.55, so a rule at auto would have caught neither, while this one catches both. In run 3, rule (a) caught s11's
preference; rule (b) was not exercised, because no distilled memory was judged an update of an explicit one.

## Run 2: what is left

- **4 unmatched memories:**
  - s08's fix narration (bugfix 0.86), which is also the flood of 1;
  - s15's "changed these files and added a test" narration (bugfix 0.74);
  - s21's user pick with its reason (decision 0.97);
  - s29's "Key Technical Concepts" summary section (fact 0.86).
- **15 missed gold items:**
  - 12 were answered with an accepted kind at 0.33–0.67. The probability was spread over several knowledge options, so
    none reached 0.70.
  - 3 were answered with a kind their item does not accept (s03's project rule as a warning, s15's fix as a project
    convention, s24's command as a bugfix).
  - Project conventions the user states ("the rule of this project is …") are the weakest kind. 3 of 6 preference items
    were missed, all of them such rules: two by spread (`preference_project` at 0.57 and 0.49), and one taken as a
    warning.

## Run 3: re-recorded after Tasks 12b and 11b

Run 3 re-records the evaluation after two **product fixes** that came from the owner's real-session check (§11.4) and
from this evaluation's findings, not from tuning on this set.

- **Not a second tuning round:** the set, the labels and the thresholds are unchanged, and no product rule or setting
  was changed after run 3.
- **One change to the evaluation:** after run 3 it learned to score a `superseded` memory as not live (see
  "Determinism").
- **Why re-record:** both tasks change what Jev is asked, so run 2's recorded answers no longer replay.

- **Task 12b** (distillation does not flood a long session):
  - `ephemeral` also covers "a verdict or status report on one task (a review's "approved", "addressed" or "no
    findings", a checkmark, a count of passing or failing tests, a "done", "fixed" or "pushed" note) that states no
    lesson for later work". Run 2's wording (questions, requests, thanks, picks) is kept.
  - `preference_user` is now "a preference of this person that holds in every project (tools, style, language)", and
    `preference_project` "a rule or convention for this repository only".
  - A cap per session (`distill.max_per_session`, 16) and a share for subagent runs (`distill.max_per_subagent_run`, 2).
    Within those caps, passages rank by priority first (the main session before a subagent, then decision, bugfix,
    warning, fact, howto, preference) and by p second. No session of this set reaches the caps.
  - A title is taken from the passage's first sentence when the heading is bare, and compaction summaries in the real
    format are cut into their sections. Questions went from 226 to 220 (s04, s09, s16, s21 and s29).
- **Task 11b** (the judge as the dedupe net): see "Doubles by cause" above. It also makes stale ends due, which this
  evaluation does not exercise.

**What changed from run 2:**

- **Recall** went from 35 to 36.
  - s13's project rule «ใบกำกับภาษีทุกใบต้องแสดงปีเป็น พ.ศ.» is now found.
  - s29's «client_max_body_size 20m» is now found: its summary section was answered bugfix 0.85.
  - s03's «amount ใน payload ของ PromptPay ต้องไม่มี comma» is now missed (bugfix 0.69, just under 0.70).
- **Precision** stayed at 0.90 (36/40). The 4 unmatched memories are:
  - s08's fix narration (bugfix), which is also the flood of 1;
  - s15's fix, taken as a project rule under the new `preference_project` wording (0.71);
  - s21's user pick with its reason (decision);
  - s29's "Key Technical Concepts" summary section (fact).
- **Doubles** stayed at 0, now with the judge catching a would-be double by cause (a) (see the table above).

**The flood set** (Task 12b's separate test, `distill_eval_flood_replay`; Task 13's set and thresholds are untouched).
One session runs a plan through twenty reviewer subagents, each answering with a one-task verdict or status report,
next to three lessons. One lesson is stated inside a reviewer's verdict. Its thresholds were declared in Task 12b before
any run.

| | Run 3 | Threshold |
|---|---|---|
| Verdicts that reached Jev | 20 of 20 | at least 18 |
| Verdicts answered `ephemeral` (dropped) | 19 of 20 | more than half |
| Verdicts kept | 1 | at most 5 |
| Lessons kept | 3 of 3 | all 3 |
| Distilled / live memories | 5 / 4 (1 superseded) | — |
| Questions / Jev requests / spend | 28 / 5 / $0.0008 | — |

- **The kept verdict:** a task 5 code review that reports one important finding, fixed and checked again (bugfix).
- **Lessons kept:**
  - the lesson inside a reviewer's verdict, that the payout worker must lock the account row, kept as a warning;
  - the warning about `sqlx migrate revert` on the shared staging database;
  - the backoff decision.
- **The fifth memory:** the user's question "How many times should a failed payout be retried? Settle it before task
  4.", kept as a decision. The judge then superseded it with the decision that answers it, Task 11's rule for a
  distilled update of an older distilled memory. A superseded memory is not live, and recall hides it.
- **A candidate-quality observation** (for the controller; nothing was changed): Jev answered that user question
  `decision` at 0.95 (`ephemeral` 0.05), although run 2's `ephemeral` wording covers questions. The judge cleaned it up
  here only because the answering decision came later in the same session.

## Spend

| | Run 1 | Run 2 | Run 3 | Run 3, flood set |
|---|---|---|---|---|
| Requests, distillation and the judge's | 73 | 53 | 55 | 5 |
| Spend, from Jev's reported input tokens (the cost ledgers agree) | $0.0068 | $0.0059 | $0.0065 | $0.0008 |
| The free dry run's upper bound (1.41 tokens per byte, plus 300 per request) | $0.0307 | $0.0329 | $0.0356 | $0.0031 |

Run 3's two live runs shared one $0.10 stop: the flood run was given $0.093, what the first left. They cost $0.0072
together, and all runs together cost $0.0200.

## Determinism

The replay hashes the exact request body with SHA-256. Nothing is normalised before hashing, so any drift fails the
replay.

**Clocks.**

- The distillation request holds no clock.
- The judge's comparison says how old each memory is (`written <age>`). The writer's clock and the judge's are both the
  real clock, and neither can be fixed from a test.
- The age still always reads "today". Every memory is written and then judged within one run, so the gap is under 24 h,
  and it is above -60 s, because `age` reads skew under a minute as "today".
- Both tests fail on any request that gives another age, and name its body.

**Evidence:**

- **Dry run, run 1 wording:** the same 76 request bodies twice, in the same order.
- **Dry run, run 2 wording:** the same 76 request bodies twice, in the same order.
- **Replay of run 1** (once, before tuning): the report was identical to the live run's. It failed only on the run's
  thresholds.
- **Replay of run 2** (twice in a row): both passed, and both reports were identical to the live run's.
- **Dry runs, run 3 code:** the same 76 request bodies twice, in the same order, still extracting 50/50 gold and 10/10
  already items; the flood set's 3 request bodies twice, in the same order.
- **Replay of run 3** (twice in a row, both sets): all four passed. Each main-set replay reproduced the live run's
  report and its 30 per-session lines. The comparison set aside the report's new superseded column, which is zero for
  every session. Both flood replays printed the same verdict answers and memories as the replay of the live recording.
  The flood live run itself stopped at the evaluation's old status check, which then knew only active and trashed, after
  every request had been recorded. The ruling to score `superseded` as not live came next, with no new live run.
- **What the replay requires:** every request must have a recorded answer, and every recorded answer must be asked for.

`recorded.jsonl` holds run 3's 55 requests (595 KB), and `flood/recorded.jsonl` the flood run's 5 (74 KB), bodies only.
Each live test rewrites its file, so runs 1 and 2 are kept here as numbers only.

## Caveats

- **Labels and set.** The labels are one author's, the same person who wrote the transcripts, and the set is synthetic.
  The labels also miss some plausible knowledge: run 1's s29 warning counted against precision.
- **Small samples.** There are 50 gold items, 10 already items and 6 nothing sessions, so the intervals are wide. The
  recall intervals of run 2 (0.56–0.81) and run 3 (0.58–0.83) do not rule out a recall under 0.60. The flood set is one
  session.
- **Not an independent test.** Run 2 was tuned on the set it is scored on: one round, chosen after reading run 1's
  errors. Run 3 keeps that tuning; its own changes came from the real sessions and from run 1's doubles, not from this
  set's misses. The owner's real sessions (§11.4) are the independent check.
- **Fake neighbours.** The judge's neighbours come from the tests' hashed fake embedder, not the real model, so which
  older memories it compares can differ in production.
- **Jev varies.** Its answers to the same wording can vary between runs. The replay pins the answers recorded here, not
  Jev's behaviour.
- **One platform, and a timing window.** The replay was checked on macOS arm64 only.
  - **Neighbour order is not the risk.** The fake vectors are bit-exact, and the closest two neighbours in any recorded
    comparison differ in similarity by 0.02 or more.
  - **The residual is a timing window in the product.** The embed worker marks a memory's embedding done before it adds
    the vector to the index. On a loaded runner, the judge can read a newer memory's neighbours before an older
    memory's vector is in the index. The newer memory's comparison body then differs.
  - **Effect:** the replay fails loudly, with a miss and an unused answer, and never passes falsely.
  - **Owner:** the fix is in product code, and it is a follow-up for pane 4.
