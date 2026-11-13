# vmac-complete

The completion engine vmac runs beside itself for ghost-text completions
(design: [docs/design/autocomplete/](../docs/design/autocomplete/)). vmac
starts it, keeps it in step with your buffers, and asks it for a completion
at the cursor when you pause typing. It answers with text, and keeps the
model's cache between keystrokes, so each key only feeds the model the new
tokens.

This is the prototype: milestones M0 (measure LFM3 on your Mac) and M1
(ghost text from the current file, with the cache kept). Repo-wide chunks,
blending and the SSD tier come next (M3 to M5).

## Install (Apple silicon)

From a vmac checkout:

```sh
python3 -m venv ~/.local/share/vmac/complete-venv
~/.local/share/vmac/complete-venv/bin/pip install -e "./engine[mlx]"
```

That gives `~/.local/share/vmac/complete-venv/bin/vmac-complete`. The model
(`LiquidAI/LFM3-1.7B-MLX-4bit`, about 1 GB) downloads from Hugging Face the
first time it runs.

## M0: measure it

```sh
vmac-complete bench all --source ~/git/vmac --out ~/vmac-m0
```

It takes a few minutes and writes `~/vmac-m0/report.md`:

- **speed**: prefill at 2K, 7K and 16K tokens, decode speed, and what keeping
  the cache saves (two tokens typed; 40 deleted).
- **typing**: types lines of real code one character at a time, asking for a
  completion after each, with the cache kept and from nothing.
- **fim**: hides 200 lines in the middle of real code and asks for them back,
  in each prompt layout the model's vocabulary allows. Tells us whether LFM3
  can fill in the middle, or whether we need the fine-tune (M2).
- **rotation**: moves a chunk's cached keys 3,000 positions by rotation and
  compares them with computing it there. If they match, cached chunks can go
  anywhere in a prompt, which the blending in M3 depends on.

Other models: `--model mlx-community/...`. One check: `bench speed`.

## M1: use it in vmac

In `~/.config/vmac/init.lua`:

```lua
vmac.set_autocomplete({
  enabled = true,
  command = { os.getenv("HOME") .. "/.local/share/vmac/complete-venv/bin/vmac-complete" },
})
```

In insert state, pause typing and a completion shows dimmed after the
cursor. `TAB` takes it, `M-f` a word, `M-e` a line; typing what it says keeps
it; anything else drops it. `:autocomplete-status` shows the engine's state
and what the last completion cost (tokens cached against prefilled, ms).
Its log is `~/.local/share/vmac/logs/vmac-complete.log`.

To try the keys without a model: `backend = "fake"` (it only completes
`def ` as `main():`).

## The protocol

One JSON object per line on stdin and stdout; see
[03-protocol.md](../docs/design/autocomplete/03-protocol.md). `focus`, `save`
and `outcome` are accepted and ignored for now.

## Tests

```sh
cd engine && python3 -m pytest
```

`tests/test_mlx.py` builds a tiny LFM3 with random weights and checks that
reusing the cache (appends, backspaces past the sliding window, generated
tokens dropped) gives the same logits as computing from nothing, and that
rotating cached keys is exact. It runs wherever MLX does, including Linux
with `pip install "mlx[cpu]" mlx-vlm`.
