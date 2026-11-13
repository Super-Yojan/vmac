# Ghost-text autocomplete with cached KV

Status: draft for review. A prototype of M0 and M1 is in [engine/](../../../engine/) and `src/complete.rs`.

vmac gets inline (ghost text) completions from a small local model, Liquid's
LFM3-1.7B, with the whole repository as context, at keystroke speed on a
laptop. The trick is never to prefill the same code twice: every piece of
code the model reads is a chunk whose KV cache is kept (in memory, then on
the SSD), stitched into each prompt CacheBlend-style, and repaired PIE-style
when you edit it, instead of being recomputed.

Nobody ships this today. Cursor and others do plain RAG: retrieved code is
pasted into the prompt and prefilled again on every request, which is why
they use small models and short contexts on a GPU server. On a Mac, prefill
is the expensive part, so reusing KV is what makes repo context affordable
locally at all.

## The documents

| Doc | What it covers |
|-----|----------------|
| [01-model.md](01-model.md) | LFM3-1.7B: its tri-hybrid layers, what they mean for KV reuse, FIM |
| [02-architecture.md](02-architecture.md) | The pieces: vmac, the `vmac-complete` engine, the chunk store; where each lives in the code |
| [03-protocol.md](03-protocol.md) | What vmac and the engine say to each other |
| [04-cache-lifecycle.md](04-cache-lifecycle.md) | Chunks, the prompt layout, blending, and keeping caches right while you edit |
| [05-storage.md](05-storage.md) | GPU/unified memory, RAM and SSD tiers, sizes, eviction, prewarming |
| [06-latency.md](06-latency.md) | The latency budget, per keystroke and per context switch |
| [07-evaluation.md](07-evaluation.md) | How we know it beats plain RAG: baselines, benchmarks, edit replays |
| [08-milestones.md](08-milestones.md) | Phases, each with something that ships and a go/no-go |
| [m0-results.md](m0-results.md) | M0 measured on a Mac: 12 ms per keystroke with the cache kept, rotation exact, FIM not usable as released |

## Decisions this draft makes

1. **Model: LFM3-1.7B**, quantized, running on the machine the vmac server
   runs on (normally your Mac). See [01-model.md](01-model.md).
2. **Engine: a separate process, `vmac-complete`**, spoken to over stdio
   like a language server. First version in Python on MLX, installed by
   vmac's package manager the way it installs pip language servers. The
   protocol stays fixed so a Rust engine, or a vLLM + LMCache server on a
   GPU box, can replace it.
3. **Our own blending, not LMCache's.** LMCache's blending assumes every
   layer is attention; 13 of LFM3's 24 layers are short convolutions with no
   KV cache. We implement blending for the hybrid ourselves (anchored spans,
   [04](04-cache-lifecycle.md)). LMCache + vLLM stays the backend for a
   remote GPU, if we add one.
4. **Suffix-before-prefix prompt layout (SPM)**, so typing at the cursor is
   always an append to the cached sequence, which is exact and cheap.
   Blending is only paid when the context changes (you move to another
   function, retrieval picks other chunks), and that is prewarmed off the
   keystroke path.
5. **Retrieval without an embedding model at first**: LSP definitions near
   the cursor, imports, recently edited buffers, and identifier overlap.
6. **FIM likely needs a fine-tune.** LFM3 is released as a chat model and
   its card doesn't mention fill-in-the-middle. Milestone M0 checks it;
   M2 is a LoRA fine-tune on LFM3-1.7B-Base if it falls short.

## Open questions for Yojan

- Is LFM3-1.7B (text) the model you meant by "Liquid model 3"? This draft
  assumes it.
- Local only (Mac, MLX), or should a remote GPU server be a first-class
  target too? The draft makes local first and remote optional.
- Are you OK fine-tuning a FIM variant (a single-GPU LoRA run), if M0 shows
  the released model can't fill in the middle well?
- Can vmac record your editing (opt-in, local only) to build the edit-replay
  benchmark in [07](07-evaluation.md)?

## Sources

- CacheBlend (EuroSys 2025 best paper): https://arxiv.org/abs/2405.16444
- LMCache blending: https://docs.lmcache.ai/kv_cache_optimizations/blending.html
- PIE, ICLR 2025: https://proceedings.iclr.cc/paper_files/paper/2025/file/9530635032b95cea9585bd800d308300-Paper-Conference.pdf
- HEF (2026): https://arxiv.org/abs/2603.06593
- CodeComp (2026): https://arxiv.org/abs/2604.10235
- LFM3-1.7B model card: https://huggingface.co/LiquidAI/LFM3-1.7B
