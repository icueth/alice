# Jev evaluation (M2, Task 10)

Measured on 2026-10-14 on `feat/m2-judge-settings`. **Single run** (the second of the day; see "Two runs" below): every number comes from one execution of the evaluation; nothing was repeated or averaged. The set is **synthetic**: it was written for this evaluation (by the engineer, not by users), holds no real user data, and its labels are one author's judgement of what a careful reader would choose.

| | |
|---|---|
| Machine | Apple M4 Pro (14 cores), 24 GB RAM, macOS 26.6.2 |
| Judge | TypeSafe Jev `jev-1.13.0` (`https://api.typesafe.ai/v1/systemone`), 4 parallel workers, at most 8 requests/s, $2 hard budget per run enforced by the judge (it counts only the run's own spend) |
| Embedder | `multilingual-e5-small-q8` (ONNX Runtime, local) |
| Heuristic | `HeuristicJudge::default()` (not tuned on this set) |
| Test | `crates/alice-daemon/tests/jev_eval.rs` (ignored), data in `tests/fixtures/judge_eval_pairs.jsonl` and `judge_eval_queries.jsonl` |

```
# estimate without key or network (requests are built and counted, never sent)
JEV_EVAL_DRY=1 ALICE_MODELS_DIR=$HOME/.alice/models cargo test -p alice-daemon --release --test jev_eval -- --ignored --nocapture
# the run (the call log is truncated at the start; ALICE_EVAL_REUSE=1 would replay it instead)
set -a; . ./.env; set +a
ALICE_MODELS_DIR=$HOME/.alice/models cargo test -p alice-daemon --release --test jev_eval -- --ignored --nocapture
unset TYPESAFE_API_KEY
```

Cost: the dry run's upper bound (the judge's reservation rule, 1.5 tokens per byte plus 300 per request) was $0.5154 and a calibrated estimate (0.6 tokens per byte, from the first run's measured 0.43) $0.217; the ledger says **$0.1148** (1,447 requests, 2,734,033 input tokens).

## Two runs

Run 1 (cost $0.0222, 2026-10-14) used a first version of the set whose metadata gave the labels away: a rule that never read the text (same title and same age is a contradiction, same title and a newer memory an update, different kinds unrelated, else duplicate) scored 86.1% on it, bodies were one short sentence, and the floor evidence only covered retrieved results. Its numbers (Jev 0.958 against the heuristic's 0.458) are **superseded and not used below**. This page reports run 2 (set rebalanced, bodies lengthened, whole-corpus grading) only.

## The set

216 relation pairs; counts per label and language:

| Label | th | en | mixed | code | Total |
|---|---|---|---|---|---|
| duplicate | 16 | 16 | 11 | 11 | 54 |
| updates | 16 | 16 | 11 | 11 | 54 |
| contradicts | 16 | 16 | 11 | 11 | 54 |
| unrelated | 16 | 16 | 11 | 11 | 54 |
| all | 64 | 64 | 44 | 44 | 216 |

Kinds of the new memory: fact 73, decision 29, status 24, howto 20, preference 15, bugfix 15, warning 14, summary 7, note 7, diary 6, handoff 6. Composition rules, enforced in CI (`judge_eval_fixture.rs`):

- Metadata must not decide. Titles are identical in about 35% of the pairs of every label; ages are spread over "new is newer / same age / new is older" for every label (updates are never older: 40 newer, 14 same-day); 18 contradictions have the same age and 18 each are newer or older than their partner; kinds differ in 30 of the 54 duplicate, update and contradiction pairs each and in 46 of the unrelated pairs. The fixed metadata rule scores **36.1%** and the best table over (same title, age order, same kind) fitted on the set **38.9%**; CI requires both to stay at 50% or less.
- 26 unrelated pairs share an uncommon word (a topic); a further 19 share a generic title ("Reference", "Details"). 39 duplicates are paraphrases (character-trigram Jaccard of the two bodies under 0.3).
- Bodies are 160 to 266 characters (median 194). To get paragraph-length memories, a label-independent context sentence (about how or why the note was written, no facts) was appended to the bodies, chosen at random per memory from eight sentences per language; the core statement is the one or two sentences before it.
- 70 queries: 50 same-language, 12 cross-lingual (Thai for English and the reverse), 8 paraphrase-only (no shared content word); 9 have no answer in the corpus. The corpus for search is the 432 memories of the pairs.

The kind of the new memory is sent as `note` (the default) in the kind test, because the request states the memory's kind and the true one would give the answer away.

## Reading the results

- **Relation.** Jev 0.917 accuracy (95% interval 0.872–0.947), macro-F1 0.919. The metadata-only rule gets 0.361 and the best metadata table 0.389, so the labels are no longer readable from titles, ages and kinds. The heuristic scores 0.287: it says "unrelated" for 199 of 216 pairs (duplicates 2% recall, updates 13%, contradictions 0%; its title-similarity rule fails when titles differ, and its duplicate rule needs a very high cosine). Jev never calls a related pair unrelated and never calls an unrelated pair related; its errors are between the related labels: 10 contradictions and 7 duplicates read as updates, 1 update read as a contradiction. Per language: English 0.984, Thai 0.906, code 0.886, mixed 0.864 (44–64 pairs each; intervals overlap).
- **Calibration and auto-apply.** Jev Brier 0.146, ECE 0.068 (heuristic 1.419 and 0.694). At `auto_apply_threshold = 0.85`: 80 pairs applied (49% of the actionable ones), all correct: precision 100%, 95% Wilson interval 95–100% (lower bound 95.4%). At 0.70 it is 94% (88–97%) with 71% coverage, at 0.50 89% (83–93%). So 0.85 meets the 95% target on this set, but **only just, and not for real traffic**: the set is balanced (25% unrelated) and no unrelated pair is ever applied here, whereas real traffic is mostly unrelated pairs, so precision there can be lower. The verdict is "consistent with 0.85; not established". The heuristic applies 4 pairs at 0.85 (1 right).
- **Forward/reversed agreement.** 93.1% of pairs agree. Accuracy when they agree is 92% (184/201) and when they disagree 93% (14/15): on this set, disagreement does **not** flag errors, so it is not a usable "send to review" signal.
- **Kind.** Jev 0.769 against 0.218 for the heuristic (0.761 against 0.206 without the 7 gold `note` memories). Kinds overlap (a status that is also a fact), so some Jev misses are label fuzziness.
- **Search.** nDCG@10 on the 61 answerable queries: hybrid at the shipped floor 0.791, vector only without a floor 0.786, vector only at 0.82 0.720, lexical only 0.633. Paired bootstrap: hybrid beats lexical-only (+0.158, interval +0.097 to +0.227) and vector-only at the shipped floor (+0.071, +0.028 to +0.118); hybrid against vector-only without a floor is +0.005 (−0.054 to +0.061), **no difference established**. On the 20 cross-lingual and paraphrase queries vector-only without a floor scores 0.704 against 0.621 for hybrid, but the difference (−0.083, −0.228 to +0.041) includes 0 (n = 20). Queries nothing answers: the best graded result is near zero for most, but the Thai Kubernetes query returns 9 results with a best grade of 0.60 and the tokio query 2 with 0.29; the floor does not stop all junk. Caveats: nDCG's ideal ranking is pooled from the four modes only; the corpus is 432 memories of one synthetic domain; one answerable query ("limit on simultaneous database links", paraphrase of `pool.max_connections`) has no memory that Jev grades as answering, so its label should be rechecked.
- **Cost and latency.** 1,447 requests, 2,734,033 input tokens, $0.1148; $0.0113 per 1,000 decisions (10,199 decisions, most of them whole-corpus grading); latency p50 335 ms, p95 664 ms (4 parallel workers, no retries, no failures).

### What the floor evidence says (input for Task 13)

No threshold or heuristic was tuned; these are observations. Section A (the whole corpus graded for the 20 cross-lingual and paraphrase queries) is the evidence for the floor; section B is conditional on retrieval and must not be read as recall. The cosines of this embedder are compressed (median of a random memory against a query: 0.77).

- Memories Jev says answer a cross-lingual or paraphrase query (70 of them) have a median cosine of 0.833 (min 0.721, p10 0.789); the other 8,570 memories 0.769 (p90 0.809). The shipped floor 0.82 keeps **67% (56–77%)** of the answering memories by cosine and 73% (61–82%) when a lexical match is counted; cross-lingual 84% / 89%, paraphrase only **47% (31–64%)** / 53%. It admits 4% (6% with lexical) of the other memories.
- Lower floors: 0.80 keeps 79% (68–87%) / 84% and admits 17%; 0.78 keeps 93% (84–97%) / 96% and admits 38%; 0.75 keeps 99% and admits 72%; 0.70 admits 98%. A floor does separate here, but the answering memories' lower tail overlaps the others' upper tail, so an absolute floor trades recall against noise steeply between 0.78 and 0.82.
- If "relevant" also includes memories about the query's subject without answering it (225 pairs), recall at 0.82 is 37% (31–43%) by cosine and 49% with lexical; those memories sit well below the floor.
- Section B (same-language queries, retrieved results only) shows 91% (84–95%) of answering results above 0.82 and 98% above 0.80, but that is conditional on retrieval and the pool's cosines never go below 0.74. The first run's sentence "a floor of 0.78–0.80 loses almost nothing relevant" came from this view and is withdrawn: against the whole corpus the same floors lose 7–21% of the answering cross-lingual and paraphrase memories.
- Suggestions for Task 13 (not applied here): 0.82 loses 16% of the cross-lingual and about half of the paraphrase answers (a third of the two groups together); 0.78–0.80 recovers most of them at the cost of more noise, which a relative cut (against the best score) or a Jev relevance grade where the project may use the cloud could handle.

## Limits

Single run, one machine, one author's labels, 216 pairs and 70 queries on a 432-memory corpus; the paraphrase group has 32 answering memories and the cross-lingual one 38, hence the wide intervals. The intervals also treat the answering memories as independent although they come from 20 queries in one synthetic domain, so the true uncertainty is larger than they show. Candidates are graded ten at a time in a fixed shuffled order, so a grade may depend on the batch it was in. Floor cosines are the model's exact cosines; the index's own similarity (int8) can differ slightly. "Lexical" means the memory is in the lexical-only top 50, not that the shipped search would return it at that rank. The kind test sends `note`, which slightly favours the 7 gold `note` memories (see the number without them). The call log (`target/m2-jev-calls.jsonl`, no headers, no key) and the raw rows (`target/m2-jev-floor.json`) are not committed.

## Raw output of the run

Fresh run, 2026-10-14 11:45 UTC. Jev model jev-1.13.0, embedder multilingual-e5-small-q8 (432 passages embedded in 1.8 s). No call was replayed (the call log is truncated at the start; 0 requests were answered from this run's own cache).

### Relation

| Judge | Pairs | Accuracy (95% Wilson) | Macro-F1 | Brier | ECE | Auto-apply coverage at 0.85 | Auto-apply precision at 0.85 (95% Wilson) |
|---|---|---|---|---|---|---|---|
| metadata only: fixed rule (title, age order, kind; no text) | 216 | 0.361 (0.300–0.427) | 0.336 | – | – | – | – |
| metadata only: best table fitted on this set (upper bound) | 216 | 0.389 (0.326–0.455) | 0.345 | – | – | – | – |
| heuristic | 216 | 0.287 (0.231–0.351) | 0.168 | 1.419 | 0.694 | 2% | 25% (5–70%, 1/4) |
| jev-1.13.0 | 216 | 0.917 (0.872–0.947) | 0.919 | 0.146 | 0.068 | 49% | 100% (95–100%, 80/80) |

Heuristic: rows are the gold label, columns the prediction

| gold \ predicted | duplicate | updates | contradicts | unrelated | recall |
|---|---|---|---|---|---|
| duplicate | 1 | 3 | 0 | 50 | 2% |
| updates | 1 | 7 | 0 | 46 | 13% |
| contradicts | 2 | 3 | 0 | 49 | 0% |
| unrelated | 0 | 0 | 0 | 54 | 100% |

Jev: rows are the gold label, columns the prediction

| gold \ predicted | duplicate | updates | contradicts | unrelated | recall |
|---|---|---|---|---|---|
| duplicate | 47 | 7 | 0 | 0 | 87% |
| updates | 0 | 53 | 1 | 0 | 98% |
| contradicts | 0 | 10 | 44 | 0 | 81% |
| unrelated | 0 | 0 | 0 | 54 | 100% |

| Language | Pairs | Heuristic accuracy | Jev accuracy |
|---|---|---|---|
| code | 44 | 0.318 | 0.886 |
| en | 64 | 0.266 | 0.984 |
| mixed | 44 | 0.295 | 0.864 |
| th | 64 | 0.281 | 0.906 |

Auto-apply sweep (non-`unrelated` predictions with P at or above the threshold; coverage is the share of the actionable pairs; precision is the share right, with its 95% Wilson interval and counts). **The set is balanced (25% unrelated pairs); real traffic is mostly unrelated pairs, and no unrelated pair is ever applied here, so precision on real traffic can be lower.**

| Threshold | Heuristic coverage / precision | Jev coverage / precision |
|---|---|---|
| 0.50 | 10% / 47% (26–69%, 8/17) | 98% / 89% (83–93%, 141/159) |
| 0.70 | 10% / 47% (26–69%, 8/17) | 71% / 94% (88–97%, 108/115) |
| 0.85 | 2% / 25% (5–70%, 1/4) | 49% / 100% (95–100%, 80/80) |
| 0.90 | 2% / 25% (5–70%, 1/4) | 38% / 100% (94–100%, 61/61) |
| 0.95 | 2% / 25% (5–70%, 1/4) | 24% / 100% (91–100%, 39/39) |
| 0.99 | 0% / – | 9% / 100% (80–100%, 15/15) |

Forward/reversed agreement (Jev): 93.1% of 216 pairs; accuracy when they agree 92% (87–95%, 184/201) , when they disagree 93% (70–99%, 14/15)

### Kind classification

The memory is sent with the default kind (`note`) so the request does not state the answer.

Kind accuracy: heuristic 0.218, Jev 0.769 (216 memories); without the 7 gold `note` memories (the default the request states): heuristic 0.206, Jev 0.761

### Search, Jev-graded

Grade = 2 × P(answers) + P(partially), used as the gain `2^g - 1`; the ideal ranking is the best of every candidate any of the four modes returned (pooling). The answerable-queries column leads: for a query nothing answers, the ideal is near zero and an empty result scores 0, so the all-queries column rewards returning junk.

| Recall mode | nDCG@10, answerable queries | nDCG@10, cross-lingual and paraphrase | nDCG@10, all queries |
|---|---|---|---|
| hybrid (floor 0.82) | 0.791 | 0.621 | 0.726 |
| lexical only | 0.633 | 0.424 | 0.572 |
| vector only (floor 0.82) | 0.720 | 0.543 | 0.644 |
| vector only, no floor | 0.786 | 0.704 | 0.734 |

Queries: 61 answerable, 20 cross-lingual or paraphrase (all answerable), 70 in all.

Paired bootstrap (10,000 resamples over queries) of the nDCG@10 difference, 95% interval; a difference whose interval includes 0 is not established:

| Difference | Answerable queries | Cross-lingual and paraphrase |
|---|---|---|
| hybrid − vector only, no floor | +0.005 (-0.054 to +0.061) | -0.083 (-0.228 to +0.041) |
| hybrid − vector only (floor 0.82) | +0.071 (+0.028 to +0.118) | +0.078 (-0.013 to +0.183) |
| hybrid − lexical only | +0.158 (+0.097 to +0.227) | +0.197 (+0.092 to +0.312) |

Answerable queries for which Jev found no memory that answers them (P(answers) < 0.5 for the whole graded set), so their nDCG compares near-zero grades: limit on simultaneous database links

Queries no memory answers: best Jev grade among the results of hybrid / vector only (floor 0.82) / all graded candidates (result count in brackets); a grade near 0 is right.

| Query | Language | Hybrid | Vector only | Graded |
|---|---|---|---|---|
| วิธีตั้งค่า Kubernetes cluster | th | 0.60 (9) | 0.60 (9) | 0.60 (21) |
| สูตรทำต้มยำกุ้ง | th | 0.00 (0) | 0.00 (0) | 0.01 (20) |
| how to configure Kubernetes autoscaling | en | 0.01 (2) | 0.01 (1) | 0.11 (21) |
| best pizza recipe | en | 0.00 (0) | 0.00 (0) | 0.00 (20) |
| what is the weather in Paris | en | 0.00 (0) | 0.00 (0) | 0.01 (20) |
| วิธีทำ Kubernetes autoscaling | mixed | 0.02 (7) | 0.02 (7) | 0.15 (21) |
| ราคา Bitcoin วันนี้ | mixed | 0.04 (10) | 0.00 (6) | 0.04 (21) |
| tokio::select! cancellation safety | code | 0.29 (2) | 0.00 (0) | 0.29 (20) |
| BTreeMap range query | code | 0.08 (2) | 0.00 (0) | 0.15 (22) |

### Relevance floor evidence for Task 13

The cosine is the real model's exact cosine between the query and the memory. "Lexical" means the memory is in the top 50 of the lexical-only search (a lexical match passes the floor at any similarity). Shares come with 95% Wilson intervals and counts.

#### A. Whole corpus: every memory graded for the 20 cross-lingual and paraphrase queries

Jev graded all 432 memories against each of these queries, so recall at a floor is measured against every memory that answers the query, retrieved or not. "Other" is every other memory of the corpus, so the admission columns are the real admission rate of a floor over this corpus.

**cross-lingual and paraphrase, relevant = Jev says the memory answers the query (P(answers) ≥ 0.5)**: 70 relevant, 8570 other memories. Cosine min / p10 / median / p90: relevant 0.721 / 0.789 / 0.833 / 0.871; other memories 0.662 / 0.727 / 0.769 / 0.809.

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | other memories admitted (cosine) | other memories admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (95–100%, 70/70) | 100% (95–100%, 70/70) | 98% (98–99%, 8440/8570) | 98% (98–99%, 8440/8570) |
| 0.75 | 99% (92–100%, 69/70) | 99% (92–100%, 69/70) | 72% (71–73%, 6157/8570) | 72% (71–73%, 6168/8570) |
| 0.78 | 93% (84–97%, 65/70) | 96% (88–99%, 67/70) | 38% (37–39%, 3250/8570) | 39% (38–40%, 3303/8570) |
| 0.80 | 79% (68–87%, 55/70) | 84% (74–91%, 59/70) | 17% (17–18%, 1486/8570) | 19% (18–19%, 1599/8570) |
| 0.82 | 67% (56–77%, 47/70) | 73% (61–82%, 51/70) | 4% (4–4%, 341/8570) | 6% (6–7%, 541/8570) |

**cross-lingual, relevant = Jev says the memory answers the query (P(answers) ≥ 0.5)**: 38 relevant, 5146 other memories. Cosine min / p10 / median / p90: relevant 0.780 / 0.809 / 0.849 / 0.873; other memories 0.667 / 0.729 / 0.774 / 0.812.

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | other memories admitted (cosine) | other memories admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (91–100%, 38/38) | 100% (91–100%, 38/38) | 99% (98–99%, 5069/5146) | 99% (98–99%, 5069/5146) |
| 0.75 | 100% (91–100%, 38/38) | 100% (91–100%, 38/38) | 74% (73–75%, 3816/5146) | 74% (73–75%, 3821/5146) |
| 0.78 | 97% (87–100%, 37/38) | 100% (91–100%, 38/38) | 43% (42–44%, 2217/5146) | 43% (42–45%, 2235/5146) |
| 0.80 | 92% (79–97%, 35/38) | 97% (87–100%, 37/38) | 20% (19–22%, 1049/5146) | 21% (20–23%, 1101/5146) |
| 0.82 | 84% (70–93%, 32/38) | 89% (76–96%, 34/38) | 5% (4–5%, 237/5146) | 7% (6–7%, 335/5146) |

**paraphrase, relevant = Jev says the memory answers the query (P(answers) ≥ 0.5)**: 32 relevant, 3424 other memories. Cosine min / p10 / median / p90: relevant 0.721 / 0.765 / 0.820 / 0.847; other memories 0.662 / 0.725 / 0.764 / 0.804.

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | other memories admitted (cosine) | other memories admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (89–100%, 32/32) | 100% (89–100%, 32/32) | 98% (98–99%, 3371/3424) | 98% (98–99%, 3371/3424) |
| 0.75 | 97% (84–99%, 31/32) | 97% (84–99%, 31/32) | 68% (67–70%, 2341/3424) | 69% (67–70%, 2347/3424) |
| 0.78 | 88% (72–95%, 28/32) | 91% (76–97%, 29/32) | 30% (29–32%, 1033/3424) | 31% (30–33%, 1068/3424) |
| 0.80 | 62% (45–77%, 20/32) | 69% (51–82%, 22/32) | 13% (12–14%, 437/3424) | 15% (13–16%, 498/3424) |
| 0.82 | 47% (31–64%, 15/32) | 53% (36–69%, 17/32) | 3% (3–4%, 104/3424) | 6% (5–7%, 206/3424) |

**cross-lingual and paraphrase, relevant = answers or is about the subject (P(answers) + P(partially) ≥ 0.5)**: 225 relevant, 8415 other memories. Cosine min / p10 / median / p90: relevant 0.714 / 0.754 / 0.804 / 0.853; other memories 0.662 / 0.727 / 0.769 / 0.809.

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | other memories admitted (cosine) | other memories admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (98–100%, 225/225) | 100% (98–100%, 225/225) | 98% (98–99%, 8285/8415) | 98% (98–99%, 8285/8415) |
| 0.75 | 92% (88–95%, 207/225) | 92% (88–95%, 208/225) | 72% (71–72%, 6019/8415) | 72% (71–73%, 6029/8415) |
| 0.78 | 68% (62–74%, 154/225) | 71% (65–77%, 160/225) | 38% (37–39%, 3161/8415) | 38% (37–39%, 3210/8415) |
| 0.80 | 53% (46–59%, 119/225) | 60% (53–66%, 135/225) | 17% (16–18%, 1422/8415) | 18% (17–19%, 1523/8415) |
| 0.82 | 37% (31–43%, 83/225) | 49% (42–55%, 110/225) | 4% (3–4%, 305/8415) | 6% (5–6%, 482/8415) |

#### B. Among retrieved results: the 50 same-language queries

Only the candidates the four recall modes returned plus the vector top 20 without a floor were graded (1127 pairs), so every row has a cosine of at least 0.74 and the 0.70 and 0.75 rows are 100% by construction. Recall is conditional on being retrieved; the admission columns describe retrieved candidates (a harder set than the whole corpus).

**same-language queries, among retrieved results, relevant = answers (P(answers) ≥ 0.5)**: 112 relevant, 1015 other graded. Cosine min / p10 / median / p90: relevant 0.783 / 0.827 / 0.886 / 0.917; other graded 0.739 / 0.781 / 0.819 / 0.844.

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | other graded admitted (cosine) | other graded admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (97–100%, 112/112) | 100% (97–100%, 112/112) | 100% (100–100%, 1015/1015) | 100% (100–100%, 1015/1015) |
| 0.75 | 100% (97–100%, 112/112) | 100% (97–100%, 112/112) | 100% (99–100%, 1011/1015) | 100% (100–100%, 1015/1015) |
| 0.78 | 100% (97–100%, 112/112) | 100% (97–100%, 112/112) | 91% (89–92%, 920/1015) | 93% (92–95%, 947/1015) |
| 0.80 | 98% (94–100%, 110/112) | 100% (97–100%, 112/112) | 81% (78–83%, 819/1015) | 87% (85–89%, 886/1015) |
| 0.82 | 91% (84–95%, 102/112) | 96% (91–99%, 108/112) | 47% (44–50%, 480/1015) | 62% (59–65%, 630/1015) |

**same-language queries, among retrieved results, relevant = answers or about the subject**: 217 relevant, 910 other graded. Cosine min / p10 / median / p90: relevant 0.763 / 0.807 / 0.848 / 0.910; other graded 0.739 / 0.780 / 0.818 / 0.841.

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | other graded admitted (cosine) | other graded admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (98–100%, 217/217) | 100% (98–100%, 217/217) | 100% (100–100%, 910/910) | 100% (100–100%, 910/910) |
| 0.75 | 100% (98–100%, 217/217) | 100% (98–100%, 217/217) | 100% (99–100%, 906/910) | 100% (100–100%, 910/910) |
| 0.78 | 98% (95–99%, 212/217) | 99% (96–100%, 214/217) | 90% (88–92%, 820/910) | 93% (91–94%, 845/910) |
| 0.80 | 94% (90–96%, 204/217) | 99% (96–100%, 214/217) | 80% (77–82%, 725/910) | 86% (84–88%, 784/910) |
| 0.82 | 74% (68–80%, 161/217) | 92% (87–95%, 199/217) | 46% (43–50%, 421/910) | 59% (56–62%, 539/910) |

### Floor policy chosen in Task 13

**Tuned on the synthetic set above; not evidence about any other corpus.** The shipped search
now (1) rescores each memory candidate with its exact stored f16 vector before the floor is
applied (the int8 HNSW similarity can sit a little low; the rows above already hold exact
cosines, so this step cannot show up in the numbers below), (2) applies an absolute floor
`search.relevance_floor` and (3) drops a vector-only candidate more than `search.relevance_margin`
below the best vector similarity of its tier among the candidates that pass the filters. A
lexical match passes at any similarity, as before.

*Protocol.* The 20 cross-lingual and paraphrase queries (Jev graded every memory for them; 70
memories answer, P(answers) >= 0.5, and one query has no answer, so 19 queries enter the recall
averages). Grid: floor 0.76 to 0.80 in steps of 0.005, margin 0.01 to 0.20 in steps of 0.005, and
no margin. Objective: the mean over queries of the share of answering memories kept (cosine or
lexical top 50). Constraint: the share of the other memories admitted, pooled over the queries
used for choosing, is at most what floor 0.82 admits on the same queries (6.3% over all 20).
Leave-one-query-out: choose on 19 queries, score the held-out one, 20 folds.

*Choices.* Floor 0.79 with margin 0.03 in 11 folds, 0.79 with 0.02 in 8, 0.795 with 0.03 in 1;
on all 20 queries 0.79 and 0.03 (the defaults).

| | before: floor 0.82, no margin | chosen: floor 0.79, margin 0.03 |
|---|---|---|
| held-out (leave-one-query-out), recall of answering memories, mean over 19 queries | 74.4% | 74.1% |
| held-out, recall pooled over the 70 answering memories | 72.9% (51/70) | 65.7% (46/70) |
| held-out, other memories admitted (mean over 20 queries) | 6.3% | 5.8% |
| all 20 queries, same rule fitted on them: recall, mean over queries | 74.4% | 78.1% |
| all 20 queries, recall pooled | 72.9% (51/70) | 68.6% (48/70) |
| all 20 queries, other memories admitted | 6.3% (541/8570) | 6.2% (535/8570) |
| 50 same-language queries, among retrieved results: answering kept | 96.4% (108/112) | 89.3% (100/112) |
| same, other graded results admitted | 62.1% (630/1015) | 42.7% (433/1015) |

*What this says.* Under the rule "admit no more of the other memories than 0.82 does", **no
floor/margin pair in the grid keeps more answering memories than floor 0.82 alone**: the held-out
mean recall is the same to within 0.3 points, pooled recall is lower, and admission is about 0.5
points lower. The margin is worth having for what it does to the same-language queries (a
third fewer other results admitted, at a cost of 7 points of recall among retrieved results,
whose lexical matches pass either way). The recall the two-part rule can add is only available
with more admission: for example floor 0.79 with margin 0.04 keeps 75.7% (53/70, mean 85.9%) and
admits 10.3%; floor 0.79 without a margin keeps 91.4% and admits 27.9%. The two settings are
`config set search.relevance_floor` and `search.relevance_margin` (0 turns the margin off), so
the trade can be moved without a release. Real model, shared gold set (`real_search_quality`):
every gold pair is at or above 0.79, no unrelated pair is, the raw fallback fires for 6 of 6
unrelated queries, and 6 of the 10 queries that share no word with their memory are returned
(4 of 10 at floor 0.82), all in the top 3.

### Cost and latency

Jev requests 1447 (answered 1447, cache hits 0, retries 0, failures 0); input tokens 2734033; latency p50 335 ms, p95 664 ms; ledger total $0.1148; $0.0113 per 1,000 decisions (10199 decisions: 216 relation pairs, 216 kinds, 9767 graded pairs).
