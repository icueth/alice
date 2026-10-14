# Jev evaluation (M2, Task 10)

Measured on 2026-10-14 on `feat/m2-judge-settings`. **Single run**: every number below comes from one execution of the evaluation; nothing was repeated or averaged. The set is **synthetic**: it was written for this evaluation (by the engineer, not by users), holds no real user data, and its labels are one author's judgement of what a careful reader would choose.

| | |
|---|---|
| Machine | Apple M4 Pro (14 cores), 24 GB RAM, macOS 26.6.2 |
| Judge | TypeSafe Jev `jev-1.13.0` (`https://api.typesafe.ai/v1/systemone`), 4 parallel workers, at most 8 requests/s, $2 hard budget enforced by the judge |
| Embedder | `multilingual-e5-small-q8` (ONNX Runtime, local) |
| Heuristic | `HeuristicJudge::default()` (not tuned on this set) |
| Test | `crates/alice-daemon/tests/jev_eval.rs` (ignored), data in `tests/fixtures/judge_eval_pairs.jsonl` and `judge_eval_queries.jsonl` |

```
# estimate without key or network (requests are built and counted, never sent)
JEV_EVAL_DRY=1 ALICE_MODELS_DIR=$HOME/.alice/models cargo test -p alice-daemon --release --test jev_eval -- --ignored --nocapture
# the run
set -a; . ./.env; set +a
ALICE_MODELS_DIR=$HOME/.alice/models cargo test -p alice-daemon --release --test jev_eval -- --ignored --nocapture
unset TYPESAFE_API_KEY
```

The dry run estimated at most $0.086 (616 requests, reserved at 1.5 tokens per byte plus 300 per request); the ledger says $0.0222.

## The set

216 relation pairs, 54 per label, each label with 16 Thai, 16 English, 11 mixed Thai–English and 11 code pairs. Kinds of the new memory: fact 73, decision 29, status 24, howto 20, bugfix 15, preference 15, warning 14, note 7, summary 7, diary 6, handoff 6. Updates are always newer than what they replace; all 54 contradictions have two memories of the same age (time order cannot decide); about 25 unrelated pairs share a topic or words (hard negatives: two ports, two pages, two "how to" for the same tool); the duplicates are paraphrases written with little shared wording. Bodies are one or two sentences. 70 queries: 50 same-language, 12 cross-lingual (Thai for English and the reverse), 8 paraphrase-only (no shared content word); 9 of them have no answer in the corpus. The corpus for search is the 432 memories of the pairs. CI checks the composition and cleanliness of both files (`judge_eval_fixture.rs`).

The kind of the new memory is sent as `note` (the default) in the kind test, because the request states the memory's kind and the true one would give the answer away.


## Reading the results

