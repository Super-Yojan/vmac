# 04. Cache lifecycle: chunks, blending, and edits

This is the core of the design: what is cached, how cached pieces are put
together into a prompt, and how they're kept right while you type and edit.

## Everything is a chunk

A **chunk** is a run of code with its own KV cache, computed once by itself:

- **Repo chunks**: tree-sitter nodes from files in the project (functions,
  methods, classes, impls, top-level blocks), 64–512 tokens, each with a
  one-line header naming its file (`// src/editor.rs`).
- **File chunks**: the same split applied to the file you're editing. The
  file around the cursor is made of chunks too, which is what makes moving
  the cursor cheap (below).

A chunk's cache is keyed by a hash of: the model and its quantization, the
chunk's token ids, and the anchor spacing. Same text, same key, whatever
file or position it came from. A chunk is computed at a fixed position after
a short fixed preamble (CacheBlend's "dummy prefix"), so its keys can be
rotated to wherever it lands.

What's stored per chunk ([05](05-storage.md) has sizes):

- K and V for the 6 global attention layers, every token.
- K and V for the 5 sliding-window layers, every token (spans recomputed
  inside the chunk attend to their neighbours through them).
- The conv layers' inputs for the 6 tokens before each **anchor**: the
  chunk's start, every 64 tokens, and the chunk's end. This is what lets a
  span be recomputed from an anchor without the whole chunk.

## The prompt layout

Suffix before prefix (SPM), with the text being typed last:

```
[preamble] [repo chunk 1] … [repo chunk k] [FIM suffix marker]
[rest of the cursor's chunk, after the cursor] [file chunks after it…]
[FIM prefix marker] [file chunks before the cursor's chunk…]
[the cursor's chunk, up to the cursor]  ← the live tail
[FIM middle marker] → generated text
```

Why this order:

- **Typing is an append.** The cursor is at the end of the sequence, so a
  typed character adds a token or two to the live tail. That's an ordinary,
  exact prefill of a few tokens against the cache. No blending, no repair.
  This is the case on almost every keystroke.
- **Backspace is a truncate.** Dropping the last tokens is exact for the
  attention layers. The conv layers' state at the new end is recomputed from
  the last anchor before it (at most 64 tokens, and in practice the live tail
  keeps a conv-state checkpoint every few tokens).
- **The things that change least come first.** Repo chunks change when
  retrieval picks others; the suffix changes when you edit after the cursor;
  the live tail changes every key.
- **The local file is within the sliding window.** The 256 tokens before the
  cursor are the end of the prefix, so the sliding layers see the code you're
  writing, computed exactly.

The suffix part is cut to a budget (default 512 tokens of the cursor's chunk
and the chunks after it); the prefix part to another (default 2,048); repo
chunks to a third (default 4,096). Budgets are in [06](06-latency.md).

## When the context changes

A **context switch** is anything that changes the sequence before the live
tail: entering insert state somewhere else, retrieval picking different
chunks, an edit in the suffix. Then the engine builds a new sequence:

1. Lay out the chunks as above. Chunks already in the current sequence keep
   their KV (moved if their position changed). The rest come from the chunk
   store (memory or SSD), or are computed fresh if missing.
2. Rotate each reused chunk's keys to its new position. Only the rotated
   half of each global-layer head (theta 1e6) and the sliding layers (theta
   1e4) are touched; values never are. This is exact.
3. **Blend**: recompute the spans whose KV is most wrong, now that the chunk
   sits after other text (below).
4. Prefill the live tail exactly.

The `focus` message ([03](03-protocol.md)) lets the engine do all of this
before the first keystroke: when you enter insert state, or rest the cursor
in normal state. So most context switches are off the keystroke path.

## Blending on a hybrid model

CacheBlend's observation: a chunk's cached KV is wrong in the places where
the chunk's meaning depends on what came before it, and those places are
few. It recomputes about 15% of tokens per layer (picking them on the first
layer by how much their KV deviates, then narrowing layer by layer), and
gets within about 0.02 F1 of full recompute at 2.2–3.3× lower time to first
token. It needs only the other tokens' K and V for that.

On LFM3 a recomputed token also needs its 6 predecessors' conv inputs at each
conv layer ([01](01-model.md#what-the-hybrid-layers-mean-for-caching)). So:

- **Recompute spans, not tokens.** Selection works as in CacheBlend: run the
  first attention layer for all tokens of the reused chunks (cheap), compare
  with the cached K and V, and score tokens by deviation. Then cover the
  top-scoring tokens with spans that start at anchors.
- **A span starts at an anchor** and runs through the model in full, layer by
  layer. At each conv layer, its first token reads the stored conv inputs of
  the 6 tokens before the anchor. At each attention layer, it reads the
  cached K and V of everything else plus its own fresh ones, and its fresh
  K and V replace the cached ones.
- **Every reused chunk's first span is always recomputed** (from its start
  anchor, 32 tokens by default). This is where cross-chunk attention matters
  most: a chunk's first tokens were computed with nothing real before them.
- **The budget is in recomputed tokens**, default 15% of reused tokens plus
  the first spans. Tuned in M3.

The first version (M3) does only the first-span part ("boundary
recompute"), which needs no selection pass. Deviation-based spans come after,
if the evaluation shows boundary recompute leaves quality on the table.

Later, selection can also use structure: CodeComp (2026) shows static
analysis (call sites, def-use, returns) picks important code tokens better
than attention alone. We have tree-sitter in the engine; a token being a call
site or a definition the cursor's code uses can add to its score. That's a
stretch goal.

## Retrieval

Which repo chunks go in, ranked, cut to the budget:

1. Definitions of identifiers within ±20 lines of the cursor: from vmac's
   language server (`focus` hints), else from a tree-sitter symbol index the
   engine keeps.
2. Chunks of files the current file imports.
3. Chunks of buffers you edited recently (they're also warm in the store).
4. Lexical similarity: identifier overlap (Jaccard) between the cursor's
   window and chunks, the RepoCoder / RepoBench baseline. No embedding model
   needed.

Retrieval reruns on `focus` and every few seconds of typing, not on every
key. Its result is kept stable: a chunk stays in unless something clearly
better appears (hysteresis), so the sequence, and its KV, survive.

## Keeping caches right while you edit

Three kinds of change, from cheapest:

**Typing at the cursor.** An append to the live tail. Exact.

**An edit inside a chunk that's cached but not at the cursor** (you fix a
line in a function, then move on; a save from an agent; `git checkout`).
The chunk's text changed, so its hash changed and its old cache no longer
matches. Rather than compute the new chunk from scratch, the engine repairs
the old cache the PIE way:

1. Tokenize the new text and diff it against the old tokens: the edit is a
   replaced token range `[i, j)` → `m` new tokens.
2. Keys after the edit are rotated by `m − (j − i)` positions, the single
   combined rotation PIE derives, so no two tokens share a position. (Without
   this, PIE's "temporal confusion" drops RepoBench exact match from about 29
   to about 5 on DeepSeek-Coder 6.7B.)
3. Recompute the edited tokens, plus a **repair span** after them (from the
   anchor before the edit, through 64 tokens after it). PIE leaves everything
   after an edit stale and loses little on a plain transformer (within 1–2%
   EM of full recompute, with over 85% less compute). LFM3's conv layers mix
   each token with its neighbours, so the tokens right after an edit are more
   wrong than in a transformer; the repair span fixes those. Its length is
   tuned in M4.
4. Store the repaired cache under the new hash, with a **staleness count**
   (edited tokens since it was last computed in one pass). Past a threshold
   (default 25% of the chunk, or 3 repairs), the chunk is recomputed in full
   when the engine is idle.

**An edit in the current file's suffix or prefix, away from the cursor**
(you jump up, change a signature, come back). The affected file chunk is
repaired as above, and the next completion is a context switch with that
chunk swapped in.

**Undo, reload, big pastes.** If the edit is more than half of a chunk, the
chunk is just recomputed.

## Lifecycle of a chunk, end to end

```
 file opened / retrieved
        │
        ▼
   chunked (tree-sitter) ──► hash ──► in store? ── yes ──► load (RAM ◄ SSD)
                                         │ no
                                         ▼
                                   compute alone at the fixed position
                                         │
                                         ▼
                           placed in a sequence: rotate, blend
                                         │
                 edited ──► repair (PIE + repair span) ──► new hash, staleness+1
                                         │
                      too stale ──► full recompute when idle
                                         │
                        unused ──► demoted RAM → SSD ──► evicted (LRU)
```
