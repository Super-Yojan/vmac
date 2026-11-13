# 01. The model: LFM3-1.7B

## What it is

From the model card and `config.json` (checked 2026-11-13):

| | |
|---|---|
| Parameters | 1.73B |
| Layers | 24: 13 gated short convolution, 5 sliding-window attention, 6 global attention |
| Layer order | repeats conv, conv, attention, conv; global attention at layers 3, 7, 11, 15, 19, 23 (1-based), sliding at 5, 9, 13, 17, 21 |
| Attention | 16 query heads, 4 KV heads (GQA), head dim 128 |
| Sliding window | 256 tokens |
| RoPE | global layers: theta 1e6, rotary on half of each head (`partial_rotary_factor` 0.5); sliding layers: theta 1e4 |
| Convolution | kernel 7 (`conv_L_cache`), so each conv layer sees a token and the 6 before it |
| Context | 256K tokens |
| Vocabulary | 128K, ChatML-style chat markers |
| Runtimes listed | MLX (4-bit), llama.cpp (GGUF), vLLM, SGLang, Transformers, ONNX |
| License | LFM Open License v1.0 |

The card says it isn't meant for agentic coding on big codebases. That's
fine: ghost text is short next-line or next-block prediction, and the
repository context is what we supply.

## What the hybrid layers mean for caching

Most of the papers assume a plain transformer, where every layer keeps K and
V for every token, and reusing a chunk means reusing those. LFM3 is
different in three ways, and the design follows from them.

**1. Only the 6 global layers carry long-range information.** The sliding
layers see 256 tokens back; the conv layers see 6. So for a retrieved chunk
sitting thousands of tokens before the cursor, the only thing the cursor's
tokens read from it is its K and V in the 6 global layers. That makes chunk
caches small: 2 KiB per token per attention layer (K and V, 4 heads × 128
dims × bf16), so 12 KiB per token for the global layers. A 300-token
function is 3.6 MiB. (The card's figure of 1.6 GB for a 128K context agrees:
128K × 12 KiB = 1.5 GiB.)

**2. Conv layers have no per-token cache.** A conv layer's output for a token
depends on that layer's input for the token and the 6 before it. CacheBlend
recomputes a scattered set of "high deviation" tokens per layer, which in a
transformer only needs the cached K and V of the others. In LFM3, recomputing
token *t* at a conv layer also needs the layer inputs of *t−6 … t−1*, which a
KV cache doesn't hold. Storing every token's conv inputs would cost 13 layers
× 2048 × 2 bytes = 52 KiB per token, four times the KV. So we recompute
**contiguous spans that start at anchors**, and store the conv inputs only
for the 6 tokens before each anchor. Details in
[04-cache-lifecycle.md](04-cache-lifecycle.md#blending-on-a-hybrid-model).

**3. Positions only live in the attention layers.** Conv layers have no
position encoding, so moving a chunk to a new place in the prompt is the
same RoPE re-rotation CacheBlend and PIE do, applied to the rotated half of
the keys in the 11 attention layers, with each layer's own theta.

None of the cited papers test a hybrid like this. Whether 15% recompute
still holds quality, and whether PIE's "leave later tokens stale" still holds
when conv layers mix neighbours, are the main research risks. Both are
measured in M3 and M4 ([08](08-milestones.md)).

## Fill-in-the-middle

Autocomplete needs the text after the cursor too. Coding models are trained
with FIM tokens (prefix, suffix, middle); LFM3's card doesn't mention them.
Options, in order of preference:

1. **The released model already does FIM** with some prompt. M0 checks with
   a small set (a few hundred held-out functions) against a coding model of
   similar size that is FIM-trained.
2. **A LoRA fine-tune of LFM3-1.7B-Base for FIM**, in SPM order (suffix
   first, then prefix; see [04](04-cache-lifecycle.md#the-prompt-layout)).
   Liquid says the family fine-tunes on one GPU. Training data: permissively
   licensed code, FIM-split at random spans, with retrieved cross-file
   context in the prompt (the RepoBench / CrossCodeEval style), so the model
   learns to use the chunks we give it.
3. **Prefix-only completion** (no suffix) as a fallback. It's worse in the
   middle of a file but works with the released model as it is.

While fine-tuning, also train with blended caches (the model sees KV that
was stitched rather than computed in one pass). That's cheap to add and is
the main lever if blending costs too much quality on this architecture.

## Why not a bigger model

A larger coding model on a GPU server would complete better. But the point
of this design is that it runs on the laptop, next to the files, with no
server and no network. LFM3-1.7B at 4 bits is about 1 GB, and its small KV
(only 6 global layers) is what makes caching a whole repo on an SSD
practical. The protocol in [03](03-protocol.md) doesn't depend on the model,
so a bigger one can be swapped in on a remote machine later.
