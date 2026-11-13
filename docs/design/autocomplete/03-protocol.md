# 03. The protocol between vmac and the engine

One JSON object per line over the engine's stdin and stdout, the way vmac
talks to ACP agents (`src/agent.rs`) and to its own windows
(`src/daemon.rs`). Requests that expect an answer carry an `id`; the rest are
notifications. Stderr is the engine's log, kept like a package's install
log.

Positions are **character offsets** into the buffer, which is what vmac's
`Buffer` uses everywhere (not LSP's UTF-16 columns). The engine maps them to
tokens itself.

Every document message carries the buffer's **revision** (`Buffer::revision`,
bumped on every change). A completion is only shown if its revision is still
the buffer's revision when it arrives, or if what was typed since is a
prefix of it.

## vmac → engine

```jsonc
// Once, after spawning.
{"id": 1, "method": "initialize", "params": {
  "root": "/Users/yojan/git/vmac",          // the session's project
  "model": "LiquidAI/LFM3-1.7B",            // from Lua config
  "store": "~/.cache/vmac/kv",              // the SSD tier's folder
  "budget": {"ram_mb": 1024, "ssd_mb": 8192},
  "client": {"name": "vmac", "version": "0.0.11"}
}}

// A buffer the engine should mirror (open buffers with a file, in a project).
{"method": "open", "params": {"doc": 7, "path": "/…/src/buffer.rs",
  "language": "rust", "revision": 0, "text": "…"}}

// Every edit, as it happens, in order. Drained from the buffer's edit log.
{"method": "edit", "params": {"doc": 7, "revision": 42,
  "edits": [{"start": 1290, "end": 1290, "text": "s"}]}}

// Undo, redo, reload from disk: the whole text.
{"method": "replace", "params": {"doc": 7, "revision": 43, "text": "…"}}

{"method": "save",  "params": {"doc": 7}}     // the file on disk changed
{"method": "close", "params": {"doc": 7}}

// Where the cursor is, sent when insert state starts and when the cursor
// rests in normal state (~300 ms). A hint: the engine prewarms the context
// for this spot so the first completion doesn't pay for blending.
{"method": "focus", "params": {"doc": 7, "revision": 43, "pos": 1291,
  "hints": {"definitions": [{"path": "/…/src/editor.rs", "start": 8812, "end": 9930}]}}}

// Ask for a completion at the cursor.
{"id": 9, "method": "complete", "params": {"doc": 7, "revision": 43, "pos": 1291,
  "max_tokens": 64, "stop": "block"}}        // "line" | "block"

{"method": "cancel", "params": {"id": 9}}

// What happened to a shown completion (for the evaluation and for tuning).
{"method": "outcome", "params": {"id": 9, "accepted_chars": 31, "how": "tab"}}
```

`hints.definitions` are optional ranges vmac already knows through its
language server (where identifiers near the cursor are defined). The engine
retrieves on its own too; hints just rank first.

## engine → vmac

```jsonc
{"id": 1, "result": {"ready": true, "model": "LFM3-1.7B-4bit", "fim": true}}

// Streamed pieces, then the end. Text is what goes at `pos`.
{"method": "partial", "params": {"id": 9, "revision": 43, "text": "elf.text.len_"}}
{"id": 9, "result": {"revision": 43, "text": "elf.text.len_chars()", "stats": {
  "ms_to_first": 61, "ms_total": 138,
  "tokens_cached": 5210, "tokens_recomputed": 4, "tokens_decoded": 7,
  "context_switch": false}}}

// Loading the model, prewarming, out of memory. Shown on the modeline.
{"method": "status", "params": {"state": "warming", "detail": "12 chunks"}}
```

## Rules

- **Edits are never dropped.** The engine applies them in order; if a
  revision skips, it asks for the text (`{"method": "resync", "params":
  {"doc": 7}}`) and vmac answers with `replace`.
- **One completion in flight per buffer.** A new `complete` cancels the old
  one; the engine stops decoding it at the next token.
- **The engine may answer with nothing** (`"text": ""`) when it has no
  confident completion; vmac shows nothing.
- **Typing through a shown completion asks nothing.** If you type the next
  characters of the ghost text, vmac trims it locally. It asks again only
  when you type something else, or reach the end.
- **The engine can die.** vmac restarts it on the next `complete` (with a
  back-off), re-sending `initialize` and `open` for its buffers; caches on
  the SSD survive.

## Swapping the engine

Anything that speaks this protocol can be the engine. Planned ones:

| Engine | Where | Caching |
|--------|-------|---------|
| `vmac-complete` (MLX) | Mac | full design: chunks, blending, edit repair, SSD tier |
| `vmac-complete --backend llamacpp` | CPU, remote Linux without GPU | prefix cache only, plain RAG prompts |
| `vmac-complete --backend vllm` | a GPU box | forwards to a vLLM + LMCache server; LMCache's blending once it supports the model, or our patch |
