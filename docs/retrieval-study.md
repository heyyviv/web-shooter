# Retrieval study (2026-11-02)

Which retrieval settings serve web-shooter's `fetch` + query best? This study covers lexical variants of BM25, fusion with the static embeddings, chunk size and neighbour context. Dense model choice and rerankers were left out by choice; the earlier comparisons are in `CLAUDE.md`.

## Setup

- **Test set.** The 32 existing cases, plus 40 new ones written for this study on 16 pages that weren't in the set: MDN, Python, Rust book, PostgreSQL, SQLite, Kubernetes, Effective Go, RFC 6749 (plain text), a GitHub README, Martin Fowler, GOV.UK, NHS, two Wikipedia articles and two arXiv PDFs (word2vec, Adam). The new cases include fact lookups, paraphrased questions, table cells and three multi-page cases with distractor pages. Answers were picked from the page text before any ranking was run. After the first baseline, three expected strings were loosened because the right passage was returned but phrased differently ("330 metres" vs "330 m"). Nothing else was changed. Stack Overflow was dropped because it serves a Cloudflare check.
- **Held-out.** `eval/heldout.json` (15 cases) was run only on the finalists, never during the sweeps.
- **Metrics.** found (answer anywhere in the reply), top (in the best-scored passage), MRR (1 / rank of the first returned passage holding the answer), tokens (reply size). The ranking time is under 10 ms per call in every setting, so it is left out.
- **How to reproduce.** Branch `retrieval-study`. `src/exp.rs` reads switches from `WS_EXP`, and an unset switch keeps the shipped behaviour. `eval --summary` prints one line, with the first 32 cases and the 40 new ones shown separately.

  ```sh
  WS_EXP="bw=0.5" cargo run --release --example eval -- --summary [--heldout] [--bm25]
  ```

  Switches: `k1`, `b`, `delta` (BM25+), `hw` (heading terms counted n times, BM25F-style), `stem=0`, `prox`/`proxw` (bonus when consecutive query terms appear within a window), `prf`/`prfk`/`prfw` (RM3-style expansion: n terms from the top k chunks at weight w), `fu` (0 weighted, 1 RRF, 2 z-score, 3 rank-percentile), `bw` (BM25 weight), `rrfk`, `dh=0` (embed the body without the heading path), `tb`/`mb` (chunk target and minimum bytes), `nb` (add nb × the better neighbour's score), `cut` (the relative cut-off).

**Noise.** On 72 cases a difference of 2–3 in top is within noise. The paired counts below are the better guide.

## Baseline

| Setting | found | top | MRR | tokens |
|---|---|---|---|---|
| shipped (hybrid, BM25 weight 0.3) | 66/72 | 48/72 | 0.759 | 1,109 |
| BM25 only | 60/72 | 45/72 | 0.716 | 837 |

On the first 32 cases hybrid ranking is clearly better (top 24 vs 22, found 30 vs 25). On the 40 new cases BM25 alone is about as good (top 23 vs 24, MRR 0.713 vs 0.706). The new cases are mostly fact lookups with words taken from the page, and those don't need embeddings.

## Lexical variants (with embeddings)

| Setting | found | top | MRR | tokens |
|---|---|---|---|---|
| shipped (k1 1.2, b 0.75) | 66 | 48 | 0.759 | 1,109 |
| k1 0.9, b 0.4 | 67 | 50 | 0.779 | 1,112 |
| k1 1.2, b 0.3 | 67 | 49 | 0.767 | 1,116 |
| k1 1.5–2.0 | 66 | 45–46 | 0.75 | 1,120 |
| BM25+ δ 0.5 | 66 | 50 | 0.770 | 1,119 |
| headings ×2 / ×3 | 66 | 48 | 0.744–0.753 | 1,110 |
| no stemming | **62** | 47 | 0.731 | 1,097 |
| proximity bonus 0.5–2 | 65–66 | 46–47 | 0.745–0.749 | 1,045–1,091 |
| RM3 expansion (5–20 terms) | 62–65 | 44–48 | 0.72–0.76 | 1,013–1,084 |

- **Stemming matters.** Without it, 4 answers are lost.
- **Less length normalisation helps a little** (lower b or k1, or BM25+), about +1–2 top. That is within noise, and BM25 alone does no better with it.
- **Heading weight, proximity and RM3 don't help.** RM3 is the worst on BM25 alone (top 39, found 56): with chunks this small, the top three chunks are a weak feedback set and add off-topic terms.

## Fusion

| Setting | found | top | MRR | tokens |
|---|---|---|---|---|
| weighted, BM25 0 (embeddings only) | 57 | 37 | 0.621 | 1,333 |
| weighted, BM25 0.2 | 64 | 43 | 0.705 | 1,199 |
| **weighted, BM25 0.3 (shipped)** | 66 | 48 | 0.759 | 1,109 |
| weighted, BM25 0.4 | 66 | 48 | 0.767 | 1,032 |
| **weighted, BM25 0.5** | 66 | **51** | **0.778** | **972** |
| weighted, BM25 0.7 | 63 | 47 | 0.745 | 902 |
| RRF k 60 | **68** | 45 | 0.740 | 1,840 |
| RRF k 10 | 66 | 44 | 0.722 | 1,151 |
| z-score, BM25 0.5 | 63 | 45 | 0.723 | 719 |
| rank-percentile, BM25 0.5 | 66 | 45 | 0.729 | 1,694 |
| embed body without headings | 66 | 46 | 0.731 | 1,168 |

- The 0.3 BM25 weight was tuned on the first 15 cases with `potion-retrieval-32M`. With `potion-base-8M`, the model now shipped, 0.5 does better and returns 12% fewer tokens.
- RRF finds the most answers only because it returns 66% more tokens; its flat scores defeat the cut-off, as found before.
- z-score returns the fewest tokens but loses 3 answers.
- Embedding the heading path with the passage is worth keeping.

## Chunking and neighbour context

| Setting | found | top | MRR | tokens |
|---|---|---|---|---|
| ~100 tokens (400 B) | 62 | 40 | 0.681 | 1,021 |
| ~150 tokens (600 B) | 64 | 43 | 0.693 | 1,037 |
| **~200 tokens (800 B, shipped)** | 66 | 48 | 0.759 | 1,109 |
| ~300 tokens (1,200 B) | 64 | 46 | 0.736 | 1,138 |
| ~400 tokens (1,600 B) | 63 | 49 | 0.756 | 1,215 |
| no small-tail merge / larger merge | 66 | 47 | 0.75 | 1,110 |
| neighbour +0.2 | 67 | 51 | 0.786 | 1,214 |
| neighbour +0.3 | 66 | 53 | 0.791 | 1,272 |
| neighbour +0.5 | 65 | 53 | 0.804 | 1,341 |

- The shipped 200 tokens finds the most answers. Bigger chunks raise top only because each passage holds more text (an easier target), and they cost more tokens.
- Neighbour smoothing (a chunk also scores part of its neighbour's score) helps top: the answer often sits one chunk after the passage that matches the question, as in the infobox under the Eiffel Tower's lead. It raises tokens because it flattens scores.

## Combinations and held-out

| Setting | dev found | dev top | dev MRR | dev tokens | held-out found | held-out top | held-out MRR | held-out tokens |
|---|---|---|---|---|---|---|---|---|
| shipped | 66/72 | 48 | 0.759 | 1,109 | 14/15 | 12 | 0.856 | 1,384 |
| **BM25 0.5** | 66/72 | 51 | 0.778 | 972 | 14/15 | **12** | 0.844 | **1,086** |
| BM25 0.5, neighbour 0.2 | 66/72 | 53 | 0.791 | 1,074 | 14/15 | 11 | 0.811 | 1,175 |
| BM25 0.5, neighbour 0.3, cut 0.75 | 65/72 | 54 | 0.800 | 963 | 14/15 | 11 | 0.800 | 1,023 |
| neighbour 0.3 alone | 66/72 | 53 | 0.791 | 1,272 | 14/15 | 11 | 0.833 | 1,544 |

Paired against shipped, on the dev set (cases whose answer moved into or out of the top passage):

- **BM25 0.5:** gained 6 (`react-existing-page`, `pdf-attention-cost`, `pdf-bert-no-nsp`, `w2v-cbow-cost`, `adam-defaults`, `multi-wage-18`), lost 3 (`para-leave-field`, `para-motivation`, `pdf-nist-csf-functions`).
- **+ neighbour 0.3, cut 0.75:** gained 9, lost the same 3.

Neither is statistically significant (sign test p ≈ 0.5 and ≈ 0.15). The pattern is consistent, though. A heavier BM25 weight wins on table cells and exact values and loses on paraphrased questions (`para-*`), which need the embeddings.

## Conclusions

1. **Raise the BM25 weight from 0.3 to 0.5.** On the dev set it gives 3 more answers on top and 12% fewer tokens; on held-out it holds top at 12/15 with 22% fewer tokens. It costs nothing in found. This is the only change I'd recommend shipping now.
2. **Neighbour smoothing is promising but unproven.** It is +3 top on dev and −1 on held-out. Keep it on the list until the held-out set is bigger.
3. **Keep:** stemming, the 200-token chunks, embedding the heading path, and weighted min-max fusion over RRF, z-score or rank fusion.
4. **Drop:** RM3 expansion, the proximity bonus and heading up-weighting. None of them helped.
5. **Next idea, untested:** set the BM25 weight per query. The cases that lose with more BM25 are paraphrases (few query words appear on the page), and the ones that win are exact lookups (numbers, table labels). The share of query terms found in the top chunk is a cheap signal for which kind a query is.

What these numbers don't show: everything is single-run on cached snapshots. The new cases were written by the same person (Claude) who ran the study, and they lean toward fact lookups. The held-out set is only 15 cases.
