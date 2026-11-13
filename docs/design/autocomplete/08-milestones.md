# 08. Milestones

Each phase ends with something usable and a decision. Sizes are rough, for
one person.

## M0. Feasibility (about a week)

- Run LFM3-1.7B (4-bit, MLX) on Yojan's Mac: measure prefill and decode
  speed at 2K, 7K and 16K contexts. Redo the numbers in
  [06](06-latency.md).
- FIM check: a few hundred held-out functions, with the released model's
  best prompt, against a FIM-trained coding model of similar size.
- Write the LFM3 forward pass with explicit KV and conv state; check it
  matches MLX's own output token for token.
- Check rotation: compute a chunk at position 0, rotate to position 3,000,
  and compare its keys with computing it there (should match to float
  error).

**Decide:** is FIM good enough as released (skip M2)? Are the speeds in range
for the targets, or do the budgets shrink?

## M1. Ghost text, no repo context (about two weeks)

- vmac: edit log in `buffer.rs`, `src/complete.rs`, ghost text in the
  frame and `shell.rs`, Lua keys and config, `:packages` entry.
- Engine: the protocol, document mirror, SPM prompt with the current file
  only, the live tail kept between requests (plain prefix reuse).
- Behind `vmac.set_autocomplete({ enabled = true })`, off by default.

**Ships:** usable local ghost text in vmac. **Decide:** is the UI right (keys,
when it shows, flicker)?

## M2. FIM fine-tune, if M0 said so (one to two weeks)

- LoRA on LFM3-1.7B-Base: SPM order, random FIM spans, cross-file context
  in the prompt. Part of the training with stitched caches.
- Evaluate on RepoBench-C and CrossCodeEval against the released model.

**Ships:** a FIM adapter, installed with the model.

## M3. Chunks and blending (two to three weeks)

- Tree-sitter chunker in the engine, chunk hashing, the warm (memory) tier.
- Retrieval: imports, recent buffers, identifier overlap, then LSP hints
  through `focus`.
- Context switches with rotation and boundary recompute; `focus` prewarming.
- The evaluation harness: static benchmarks, Full / Prefix / Naive /
  Boundary.

**Decide:** does Boundary meet the quality bar in [07](07-evaluation.md)? If
not, add deviation-based spans (Blend), and measure again.

## M4. Edit repair (about two weeks)

- Token diffs of edited chunks, PIE rotation, repair spans, staleness
  counts, idle recompute.
- Edit recording in vmac (opt-in) and the replay harness.
- PIE's edit benchmark, and the first edit replays.

**Decide:** repair span length and the staleness threshold, from the replays.

## M5. SSD tier and prewarming (one to two weeks)

- `.kv` files, index, background writes, eviction, layer-wise loading
  overlapped with recompute.
- Prewarming on project open, AC-power rules, `:autocomplete-clear-cache`.
- 8-bit KV, measured.

**Ships:** completions with repo context that stay fast across restarts and
large projects. Turned on by default if the numbers hold.

## M6. Report (about a week)

- Full evaluation on replays and benchmarks; a write-up of what blending
  and PIE cost on a hybrid model. That result is new, whatever it is, and
  worth publishing.

## Later, if wanted

- **Remote GPU backend**: `--backend vllm`, with LMCache for storage, and
  our blending patched in for LFM3 (or LMCache's, if it gains hybrid
  support).
- **Structure-aware selection**, after CodeComp: tree-sitter def-use and
  call sites added to the span scores.
- **Rust engine**: the same protocol, the model runner in Rust (mlx-rs or
  candle), shipped inside vmac instead of installed with pip.
- **Compressed context**, after HEF: chunks as a few learned vectors instead
  of full KV. It needs the generator trained for it, so it's a separate
  project, but it would shrink the store by orders of magnitude.

## Risks

| Risk | What we'd see | Fallback |
|------|---------------|----------|
| LFM3 can't do FIM, and the fine-tune doesn't fix it | low exact match mid-file | prefix-only completions; or another small FIM model under the same engine |
| Blending loses too much on a hybrid model | Boundary and Blend over 1 point below Full | train with stitched caches (M2); raise the budget; fall back to Prefix for the repo part |
| PIE's staleness hurts more with conv layers | replay quality drifts with edits | longer repair spans, lower recompute threshold |
| MLX is slower than assumed | M0 numbers | smaller budgets, more aggressive prewarming, llama.cpp backend |
| Battery and heat | `powermetrics` in M5 | prewarm only on AC; lower decode budget on battery |
