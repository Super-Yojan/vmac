# 05. Storage tiers

## What a chunk costs

For LFM3-1.7B, in bf16 ([01](01-model.md) has the shapes):

| Part | Per token |
|------|-----------|
| Global attention K and V (6 layers × 2 KiB) | 12 KiB |
| Sliding attention K and V (5 layers × 2 KiB) | 10 KiB |
| Conv inputs at anchors (6 tokens × 13 layers × 4 KiB, every 64 tokens) | ~4.9 KiB |
| **Total** | **~27 KiB** |

With K and V stored in 8 bits: ~16 KiB per token. A 300-token function is
about 8 MiB in bf16, 5 MiB in 8 bits. Whether 8-bit KV costs quality is
measured in M5 ([07](07-evaluation.md)); until then, bf16.

For scale: vmac itself is about 37,000 lines, maybe 400K tokens, so caching
every chunk of it would be around 10 GiB in bf16. We don't cache everything;
we cache what retrieval uses, and prewarm what it's likely to use.

## The tiers

On a Mac, the GPU and CPU share memory, so "GPU memory" and "RAM" are the
same chips. What differs is whether a cache is already arrays the model can
read, or bytes on the SSD.

| Tier | What | Default size | Latency to use |
|------|------|--------------|----------------|
| Live | the current sequence's KV and conv checkpoints | ~200 MiB (≈7K tokens) | none |
| Warm | chunk caches as arrays in memory, LRU | 1 GiB (≈38K tokens) | none (a view into it) |
| SSD | chunk files in `~/.cache/vmac/kv/` | 8 GiB (≈300K tokens) | read at SSD speed, a few GB/s |

Loading a full repo-chunk budget (4,096 tokens, ~108 MiB) cold from the SSD
takes on the order of 20–40 ms on recent Macs, and less from the OS page
cache. That's a context switch cost, and prewarming hides most of it.

CacheBlend's pipelining applies: load layer *l + 1* while recomputing spans
at layer *l*. The paper's rule picks the recompute ratio so that recompute
time roughly matches load time, never below 15%. On a Mac, compute and SSD
reads don't compete for the same resource, so the same rule holds; M5
measures both.

## Files on the SSD

```
~/.cache/vmac/kv/
  LFM3-1.7B-4bit/            # one folder per model and quantization
    3f/3fa9c2….kv            # one file per chunk, named by its hash
    index.db                 # hash → size, last used, uses, staleness
```

A `.kv` file is a small header (format version, model, token ids, anchors,
staleness, checksum) followed by one contiguous array per layer, in the
layout the model runner wants, so loading is a memory map and no copying.
Files are written to a temporary name and renamed, so a crash leaves no
half-written cache.

The folder is created with owner-only permissions. A KV cache is a
transformed copy of your source code: it never leaves the machine, and
`:autocomplete-clear-cache` deletes it.

## What gets written

Not every chunk that's computed deserves the SSD:

- The cursor's chunk changes every key; it lives only in the live tier.
- A chunk is written once it's been used in two sequences, or was
  prewarmed on purpose.
- Writes happen in the background after the completion is shown.

## Eviction

- Warm → SSD: least recently used first, weighted by size (a big chunk is
  more costly to recompute, so it's kept a little longer).
- SSD → gone: least recently used first, but chunks whose hash no longer
  matches any chunk in the project (the text was edited away, the file was
  deleted, another branch) go before anything else.
- A model or quantization change makes the old folder dead; it's deleted
  after a week unused.

## Prewarming

The engine computes chunk caches ahead of need, at low priority, and stops
the moment a completion is asked for:

1. On `focus`: the chunks retrieval would pick for that spot.
2. On project open: chunks of the open buffers and of files changed in the
   last few commits, then definitions those files use.
3. When idle for a while: the rest of the files you've touched this week.

Prewarming uses the GPU and the battery. By default it runs steps 2 and 3
only on AC power; step 1 always runs, since it's what keeps the first
completion fast.
