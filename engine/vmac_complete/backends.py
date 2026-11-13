"""Models the engine can run.

- ``mlx``: LFM3 (or another model mlx-vlm / mlx-lm can load) on Apple
  silicon, with its cache kept between completions (cache.py).
- ``fake``: no model; completes from a fixed table. For tests, and for
  trying vmac's side without downloading anything.
"""

from __future__ import annotations

import copy
from typing import Any, Iterable


class Backend:
    name = "backend"
    model = ""
    eos: set[int] = set()

    # ---- text
    def encode(self, text: str) -> list[int]:
        raise NotImplementedError

    def special(self, text: str) -> int | None:
        """The id of a special token, or None when the vocabulary hasn't it."""
        return None

    def decode(self, ids: list[int]) -> str:
        raise NotImplementedError

    def encode_parts(self, parts: Iterable[tuple[str, bool]]) -> list[int]:
        """Each part on its own, so the tokens of one never merge with the
        next's (what keeps the suffix's tokens the same while the prefix
        changes)."""
        ids: list[int] = list(self.bos())
        for text, special in parts:
            if special:
                tid = self.special(text)
                ids.extend([tid] if tid is not None else self.encode(text))
            elif text:
                ids.extend(self.encode(text))
        return ids

    def bos(self) -> list[int]:
        return []

    # ---- the model (cache.Ops)
    def new_state(self) -> Any:
        raise NotImplementedError

    def feed(self, state: Any, tokens: list[int]) -> Any:
        raise NotImplementedError

    def mark(self, state: Any) -> Any:
        raise NotImplementedError

    def rewind(self, state: Any, mark: Any) -> Any:
        raise NotImplementedError

    def sample(self, logits: Any) -> int:
        raise NotImplementedError


class FakeBackend(Backend):
    """Characters are tokens. "Logits" are just the text so far; the next
    token continues the first entry of `table` whose key ends the text."""

    name = "fake"
    model = "fake"
    EOS = 0

    def __init__(self, table: dict[str, str] | None = None) -> None:
        self.table = table or {"def ": "main():\n    pass\n", "": ""}
        self.fed_total = 0  # tokens the "model" ran on, for tests
        self.eos = {self.EOS}

    def encode(self, text: str) -> list[int]:
        return [ord(c) for c in text]

    def special(self, text: str) -> int | None:
        return None

    def decode(self, ids: list[int]) -> str:
        return "".join(chr(i) for i in ids if i != self.EOS)

    def new_state(self) -> list[int]:
        return []

    def feed(self, state: list[int], tokens: list[int]) -> str:
        self.fed_total += len(tokens)
        state.extend(tokens)
        return self.decode(state)

    def mark(self, state: list[int]) -> int:
        return len(state)

    def rewind(self, state: list[int], mark: int) -> list[int]:
        del state[mark:]
        return state

    def sample(self, logits: str) -> int:
        for key, completion in self.table.items():
            for i in range(len(completion)):
                # The text ends with the key and the completion's first i chars.
                if logits.endswith(key + completion[:i]):
                    return ord(completion[i])
        return self.EOS


class MLXBackend(Backend):
    """A model under MLX. LFM3 loads through mlx-vlm (mlx-lm can't load it
    yet); other models through mlx-lm."""

    name = "mlx"

    def __init__(self, model: str, temperature: float = 0.0) -> None:
        import mlx.core as mx  # noqa: F401  (fails early off Apple silicon)

        self.model = model
        self.temperature = temperature
        self.lm, self.tokenizer = self._load(model)
        eos = getattr(self.tokenizer, "eos_token_id", None)
        self.eos = {eos} if isinstance(eos, int) else set(eos or [])
        for name in ("<|im_end|>", "<|endoftext|>"):
            tid = self.special(name)
            if tid is not None:
                self.eos.add(tid)

    @staticmethod
    def _load(model: str):
        """mlx-vlm for LFM3 (mlx-lm can't load it yet), mlx-lm for the rest
        (coding models like Qwen2.5-Coder, for comparison)."""
        if "lfm3" in model.lower():
            from mlx_vlm import load

            m, processor = load(model)
            return getattr(m, "language_model", m), getattr(processor, "tokenizer", processor)
        try:
            from mlx_lm import load as lm_load

            return lm_load(model)
        except Exception as lm_error:  # perhaps one only mlx-vlm knows
            try:
                from mlx_vlm import load

                m, processor = load(model)
                return getattr(m, "language_model", m), getattr(processor, "tokenizer", processor)
            except Exception:
                raise lm_error

    def encode(self, text: str) -> list[int]:
        return list(self.tokenizer.encode(text, add_special_tokens=False))

    def special(self, text: str) -> int | None:
        vocab = self.tokenizer.get_vocab()
        return vocab.get(text)

    def decode(self, ids: list[int]) -> str:
        return self.tokenizer.decode(ids)

    def bos(self) -> list[int]:
        bos = getattr(self.tokenizer, "bos_token_id", None)
        return [bos] if isinstance(bos, int) else []

    def new_state(self) -> list[Any]:
        if hasattr(self.lm, "make_cache"):
            return self.lm.make_cache()
        from mlx_lm.models.cache import make_prompt_cache

        return make_prompt_cache(self.lm)

    def _logits(self, out):
        return getattr(out, "logits", out)

    def feed(self, state: list[Any], tokens: list[int]):
        import mlx.core as mx

        out = self._logits(self.lm(mx.array([tokens]), cache=state))
        logits = out[:, -1, :]
        mx.eval(logits, [c.state for c in state])
        return logits

    @staticmethod
    def _plain_kv(c: Any) -> bool:
        """A cache that only grows and can be cut back (global attention)."""
        return type(c).__name__ in ("KVCache", "QuantizedKVCache")

    def mark(self, state: list[Any]) -> list[Any]:
        # Global attention: just how long it was. The rest (sliding window,
        # conv): a copy, which is small.
        return [c.offset if self._plain_kv(c) else copy.deepcopy(c) for c in state]

    def rewind(self, state: list[Any], mark: list[Any]) -> list[Any]:
        out = []
        for c, m in zip(state, mark):
            if self._plain_kv(c):
                c.trim(c.offset - m)
                out.append(c)
            else:
                out.append(copy.deepcopy(m))
        return out

    def sample(self, logits) -> int:
        import mlx.core as mx

        if self.temperature <= 0:
            return int(mx.argmax(logits, axis=-1).item())
        return int(mx.random.categorical(logits / self.temperature).item())


def make(name: str, model: str | None = None, **kw) -> Backend:
    if name == "fake":
        return FakeBackend(kw.get("table"))
    if name == "mlx":
        return MLXBackend(model or DEFAULT_MODEL, temperature=kw.get("temperature", 0.0))
    raise ValueError(f"unknown backend {name!r}")


DEFAULT_MODEL = "LiquidAI/LFM3-1.7B-MLX-4bit"