- **Relation.** Jev 0.958 accuracy (95% interval 0.923–0.978) against 0.458 for the heuristic; macro-F1 0.958 against 0.369. The heuristic's blind spots are what the design expects: it finds 7% of the paraphrased duplicates (it needs high cosine) and none of the contradictions (it has no rule for them; 40 of 54 are called unrelated). Its updates (81%) and unrelated (94%) are fine. Jev's errors are 8 contradictions read as updates and 1 duplicate read as an update; it never calls a related pair unrelated. The contradicts/updates boundary is the fuzziest label in a hand-written set, even with equal ages.
- **Languages.** Jev: Thai 0.984, code 0.977, mixed 0.955, English 0.922 (44–64 pairs each; the differences are inside the noise of this size).
- **Calibration and auto-apply.** Jev Brier 0.085, ECE 0.053 (heuristic 1.101 and 0.569). At `auto_apply_threshold = 0.85`: 112 pairs applied, 69% of the actionable ones, precision **98%**, above the 95% target, so the setting can stay 0.85. 0.95 gives 100% precision at 49% coverage; 0.70 gives 98% at 81%. The heuristic's precision at 0.85 is 19%: without Jev, auto-apply on its output would be wrong four times in five (at 0.5 or 0.7 it applies 69 pairs, 70% right, 43% coverage).
- **Forward/reversed agreement.** 94.4% of pairs. When the two questions agree Jev is right 97.1% (198/204); when they disagree 75.0% (9/12). Disagreement is a usable "send to review" signal, but only 12 pairs disagreed.
- **Kind.** Jev 0.889 against 0.278 for the heuristic (0.885 against 0.258 without the 7 gold `note` memories). The heuristic's cues are English words; it says almost nothing for Thai text. Several kinds overlap (a status that is also a fact), so some Jev misses are label fuzziness.
- **Search.** The hybrid at the shipped floor (0.82) scores 0.734 nDCG@10 (Jev-graded); lexical only 0.585, vector only 0.677. **Vector only without a floor scores best, 0.763**, and the gap is larger on cross-lingual and paraphrase queries (0.680 against 0.578 for hybrid). Hybrid beats lexical-only by 0.15 and vector-only by 0.06. The lexical list alone cannot answer cross-lingual and paraphrase queries (0.407). On queries nothing answers, the best graded result is near zero for most (0.00–0.07), but the Thai Kubernetes query returned 10 results with a best grade of 0.38 and the English one 3 results: the floor does not stop all junk. Caveats: nDCG's ideal ranking is pooled from the four modes only; candidates are graded ten at a time in id order; the corpus is 432 memories of one synthetic domain.
- **Cost and latency.** 616 requests, 527,857 input tokens, **$0.0222**, $0.0111 per 1,000 decisions (2,005 decisions), latency p50 303 ms and p95 443 ms (4 parallel workers, no retries, no failures). The whole run is under 2% of the $2 budget.

### What the floor evidence says (input for Task 13)

No threshold or heuristic was tuned in this task; these are observations. The cosines of this embedder are compressed: even the pairs Jev calls unrelated sit at a median of 0.82. Jev's "answers" pairs have a median cosine of 0.868 (min 0.78); the other graded pairs 0.820 (min 0.72). At the shipped floor 0.82 a vector-only match keeps 87% of the answering pairs (cross-lingual 82%, paraphrase 64%); with a lexical match counted, 93% (cross 94%, para 64%). Lowering the floor to 0.80 keeps 96% / 98%, to 0.78 99%, to 0.75 everything, but a floor does little to separate: at 0.80 77% of the other graded pairs pass (those pairs are candidates the retrieval modes found, a harder set than random memories, so the real admission rate on an average memory is lower). Under the looser definition (also "about the subject") recall at 0.82 falls to 78% by cosine alone. Suggestions: a floor of 0.78–0.80 loses almost nothing relevant; a floor cannot do the filtering alone, so Task 13 may prefer a cut relative to the best score, or Jev's relevance grade where the project may use the cloud. Paraphrase-only queries are the weak spot (14 answering pairs: 64% at 0.82); with n = 14 this is a direction, not a measurement.

## Limits

Single run, one machine, one author's labels, 216 pairs and 70 queries on a 432-memory corpus; confidence intervals are given for relation accuracy only. Jev's answers are cached by request in `target/m2-jev-calls.jsonl` (no headers, no key) and the raw (query, candidate, cosine, grade) rows are in `target/m2-jev-floor.json`; neither is committed.

## Raw output of the run

Single run, 2026-10-14 11:20 UTC. Jev model jev-1.13.0, embedder multilingual-e5-small-q8 (432 passages embedded in 1.0 s).

### Relation

| Judge | Pairs | Accuracy (95% Wilson) | Macro-F1 | Brier | ECE | Auto-apply coverage at 0.85 | Auto-apply precision at 0.85 |
|---|---|---|---|---|---|---|---|
| heuristic | 216 | 0.458 (0.393–0.525) | 0.369 | 1.101 | 0.569 | 13% | 19% |
| jev-1.13.0 | 216 | 0.958 (0.923–0.978) | 0.958 | 0.085 | 0.053 | 69% | 98% |

Heuristic: rows are the gold label, columns the prediction

| gold \ predicted | duplicate | updates | contradicts | unrelated | recall |
|---|---|---|---|---|---|
| duplicate | 4 | 1 | 0 | 49 | 7% |
| updates | 3 | 44 | 0 | 7 | 81% |
| contradicts | 14 | 0 | 0 | 40 | 0% |
| unrelated | 0 | 3 | 0 | 51 | 94% |

