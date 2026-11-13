# 07. Evaluation: does it beat plain RAG?

Two questions, kept apart:

1. **Speed**: how much faster is it, per keystroke and per context switch?
2. **Quality**: how much do stitched and repaired caches cost, compared with
   computing the same prompt in one pass?

Everything runs with the same model, the same retrieval and the same prompt
layout, so only the caching differs.

## Systems compared

| Name | What it does |
|------|--------------|
| **Full** | prefill the whole prompt for every request. The quality reference |
| **Prefix** | Full, plus reusing KV up to the first changed token (what llama.cpp and vLLM do by default) |
| **Naive reuse** | stitch cached chunk KVs with rotation, recompute nothing |
| **Boundary** | rotation, plus recompute of each chunk's first span (M3) |
| **Blend** | Boundary, plus deviation-picked spans to a 15% budget |
| **Blend + PIE** | Blend, with cached chunks repaired after edits instead of recomputed (M4) |
| **No context** | the file alone, no repo chunks. The floor that says whether retrieval helps |

Ablations: recompute budget (0, 5, 10, 15, 25%), span anchor spacing (32,
64, 128), repair span length (0, 16, 64, 256), bf16 vs 8-bit KV.

## Quality

**Static benchmarks** (each completion from a fresh prompt):

- RepoBench-C (Python, Java; cross-file-first, cross-file-random, in-file):
  next-line exact match and edit similarity. PIE and HEF report on it, so
  numbers compare.
- CrossCodeEval (Python, Java, TypeScript, C#), the cross-file split.
- RepoEval line completion.

**Agreement with Full**: for each prompt, the KL divergence of the next-token
distribution over the first 64 generated tokens, and whether the greedy
completion is identical. PIE reports KL under 0.0002 for its edits; this
is the most sensitive measure of what stitching costs.

**Edit benchmark, PIE's protocol**: in RepoBench-C contexts, insert, delete,
or change five lines, repair the cache, and complete. Compare with Full on
the edited text.

**Edit replays** (the one that matters most): real editing sessions
recorded in vmac (opt-in, local; every edit with its time, and the cursor),
replayed against each system. At each point where the replay is in insert
state and pauses, ask for a completion and compare it with what was typed
next. This measures the case no static benchmark covers: caches that have
been through hundreds of edits and context switches. Report quality against
staleness (edits since a chunk was last computed in one pass), to set the
recompute threshold in [04](04-cache-lifecycle.md).

Recording format: one JSON line per event (`open`, `edit`, `focus`, `save`,
mode changes), the same messages as the protocol, so the replay is just
feeding a log to an engine.

## Speed

On the edit replays, for each request:

- Time to first token and to first visible line, p50 / p95 / p99.
- Tokens computed per request (prefilled + recomputed) and tokens reused.
- Split by case: typing in place, context switch (prewarmed, cold), after a
  repair.
- Store behaviour: hit rate per tier, SSD bytes read per switch, memory use
  over a session.
- Energy: package power during a replay (`powermetrics`), since this runs on
  a laptop.

Machines: Yojan's Mac (the target), plus a Linux box with a GPU for the
vLLM variant if we build it.

## Dogfooding

Once it's usable (M3), vmac logs (locally, opt-in) each shown completion's
outcome (`outcome` in [03](03-protocol.md)): accepted all, partly, or
dismissed. Acceptance rate and accepted characters per hour, before and
after each change, are the end-to-end measures.

## What counts as success

- Context switches at least **3× faster** than Prefix at p50, on replays.
- Quality within **1 point of exact match** of Full on RepoBench-C and
  CrossCodeEval, and within 1 point on edit replays.
- Per-keystroke p50 at or under **150 ms** on Yojan's Mac.
- No growth in quality loss over a long replay (staleness stays bounded by
  the recompute threshold).

If Boundary meets these, deviation-based selection is skipped. If Blend
can't get within a point, the fallback is the fine-tune in
[01](01-model.md#fill-in-the-middle) trained on blended caches.
