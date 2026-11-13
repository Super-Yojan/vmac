"""Milestone M0: does LFM3 on this Mac do what the design needs?

    vmac-complete bench all --source ~/git/vmac --out ~/vmac-m0

runs four checks and writes `report.md` and `report.json`:

- speed: prefill at several context sizes, decode speed, and what reusing
  the cache saves (an append of two tokens, a backspace of 40).
- typing: types lines of a real file one character at a time, asking for a
  completion after each, with the cache kept and with it thrown away.
- fim: hides a line in the middle of real code and asks for it back, with
  each prompt layout the model's vocabulary allows.
- rotation: computes a chunk at position 0, rotates its keys to position
  3000, and compares them with computing it there. If they match, cached
  chunks can be moved anywhere in a prompt (CacheBlend's premise).
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import random
import statistics
import time
from pathlib import Path

from . import backends
from .cache import SequenceCache
from .documents import Document, Edit
from .engine import Engine, Request
from .prompt import PromptConfig

CODE_EXT = {".rs": "rust", ".py": "python", ".lua": "lua", ".go": "go", ".ts": "typescript", ".js": "javascript", ".c": "c", ".java": "java"}


def source_files(root: str, limit: int = 4000) -> list[Path]:
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if not d.startswith(".") and d not in ("target", "node_modules", "build", "dist", "venv")]
        for f in filenames:
            p = Path(dirpath) / f
            if p.suffix in CODE_EXT and p.stat().st_size < 400_000:
                out.append(p)
        if len(out) >= limit:
            break
    return sorted(out)


def pct(xs: list[float], q: float) -> float:
    if not xs:
        return float("nan")
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


# ---------------------------------------------------------------- speed

def bench_speed(backend: backends.Backend, files: list[Path], contexts: list[int], decode: int = 64) -> dict:
    corpus: list[int] = []
    for f in files:
        corpus.extend(backend.encode(f.read_text(errors="replace")))
        if len(corpus) > max(contexts) + 64:
            break
    if len(corpus) < max(contexts):
        contexts = [c for c in contexts if c <= len(corpus)] or [len(corpus)]
    # Warm up (kernels compile on first use).
    SequenceCache(backend).prepare(corpus[:256])
    rows = []
    for n in contexts:
        seq = SequenceCache(backend)
        prompt = corpus[:n]
        prep = seq.prepare(prompt)
        t0 = time.perf_counter()
        logits = prep.logits
        for _ in range(decode):
            logits = seq.advance(backend.sample(logits))
        decode_s = time.perf_counter() - t0
        append = seq.prepare(corpus[: n + 2])
        back = seq.prepare(corpus[: n - 40])
        rows.append({
            "context": n,
            "prefill_ms": round(prep.ms, 1),
            "prefill_tok_s": round(n / (prep.ms / 1000), 1),
            "decode_tok_s": round(decode / decode_s, 1),
            "append2_ms": round(append.ms, 1),
            "append2_fed": append.fed,
            "backspace40_ms": round(back.ms, 1),
            "backspace40_fed": back.fed,
        })
    return {"rows": rows}


# ---------------------------------------------------------------- typing

def pick_lines(files: list[Path], n: int, rng: random.Random, min_len: int = 12) -> list[tuple[Path, str, int]]:
    """(file, text, offset of a line's first non-blank char) for code lines."""
    picks = []
    tries = 0
    while len(picks) < n and tries < n * 50:
        tries += 1
        f = rng.choice(files)
        text = f.read_text(errors="replace")
        lines = text.split("\n")
        if len(lines) < 20:
            continue
        i = rng.randrange(len(lines) // 10, len(lines) * 9 // 10)
        line = lines[i]
        body = line.strip()
        if len(body) < min_len or body.startswith(("//", "#", "--", "/*", "*")):
            continue
        offset = sum(len(l) + 1 for l in lines[:i]) + (len(line) - len(line.lstrip()))
        picks.append((f, text, offset))
    return picks


def bench_typing(backend: backends.Backend, files: list[Path], cfg: PromptConfig, lines: int, rng: random.Random, root: str) -> dict:
    kept: list[float] = []
    fresh: list[float] = []
    fed_kept: list[int] = []
    fed_fresh: list[int] = []
    for f, text, offset in pick_lines(files, lines, rng):
        end = text.find("\n", offset)
        target = text[offset:end]
        doc = Document(str(f), CODE_EXT.get(f.suffix, ""), text[:offset] + text[end:], 0)
        engine = Engine(backend, cfg, root)
        for k, ch in enumerate(target):
            doc.apply([Edit(offset + k, offset + k, ch)], doc.revision + 1)
            req = Request(id=k, doc=0, revision=doc.revision, pos=offset + k + 1, max_tokens=16, stop="line")
            r = engine.complete(req, doc)
            kept.append(r.stats["ms_to_first"])
            fed_kept.append(r.stats["tokens_prefilled"])
            if k % 8 == 0:  # the same, from nothing: what plain prompting costs
                engine.seq.reset()
                r = engine.complete(req, doc)
                fresh.append(r.stats["ms_to_first"])
                fed_fresh.append(r.stats["tokens_prefilled"])
    return {
        "keystrokes": len(kept),
        "kept_ms_p50": round(pct(kept, 0.5), 1),
        "kept_ms_p95": round(pct(kept, 0.95), 1),
        "kept_fed_mean": round(statistics.mean(fed_kept), 1) if fed_kept else None,
        "fresh_ms_p50": round(pct(fresh, 0.5), 1),
        "fresh_ms_p95": round(pct(fresh, 0.95), 1),
        "fresh_fed_mean": round(statistics.mean(fed_fresh), 1) if fed_fresh else None,
    }


# ---------------------------------------------------------------- fim

def fim_tokens(backend: backends.Backend) -> list[str]:
    try:
        vocab = backend.tokenizer.get_vocab()  # type: ignore[attr-defined]
    except AttributeError:
        return []
    return sorted(t for t in vocab if "fim" in t.lower() or "middle" in t.lower())


def bench_fim(backend: backends.Backend, files: list[Path], n: int, rng: random.Random, root: str, base: PromptConfig) -> dict:
    modes = ["prefix"]
    if backend.special(base.fim_middle) is not None:
        modes += ["spm", "psm"]
    samples = pick_lines(files, n, rng)
    out = {"fim_tokens_in_vocab": fim_tokens(backend), "tried": modes, "n": len(samples), "modes": {}}
    for mode in modes:
        cfg = PromptConfig(**{**base.__dict__, "mode": mode})
        em, es, ms = [], [], []
        examples = []
        for f, text, offset in samples:
            end = text.find("\n", offset)
            target = text[offset:end].strip()
            doc = Document(str(f), CODE_EXT.get(f.suffix, ""), text[:offset] + text[end:], 0)
            engine = Engine(backend, cfg, root)
            r = engine.complete(Request(0, 0, 0, offset, max_tokens=48, stop="line"), doc)
            got = r.text.strip()
            em.append(float(got == target))
            es.append(difflib.SequenceMatcher(None, got, target).ratio())
            ms.append(r.stats["ms_total"])
            if len(examples) < 5:
                examples.append({"file": str(f.relative_to(root)) if str(f).startswith(root) else str(f), "want": target, "got": got})
        out["modes"][mode] = {
            "exact_match": round(statistics.mean(em) * 100, 1),
            "edit_similarity": round(statistics.mean(es) * 100, 1),
            "ms_p50": round(pct(ms, 0.5), 1),
            "examples": examples,
        }
    return out


# ---------------------------------------------------------------- rotation

def bench_rotation(backend: backends.Backend, files: list[Path], shift: int = 3000, n: int = 128) -> dict:
    """Keys of a chunk computed at `shift`, against the same chunk computed
    at 0 with its keys rotated by `shift`, per attention layer."""
    import mlx.core as mx

    lm = backend.lm  # type: ignore[attr-defined]
    layers = lm.layers
    corpus = backend.encode(files[0].read_text(errors="replace"))[:n]
    n = len(corpus)
    x = mx.array([corpus])

    at0 = backend.new_state()
    lm(x, cache=at0)
    there = backend.new_state()
    lm(x, cache=there, position_ids=mx.arange(n) + shift)
    mx.eval([c.state for c in at0], [c.state for c in there])

    rows = []
    for i, (layer, c0, c1) in enumerate(zip(layers, at0, there)):
        if not getattr(layer, "is_attention_layer", False):
            continue
        k0, v0 = c0.state[0][..., :n, :], c0.state[1][..., :n, :]
        k1, v1 = c1.state[0][..., :n, :], c1.state[1][..., :n, :]
        rotated = layer.self_attn.rope(k0, 0, mx.full((n,), shift))
        scale = mx.abs(k1).max().item() or 1.0
        rows.append({
            "layer": i,
            "type": getattr(layer, "layer_type", "attention"),
            "keys_max_rel_err": round(mx.abs(rotated - k1).max().item() / scale, 5),
            "keys_unrotated_rel_err": round(mx.abs(k0 - k1).max().item() / scale, 5),
            "values_max_rel_err": round(mx.abs(v0 - v1).max().item() / (mx.abs(v1).max().item() or 1.0), 5),
        })
    return {"shift": shift, "tokens": n, "layers": rows}


# ---------------------------------------------------------------- report

def report_md(r: dict) -> str:
    out = [f"# M0 report: {r['model']}", "", f"Run {r['when']} on `{r['machine']}`.", ""]
    if "speed" in r:
        out += ["## Speed", "", "| context | prefill ms | prefill tok/s | decode tok/s | +2 tokens ms (fed) | −40 tokens ms (fed) |", "|---|---|---|---|---|---|"]
        for x in r["speed"]["rows"]:
            out.append(f"| {x['context']} | {x['prefill_ms']} | {x['prefill_tok_s']} | {x['decode_tok_s']} | {x['append2_ms']} ({x['append2_fed']}) | {x['backspace40_ms']} ({x['backspace40_fed']}) |")
        out.append("")
    if "typing" in r:
        t = r["typing"]
        out += ["## Typing", "", f"{t['keystrokes']} keystrokes, a completion after each (first token).", "",
                "| | p50 ms | p95 ms | tokens prefilled (mean) |", "|---|---|---|---|",
                f"| cache kept | {t['kept_ms_p50']} | {t['kept_ms_p95']} | {t['kept_fed_mean']} |",
                f"| from nothing | {t['fresh_ms_p50']} | {t['fresh_ms_p95']} | {t['fresh_fed_mean']} |", ""]
    if "fim" in r:
        f = r["fim"]
        out += ["## Fill in the middle", "", f"{f['n']} hidden lines. FIM tokens in the vocabulary: {', '.join(f['fim_tokens_in_vocab']) or 'none'}.", "",
                "| layout | exact match % | edit similarity % | ms p50 |", "|---|---|---|---|"]
        for mode, x in f["modes"].items():
            out.append(f"| {mode} | {x['exact_match']} | {x['edit_similarity']} | {x['ms_p50']} |")
        out.append("")
        for mode, x in f["modes"].items():
            out.append(f"Examples ({mode}):")
            out.append("")
            for e in x["examples"]:
                out.append(f"- `{e['file']}`: want `{e['want']}`, got `{e['got']}`")
            out.append("")
    if "rotation" in r:
        ro = r["rotation"]
        out += ["## Rotation", "", f"{ro['tokens']} tokens, moved by {ro['shift']}. Relative error of rotated keys (unrotated for scale).", "",
                "| layer | type | keys rotated | keys unrotated | values |", "|---|---|---|---|---|"]
        for x in ro["layers"]:
            out.append(f"| {x['layer']} | {x['type']} | {x['keys_max_rel_err']} | {x['keys_unrotated_rel_err']} | {x['values_max_rel_err']} |")
        out.append("")
    for k, v in r.get("errors", {}).items():
        out += [f"**{k} failed:** `{v}`", ""]
    return "\n".join(out)


def main(argv: list[str] | None = None) -> None:
    ap = argparse.ArgumentParser(prog="vmac-complete bench", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("check", choices=["all", "speed", "typing", "fim", "rotation"])
    ap.add_argument("--model", default=backends.DEFAULT_MODEL)
    ap.add_argument("--backend", default="mlx")
    ap.add_argument("--source", default=".", help="a code folder to take text from")
    ap.add_argument("--out", default="vmac-m0")
    ap.add_argument("--contexts", default="2048,7000,16000")
    ap.add_argument("--fim-n", type=int, default=200)
    ap.add_argument("--typing-lines", type=int, default=6)
    ap.add_argument("--seed", type=int, default=0)
    a = ap.parse_args(argv)

    rng = random.Random(a.seed)
    root = os.path.abspath(os.path.expanduser(a.source))
    files = source_files(root)
    if not files:
        raise SystemExit(f"no code files under {root}")
    backend = backends.make(a.backend, a.model)
    r: dict = {"model": a.model, "when": time.strftime("%Y-%m-%d %H:%M"), "machine": os.uname().machine + " " + os.uname().sysname, "errors": {}}
    checks = ["speed", "typing", "fim", "rotation"] if a.check == "all" else [a.check]
    for check in checks:
        print(f"bench: {check}...", flush=True)
        try:
            if check == "speed":
                r["speed"] = bench_speed(backend, files, [int(x) for x in a.contexts.split(",")])
            elif check == "typing":
                r["typing"] = bench_typing(backend, files, PromptConfig(mode="prefix"), a.typing_lines, rng, root)
            elif check == "fim":
                r["fim"] = bench_fim(backend, files, a.fim_n, rng, root, PromptConfig())
            elif check == "rotation":
                r["rotation"] = bench_rotation(backend, files)
        except Exception as e:  # keep going; the report says what failed
            import traceback

            traceback.print_exc()
            r["errors"][check] = repr(e)
    out = Path(os.path.expanduser(a.out))
    out.mkdir(parents=True, exist_ok=True)
    (out / "report.json").write_text(json.dumps(r, indent=2))
    (out / "report.md").write_text(report_md(r))
    print(report_md(r))
    print(f"\nwrote {out / 'report.md'}")
