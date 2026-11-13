"""The MLX backend against a tiny LFM3 with random weights: reusing the
cache (appends, rewinds to marks, generated tokens dropped) must give the
same logits as computing the prompt from nothing. Runs wherever MLX does
(Apple silicon, or Linux with mlx[cpu]); skipped elsewhere."""

import pytest

mx = pytest.importorskip("mlx.core")
lfm3 = pytest.importorskip("mlx_vlm.models.lfm3.language")
from mlx_vlm.models.lfm3.config import ModelConfig  # noqa: E402

from vmac_complete.backends import MLXBackend  # noqa: E402
from vmac_complete.cache import SequenceCache  # noqa: E402
from vmac_complete.engine import Engine, Request  # noqa: E402
from vmac_complete.documents import Document, Edit  # noqa: E402
from vmac_complete.prompt import PromptConfig  # noqa: E402


class CharTokenizer:
    eos_token_id = 1
    bos_token_id = None

    def encode(self, text, add_special_tokens=False):
        return [2 + (ord(c) % 250) for c in text]

    def decode(self, ids):
        return "".join(chr(i - 2 + 32) if 0 <= i - 2 + 32 < 127 else "?" for i in ids)

    def get_vocab(self):
        return {}


def tiny_backend():
    mx.random.seed(0)
    # The real model's layer pattern, shrunk; a sliding window of 16 so the
    # tests run past it.
    types = ["conv", "conv", "full_attention", "conv", "sliding_attention", "conv"] * 2
    cfg = ModelConfig(
        hidden_size=64, intermediate_size=128, num_hidden_layers=len(types),
        num_attention_heads=4, num_key_value_heads=2, head_dim=16, vocab_size=256,
        layer_types=types, sliding_window=16,
    )
    lm = lfm3.LanguageModel(cfg)
    mx.eval(lm.parameters())
    be = MLXBackend.__new__(MLXBackend)
    be.model, be.temperature, be.lm, be.tokenizer, be.eos = "tiny-lfm3", 0.0, lm, CharTokenizer(), {1}
    return be


def fresh_logits(be, tokens):
    return SequenceCache(be).prepare(tokens).logits


def close(a, b):
    return mx.allclose(a, b, atol=1e-4, rtol=1e-4).item()


def test_append_matches_fresh():
    be = tiny_backend()
    toks = [3 + (i * 7) % 200 for i in range(100)]
    seq = SequenceCache(be, mark_every=8, mark_span=64)
    seq.prepare(toks[:90])
    p = seq.prepare(toks)
    assert p.reused == 90 and p.fed == 10
    assert close(p.logits, fresh_logits(be, toks))


def test_rewind_past_the_sliding_window_matches_fresh():
    be = tiny_backend()
    toks = [3 + (i * 11) % 200 for i in range(120)]
    seq = SequenceCache(be, mark_every=8, mark_span=64)
    seq.prepare(toks)
    # Backspace 20 tokens: more than the window of 16 must be restored.
    p = seq.prepare(toks[:100])
    assert p.reused == 96 and p.fed == 4
    assert close(p.logits, fresh_logits(be, toks[:100]))
    # Then type something else.
    other = toks[:100] + [5, 6, 7]
    p = seq.prepare(other)
    assert p.reused == 100
    assert close(p.logits, fresh_logits(be, other))


def test_generated_tokens_dropped_matches_fresh():
    be = tiny_backend()
    toks = [3 + (i * 13) % 200 for i in range(80)]
    seq = SequenceCache(be, mark_every=8, mark_span=64)
    logits = seq.prepare(toks).logits
    for _ in range(12):
        logits = seq.advance(be.sample(logits))
    nxt = toks + [42]
    p = seq.prepare(nxt)
    assert p.reused == 80 and p.fed == 1
    assert close(p.logits, fresh_logits(be, nxt))


def test_typing_through_the_engine_keeps_matching():
    be = tiny_backend()
    text = "".join(f"x{i} = {i}\n" for i in range(30))
    doc = Document("/r/a.py", "python", text, 0)
    eng = Engine(be, PromptConfig(mode="prefix"), "/r")
    pos = len(text)
    outs = []
    for k, ch in enumerate("y = x1"):
        doc.apply([Edit(pos + k, pos + k, ch)], k + 1)
        r = eng.complete(Request(k, 0, k + 1, pos + k + 1, max_tokens=8, stop="line"), doc)
        outs.append(r)
        # From nothing, the same completion.
        fresh = Engine(be, PromptConfig(mode="prefix"), "/r").complete(Request(k, 0, k + 1, pos + k + 1, max_tokens=8, stop="line"), doc)
        assert r.text == fresh.text
    assert all(o.stats["tokens_prefilled"] <= 3 for o in outs[1:])


def test_rotation_check_on_lfm3(tmp_path):
    """Moving cached keys by rotation is exact (to float error) on LFM3's
    partial rotary embeddings, global and sliding layers alike."""
    from vmac_complete.bench import bench_rotation

    f = tmp_path / "a.py"
    f.write_text("def f(x):\n    return x * 2\n" * 10)
    r = bench_rotation(tiny_backend(), [f], shift=300, n=64)
    assert r["layers"]
    for row in r["layers"]:
        assert row["keys_max_rel_err"] < 1e-3, row
        assert row["keys_unrotated_rel_err"] > 1e-2, row  # it did need rotating
        assert row["values_max_rel_err"] < 1e-3, row