Jev: rows are the gold label, columns the prediction

| gold \ predicted | duplicate | updates | contradicts | unrelated | recall |
|---|---|---|---|---|---|
| duplicate | 53 | 1 | 0 | 0 | 98% |
| updates | 0 | 54 | 0 | 0 | 100% |
| contradicts | 0 | 8 | 46 | 0 | 85% |
| unrelated | 0 | 0 | 0 | 54 | 100% |

| Language | Pairs | Heuristic accuracy | Jev accuracy |
|---|---|---|---|
| code | 44 | 0.432 | 0.977 |
| en | 64 | 0.500 | 0.922 |
| mixed | 44 | 0.386 | 0.955 |
| th | 64 | 0.484 | 0.984 |

Auto-apply sweep (non-`unrelated` predictions with P at or above the threshold; coverage is the share of the actionable pairs, precision the share right):

| Threshold | Heuristic applied / coverage / precision | Jev applied / coverage / precision |
|---|---|---|
| 0.50 | 69 / 43% / 70% | 162 / 100% / 94% |
| 0.70 | 69 / 43% / 70% | 131 / 81% / 98% |
| 0.85 | 21 / 13% / 19% | 112 / 69% / 98% |
| 0.90 | 21 / 13% / 19% | 96 / 59% / 99% |
| 0.95 | 21 / 13% / 19% | 80 / 49% / 100% |
| 0.99 | 1 / 1% / 0% | 47 / 29% / 100% |

Forward/reversed agreement (Jev): 94.4% of 216 pairs; accuracy when they agree 97.1% (198/204), when they disagree 75.0% (9/12)

### Kind classification

The memory is sent with the default kind (`note`) so the request does not state the answer.

Kind accuracy: heuristic 0.278, Jev 0.889 (216 memories); without the 7 gold `note` memories (the default the request states): heuristic 0.258, Jev 0.885

### Search, Jev-graded

Grade = 2 × P(answers) + P(partially); the ideal ranking is the best of every candidate any of the four modes returned (pooling).

| Recall mode | Queries | nDCG@10 (all) | nDCG@10 (answerable) | nDCG@10 (cross-lingual and paraphrase) |
|---|---|---|---|---|
| hybrid (floor 0.82) | 70 | 0.734 | 0.790 | 0.578 |
| lexical only | 70 | 0.585 | 0.645 | 0.407 |
| vector only (floor 0.82) | 70 | 0.677 | 0.728 | 0.486 |
| vector only, no floor | 70 | 0.763 | 0.807 | 0.680 |

Queries no memory answers: best Jev grade among the results of hybrid / vector only (floor 0.82) / all pooled candidates (result count in brackets); a grade near 0 is right.

| Query | Language | Hybrid | Vector only | Pool |
|---|---|---|---|---|
| วิธีตั้งค่า Kubernetes cluster | th | 0.38 (10) | 0.38 (10) | 0.38 (10) |
| สูตรทำต้มยำกุ้ง | th | 0.00 (0) | 0.00 (0) | 0.00 (10) |
| how to configure Kubernetes autoscaling | en | 0.01 (3) | 0.01 (2) | 0.20 (11) |
| best pizza recipe | en | 0.00 (0) | 0.00 (0) | 0.01 (10) |
| what is the weather in Paris | en | 0.00 (0) | 0.00 (0) | 0.00 (10) |
| วิธีทำ Kubernetes autoscaling | mixed | 0.07 (10) | 0.07 (10) | 0.07 (10) |
| ราคา Bitcoin วันนี้ | mixed | 0.01 (10) | 0.01 (9) | 0.01 (12) |
| tokio::select! cancellation safety | code | 0.02 (3) | 0.02 (1) | 0.02 (12) |
| BTreeMap range query | code | 0.02 (4) | 0.00 (2) | 0.02 (12) |

### Relevance floor evidence for Task 13

1573 graded (query, candidate) pairs: the candidates of the four recall modes plus the vector top 20 without a floor. 948 of them were returned by a recall mode. The cosine is the model's exact cosine; the raw rows are in `m2-jev-floor.json`. "Lexical" means the memory is in the lexical-only top 10 (a lexical match passes the floor at any similarity).

