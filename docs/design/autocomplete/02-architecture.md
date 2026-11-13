# 02. Architecture

## The pieces

```
 window (shell.rs)                     server process (vmac --server)
 ┌───────────────┐   Frame / Patch    ┌──────────────────────────────────────┐
 │ draws ghost   │ ◄───────────────── │ Session ─ Editor ─ Buffers           │
 │ text after    │                    │            │  edits (buffer.rs)      │
 │ the cursor    │ ── Input (keys) ─► │            ▼                         │
 └───────────────┘                    │      complete.rs  (client)           │
                                      │            │ JSON lines, stdio       │
                                      └────────────┼─────────────────────────┘
                                                   ▼
                                      ┌──────────────────────────────────────┐
                                      │ vmac-complete (engine)               │
                                      │  mirror of open buffers              │
                                      │  chunker (tree-sitter)               │
                                      │  retriever                           │
                                      │  sequence manager (live KV, SPM)     │
                                      │  blender + edit repair (PIE)         │
                                      │  model runner (MLX, LFM3-1.7B)       │
                                      │            │                         │
                                      │  chunk store: memory, then SSD       │
                                      └──────────────────────────────────────┘
```

**vmac** keeps doing what it does: buffers, keys in Lua, frames to the
window. It gains a client for the engine and a way to draw ghost text.

**`vmac-complete`** is a separate process, one per vmac server (shared by
its sessions, so one model is loaded once). It owns the model, every KV
cache, and retrieval. vmac never sees a token or a tensor.

**The chunk store** is the engine's cache of chunk KVs, in three tiers
([05-storage.md](05-storage.md)).

## Why a separate process

- It's how vmac already runs language servers (`src/lsp.rs`) and agents
  (`src/agent.rs`): a child on stdio, a reader thread into a channel,
  drained by the editor. The new client copies that shape.
- The model and its memory stay out of the editor. A crash or an
  out-of-memory kill takes completions down, not your buffers.
- The headless server build (`--no-default-features`, what `vmac connect`
  installs on remote machines) doesn't grow an ML runtime.
- The engine can be written where ML is easy (Python + MLX now) and swapped
  later (a Rust engine, or a vLLM + LMCache server), behind the same
  protocol.

## Where it goes in vmac

| Change | Where | Notes |
|--------|-------|-------|
| Engine client | new `src/complete.rs` | spawn, JSON lines, reader thread, `pump()`; like `lsp.rs` |
| Edit stream | `Buffer::edited` in `src/buffer.rs` | already called with `(start, end, added)` for every edit; add an edit log (revision, range, inserted text) the client drains, instead of syncing whole documents the way LSP does today |
| Undo/redo | `Buffer::restore` | replaces the whole text; sent as one full-replace edit, the engine diffs it |
| Ghost text state | `Editor` in `src/editor.rs` | `ghost: Option<Ghost>` next to `completion`: buffer, revision, position, text |
| Frame | `src/frame.rs` | `FrameLine` gets `ghost: Option<(usize, String)>` (column, text on this line) and `FrameWindow` gets the extra lines below; both `skip_serializing_if` empty so old windows ignore them |
| Drawing | `src/shell.rs` | ghost text in a dim face from the theme (`ghost` face, set in Lua), after the cursor, pushing the rest of the line right |
| Primitives | `src/lisp.rs` | `ghost-active?`, `ghost-accept` (all / word / line), `ghost-dismiss`, `autocomplete-request` |
| Keys and policy | `lua/core/60-lsp.lua` (or a new `65-autocomplete.lua`) | when to ask, debounce, which keys accept; next to `core.after_self_insert`, which already asks LSP on word characters |
| Pumping | `src/server.rs`, `src/daemon.rs`, `src/shell.rs` | see below |
| Install | `src/packages.rs` | `vmac-complete` as a pip package and the model weights as a download, installed like a language server, listed in `:packages` |

### Pumping fast enough

Language server replies are drained every 100 ms (`LSP_POLL`). That alone
would eat most of the latency budget. Completion replies are drained on the
terminal timer instead (`TERMINAL_POLL`, 30 ms), through a new
`Host::pump_completions`, and a later change can make the daemon wake on the
engine's channel instead of polling at all.

### Keys and behaviour (Lua)

All policy is Lua, like the rest of vmac, so it can be redefined live:

- Insert state only. Typing asks after a short debounce
  ([06](06-latency.md)); if what you typed matches the start of the ghost
  text, it's consumed and nothing is asked.
- `TAB` accepts all of it when the LSP popup is closed (today `TAB` in
  insert falls through to `table-next-cell`); `M-f` accepts a word, `C-e`
  a line. `ESC`, moving, or any non-matching key dismisses.
- While the LSP popup is open, ghost text is hidden. The popup wins.
- `vmac.set_autocomplete({ enabled = true, debounce_ms = 40, ... })` in
  `init.lua`, and `:autocomplete-toggle`.

## Remote machines

With `vmac connect`, sessions run on the remote machine, near the files, and
the window is on your Mac. The engine runs where the sessions run, because
that's where the files and buffers are. On a Linux box that has a GPU, it
runs a PyTorch backend, or talks to a vLLM + LMCache server. On one without,
it falls back to plain prompts with llama.cpp on CPU, or completions are off.
The ghost text reaches the window in the frame patches that already cross
the ssh connection, so remote latency is the ssh round trip plus the local
budget.

## What the engine is made of

| Part | Job |
|------|-----|
| Document mirror | open buffers' text, kept in step from the edit stream; files on disk for the rest |
| Chunker | tree-sitter chunks: functions, methods, classes, impls; big ones split at statement boundaries; small neighbours merged; target 64–512 tokens |
| Retriever | picks the repo chunks for the cursor ([04](04-cache-lifecycle.md#retrieval)) |
| Sequence manager | the live sequence for the active cursor: which chunks, in what order, and its KV |
| Blender | stitches cached chunk KVs into the sequence and recomputes the anchored spans |
| Edit repair | updates cached chunk KVs after small edits (PIE plus a repair span) |
| Chunk store | the KV tiers, keyed by content hash |
| Model runner | the LFM3 forward pass with explicit control of each layer's KV and conv state, so spans can be recomputed; MLX on Apple silicon |

The model runner is our own forward pass, based on the MLX model code, not
a stock server, because blending needs to run the model on a set of token
spans against a KV cache that it didn't build in one pass. That's a few
hundred lines for a 24-layer model.
