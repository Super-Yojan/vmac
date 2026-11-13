# M0 results: LFM3-1.7B on Yojan's Mac

Run 2026-11-12 with `vmac-complete bench` (engine/), model
`LiquidAI/LFM3-1.7B-MLX-4bit`, on an Apple silicon MacBook Pro (arm64,
Python 3.14, MLX through mlx-vlm 0.7.12). Code: commits d5a70d8 (speed,
typing, rotation) and 3e81530 (fill in the middle). Source text: the vmac
repository.

## Speed

| context | prefill ms | prefill tok/s | decode tok/s | +2 tokens ms | −40 tokens ms (tokens fed) |
|---|---|---|---|---|---|
| 2,048 | 1,829 | 1,120 | 94 | 108* | 252 (24) |
| 7,000 | 5,565 | 1,258 | 82 | 16 | 183 (16) |
| 16,000 | 13,079 | 1,223 | 91 | 82 | 54 (24) |

\* The first append after the warm-up probably includes compiling kernels.

Prefill runs at about 1,200 tokens/s and decoding at about 90 tokens/s, both
under the guesses in [06-latency.md](06-latency.md) (2,000 and 120). A
plain-RAG prompt of 6,700 tokens would take about 5.5 s to prefill on this
machine, so per-request prefill is out of the question and context switches
need prewarming even more than planned.

## Typing (the M1 claim)

275 keystrokes in real code, a completion asked after each (time to first
token):

| | p50 | p95 | tokens prefilled, mean |
|---|---|---|---|
| cache kept between keys | **12 ms** | **42 ms** | 34 |
| prompt computed from nothing | 1,047 ms | 1,513 ms | 1,317 |

About 87× faster at the median. Typing in place is solved by keeping the
cache, as [06](06-latency.md#where-the-win-is-honestly) predicted.

## Rotation (CacheBlend's premise)

A 128-token chunk computed at position 0 with its keys rotated by 3,000,
against computing it at 3,000. Relative max error:

| layers | keys, rotated | keys, not rotated | values (never rotated) |
|---|---|---|---|
| global attention (6) | 0.7–1.9% | 16–141% | 0–2% |
| sliding attention (5) | 1.2–1.8% | 136–170% | 1.1–2.2% |

Rotated keys are as close as the values are, which differ only by
floating-point noise in a 4-bit model at a different position, so rotation
moves cached chunks as exactly as recomputing would. (With a full-precision
tiny LFM3 in the engine's tests the error is 0.) Blending can rely on it.

## Fill in the middle

200 lines hidden in vmac's code, the model asked for each back (one line):

| layout | exact match | edit similarity |
|---|---|---|
| prefix only | 8.5% | 45% |
| SPM with `<\|fim_suf\|>` / `<\|fim_pre\|>` / `<\|fim_mid\|>` | 3.5% | 26% |
| PSM with the same tokens | 0.0% | 19% |

LFM3's vocabulary has FIM tokens, but the released model doesn't use them:
with them it writes comments, the file's path, or code from elsewhere in the
file. Prefix-only works, but with no repo context it's weak (it often writes
something plausible but different).

## What this means

- **M1 holds**: ghost text while typing costs tens of milliseconds.
- **Blending's premise holds** on LFM3's hybrid layers: rotated chunks are
  as good as recomputed ones.
- **FIM needs work.** The released LFM3 can't fill in the middle. Options:
  compare a FIM-trained coding model of similar size under the same engine
  (to know what LFM3 is giving up), fine-tune LFM3 for FIM (M2), or stay
  prefix-only and see how much repo context (M3) lifts it.

## Comparison: Qwen2.5-Coder-1.5B (FIM-trained)

The same bench (same 200 lines, same seed) with
`mlx-community/Qwen2.5-Coder-1.5B-4bit`, a coding model of similar size
trained for FIM in PSM order:

| | LFM3-1.7B | Qwen2.5-Coder-1.5B |
|---|---|---|
| prefill tok/s (7K context) | 1,258 | 1,337 |
| decode tok/s | 82–94 | 111–137 |
| typing, cache kept, p50 / p95 | 12 / 42 ms | 9 / 33 ms |
| FIM exact match: prefix only | 8.5% | 11.0% |
| FIM exact match: SPM | 3.5% | 2.5% |
| FIM exact match: PSM | 0.0% | **13.5%** |
| FIM edit similarity, best layout | 45% (prefix) | 50% (PSM) |

What it shows:

- **The gap is about 5 points of exact match** (8.5% against 13.5%) on this
  test, with no repo context. Both models are weak here because the test is
  hard (any line, exact match, nothing from other files); the gap matters
  more than the level.
- **SPM is bad on both.** Qwen was trained on PSM only. The design's
  suffix-first layout, which makes every keystroke an append, needs a model
  trained in that order. With PSM, every key changes the prefix, which sits
  before the suffix, so the suffix's tokens are fed again on every key. A
  short suffix (the rest of the line and a few lines, ~64 tokens) keeps that
  to about 50 ms per key at these speeds.
- Speed is close; Qwen decodes about 40% faster.

So a FIM fine-tune of LFM3 (M2) would buy two things at once: fill in the
middle, and SPM order, which keeps typing an append. Until then, the choice
is LFM3 prefix-only or a PSM model with a short suffix.