#### Relevant = Jev says the memory answers the query (P(answers) ≥ 0.5)

Cosine distribution (min / p10 / median / p90) of the real model's exact cosine between the query and the memory.

| Query kind | Relevant pairs | Relevant cosine | Other graded pairs | Other cosine |
|---|---|---|---|---|
| same | 116 | 0.780 / 0.825 / 0.886 / 0.923 | 1000 | 0.744 / 0.787 / 0.821 / 0.844 |
| cross | 34 | 0.791 / 0.813 / 0.844 / 0.884 | 239 | 0.724 / 0.785 / 0.819 / 0.841 |
| para | 14 | 0.797 / 0.802 / 0.833 / 0.862 | 170 | 0.746 / 0.782 / 0.813 / 0.829 |
| all | 164 | 0.780 / 0.816 / 0.868 / 0.915 | 1409 | 0.724 / 0.785 / 0.820 / 0.842 |

Floor sweep, query kind **same** (116 relevant, 1000 other graded pairs). "cosine" is what a vector-only match needs; "cosine or lexical" is the shipped rule (a lexical match passes at any similarity).

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (116/116) | 100% (116/116) | 100% (1000/1000) | 100% (1000/1000) |
| 0.75 | 100% (116/116) | 100% (116/116) | 100% (998/1000) | 100% (1000/1000) |
| 0.78 | 99% (115/116) | 99% (115/116) | 93% (926/1000) | 95% (951/1000) |
| 0.80 | 97% (113/116) | 98% (114/116) | 80% (797/1000) | 86% (863/1000) |
| 0.82 | 91% (106/116) | 96% (111/116) | 52% (518/1000) | 63% (630/1000) |

Floor sweep, query kind **cross** (34 relevant, 239 other graded pairs). "cosine" is what a vector-only match needs; "cosine or lexical" is the shipped rule (a lexical match passes at any similarity).

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (34/34) | 100% (34/34) | 100% (239/239) | 100% (239/239) |
| 0.75 | 100% (34/34) | 100% (34/34) | 98% (235/239) | 100% (239/239) |
| 0.78 | 100% (34/34) | 100% (34/34) | 92% (221/239) | 97% (231/239) |
| 0.80 | 94% (32/34) | 100% (34/34) | 77% (183/239) | 87% (207/239) |
| 0.82 | 82% (28/34) | 94% (32/34) | 47% (113/239) | 62% (149/239) |

Floor sweep, query kind **para** (14 relevant, 170 other graded pairs). "cosine" is what a vector-only match needs; "cosine or lexical" is the shipped rule (a lexical match passes at any similarity).

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (14/14) | 100% (14/14) | 100% (170/170) | 100% (170/170) |
| 0.75 | 100% (14/14) | 100% (14/14) | 99% (169/170) | 100% (170/170) |
| 0.78 | 100% (14/14) | 100% (14/14) | 93% (158/170) | 98% (167/170) |
| 0.80 | 93% (13/14) | 93% (13/14) | 65% (111/170) | 76% (130/170) |
| 0.82 | 64% (9/14) | 64% (9/14) | 36% (62/170) | 58% (98/170) |

Floor sweep, query kind **all** (164 relevant, 1409 other graded pairs). "cosine" is what a vector-only match needs; "cosine or lexical" is the shipped rule (a lexical match passes at any similarity).

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (164/164) | 100% (164/164) | 100% (1409/1409) | 100% (1409/1409) |
| 0.75 | 100% (164/164) | 100% (164/164) | 100% (1402/1409) | 100% (1409/1409) |
| 0.78 | 99% (163/164) | 99% (163/164) | 93% (1305/1409) | 96% (1349/1409) |
| 0.80 | 96% (158/164) | 98% (161/164) | 77% (1091/1409) | 85% (1200/1409) |
| 0.82 | 87% (143/164) | 93% (152/164) | 49% (693/1409) | 62% (877/1409) |

#### Relevant = Jev says the memory answers or is about the query's subject (P(answers) + P(partially) ≥ 0.5)

Cosine distribution (min / p10 / median / p90) of the real model's exact cosine between the query and the memory.

