# 06. Latency budget

## Targets

| Case | p50 | p95 |
|------|-----|-----|
| Keystroke → ghost text visible (typing in place) | ≤ 150 ms | ≤ 300 ms |
| Typing through a shown completion | 0 ms (no request) | |
| Entering insert state somewhere new, prewarmed | ≤ 200 ms | ≤ 400 ms |
| Same, cold (nothing cached) | ≤ 800 ms | ≤ 1.5 s |

Products put ghost text at under 200 ms from a GPU server; these targets are
for a laptop with no network.

## Assumed speeds (to measure in M0)

These are guesses for LFM3-1.7B at 4 bits under MLX on a recent M-series Mac.
M0 replaces them with measurements on Yojan's machine, and every number
below is redone.

- Prefill: ~2,000 tokens/s against a ~7K context.
- Decode: ~120 tokens/s.
- One forward pass for a handful of new tokens: ~10 ms.

## Context sizes (defaults)

| Part | Tokens |
|------|--------|
| Repo chunks | 4,096 |
| Suffix (after the cursor) | 512 |
| Prefix (before the cursor) | 2,048 |
| **Total** | **~6,700** |

## Per keystroke

| Step | Time |
|------|------|
| Debounce (Lua; adaptive: shorter after a pause, 0 after `.` or `(`) | 40 ms |
| Edit and `complete` to the engine, over a pipe | < 1 ms |
| Append the typed tokens to the live tail (exact) | ~10 ms |
| Decode the first line, ~10 tokens | ~85 ms |
| vmac drains the reply (30 ms timer, so ~15 ms on average) | ~15 ms |
| Frame to window, draw | ~5 ms |
| **Total** | **~155 ms** |

Decoding is the biggest part. Ways to cut it, in order of effort:

- Show the first line as soon as it's done (`partial`), keep decoding the
  block behind it.
- Stop early: at the end of the line for `stop: "line"`, when indentation
  returns to the block's level for `"block"`.
- Speculative decoding using the previous completion as the draft: if you
  typed what it predicted, its remaining tokens are checked in one pass.
- A draft model, if a smaller LFM3 or a pruned one appears.

## Per context switch

Building a new sequence when the cursor moves or retrieval changes:

| | Plain RAG | Plain RAG + prefix cache | This design (warm) |
|---|---|---|---|
| What's computed | all ~6,700 tokens | everything after the first changed token; for a new spot that's usually almost all | rotations (≈ free), first spans of ~16 chunks (~500 tokens), 15% of the rest (~900), the live tail (~150) |
| Estimated time | ~3.3 s | ~1–3.3 s | ~0.8 s |
| Loading caches from SSD | | | ~20–40 ms if cold, overlapped with recompute |

And the `focus` hint starts this when you enter insert state or rest the
cursor, so in most cases it's done before the first key.

## Where the win is, honestly

With SPM layout and stable retrieval, **plain prefix caching already makes
the per-keystroke case cheap**: every key is an append, for any engine that
keeps its KV between requests (llama.cpp and vLLM both do). Chunk reuse
doesn't speed up typing in place.

What it speeds up is everything else: moving to another function, opening
another file, retrieval swapping a chunk, editing above the cursor, coming
back to a file later. Prefix caching throws away everything after the first
changed token; this design keeps every chunk that didn't change, wherever it
moves. That's why the evaluation ([07](07-evaluation.md)) reports the two
cases apart, and replays real editing sessions, where those switches happen
all the time.

## Cancellation

A new key cancels the completion in flight. The engine checks between
decode steps (~8 ms), so a cancelled request frees the model quickly.
Context switch work that's cancelled is kept: spans already recomputed stay
recomputed.