| Query kind | Relevant pairs | Relevant cosine | Other graded pairs | Other cosine |
|---|---|---|---|---|
| same | 232 | 0.772 / 0.803 / 0.850 / 0.909 | 884 | 0.744 / 0.785 / 0.820 / 0.841 |
| cross | 59 | 0.768 / 0.813 / 0.834 / 0.874 | 214 | 0.724 / 0.785 / 0.818 / 0.839 |
| para | 33 | 0.775 / 0.796 / 0.828 / 0.852 | 151 | 0.746 / 0.780 / 0.811 / 0.827 |
| all | 324 | 0.768 / 0.803 / 0.841 / 0.906 | 1249 | 0.724 / 0.784 / 0.818 / 0.840 |

Floor sweep, query kind **same** (232 relevant, 884 other graded pairs). "cosine" is what a vector-only match needs; "cosine or lexical" is the shipped rule (a lexical match passes at any similarity).

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (232/232) | 100% (232/232) | 100% (884/884) | 100% (884/884) |
| 0.75 | 100% (232/232) | 100% (232/232) | 100% (882/884) | 100% (884/884) |
| 0.78 | 98% (228/232) | 100% (231/232) | 92% (813/884) | 94% (835/884) |
| 0.80 | 94% (217/232) | 98% (227/232) | 78% (693/884) | 85% (750/884) |
| 0.82 | 81% (188/232) | 93% (215/232) | 49% (436/884) | 60% (526/884) |

Floor sweep, query kind **cross** (59 relevant, 214 other graded pairs). "cosine" is what a vector-only match needs; "cosine or lexical" is the shipped rule (a lexical match passes at any similarity).

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (59/59) | 100% (59/59) | 100% (214/214) | 100% (214/214) |
| 0.75 | 100% (59/59) | 100% (59/59) | 98% (210/214) | 100% (214/214) |
| 0.78 | 98% (58/59) | 100% (59/59) | 92% (197/214) | 96% (206/214) |
| 0.80 | 93% (55/59) | 100% (59/59) | 75% (160/214) | 85% (182/214) |
| 0.82 | 78% (46/59) | 92% (54/59) | 44% (95/214) | 59% (127/214) |

Floor sweep, query kind **para** (33 relevant, 151 other graded pairs). "cosine" is what a vector-only match needs; "cosine or lexical" is the shipped rule (a lexical match passes at any similarity).

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (33/33) | 100% (33/33) | 100% (151/151) | 100% (151/151) |
| 0.75 | 100% (33/33) | 100% (33/33) | 99% (150/151) | 100% (151/151) |
| 0.78 | 97% (32/33) | 100% (33/33) | 93% (140/151) | 98% (148/151) |
| 0.80 | 85% (28/33) | 91% (30/33) | 64% (96/151) | 75% (113/151) |
| 0.82 | 61% (20/33) | 73% (24/33) | 34% (51/151) | 55% (83/151) |

Floor sweep, query kind **all** (324 relevant, 1249 other graded pairs). "cosine" is what a vector-only match needs; "cosine or lexical" is the shipped rule (a lexical match passes at any similarity).

| Floor | Relevant kept (cosine) | Relevant kept (cosine or lexical) | Other admitted (cosine) | Other admitted (cosine or lexical) |
|---|---|---|---|---|
| 0.70 | 100% (324/324) | 100% (324/324) | 100% (1249/1249) | 100% (1249/1249) |
| 0.75 | 100% (324/324) | 100% (324/324) | 99% (1242/1249) | 100% (1249/1249) |
| 0.78 | 98% (318/324) | 100% (323/324) | 92% (1150/1249) | 95% (1189/1249) |
| 0.80 | 93% (300/324) | 98% (316/324) | 76% (949/1249) | 84% (1045/1249) |
| 0.82 | 78% (254/324) | 90% (293/324) | 47% (582/1249) | 59% (736/1249) |

### Cost and latency

Jev requests 616 (answered 616, cache hits 0, retries 0, failures 0); input tokens 527857; latency p50 303 ms, p95 443 ms; ledger total $0.0222; $0.0111 per 1,000 decisions (2005 decisions: 216 relation pairs, 216 kinds, 1573 graded pairs).
