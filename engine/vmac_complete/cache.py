"""The live sequence: the tokens the model has seen, and its cache.

Between completions the engine keeps the model's cache for the last prompt.
A new prompt that starts the same way reuses it: the cache is rewound to
the last point both share, and only the rest is fed to the model.

LFM3's cache can't simply be cut shorter. Its global attention layers can
(drop the last entries), but its sliding-window layers forget old entries as
they go and its conv layers keep only their last few inputs. So the cache
can only be rewound to a **mark**: a point where those small parts were
copied. Marks are taken at the end of each prompt (where generation starts)
and every `mark_every` tokens over its last `mark_span`, which covers
backspacing and edits near the cursor.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Any, Protocol


class Ops(Protocol):
    """What a backend does with a cache."""

    def new_state(self) -> Any: ...
    def feed(self, state: Any, tokens: list[int]) -> Any:
        """Runs the model on `tokens` after what `state` has seen, updating
        it, and returns the logits after the last one."""
        ...
    def mark(self, state: Any) -> Any: ...
    def rewind(self, state: Any, mark: Any) -> Any:
        """The state as it was at `mark` (may be `state`, changed)."""
        ...


def common_prefix(a: list[int], b: list[int]) -> int:
    n = min(len(a), len(b))
    i = 0
    while i < n and a[i] == b[i]:
        i += 1
    return i


@dataclass
class Prepared:
    logits: Any
    reused: int  # tokens of the prompt that came from the cache
    fed: int  # tokens the model ran on
    ms: float


@dataclass
class SequenceCache:
    ops: Ops
    mark_every: int = 32
    mark_span: int = 256
    chunk: int = 512  # most tokens fed in one go
    tokens: list[int] = field(default_factory=list)
    state: Any = None
    marks: list[tuple[int, Any]] = field(default_factory=list)  # (position, mark), ascending

    def reset(self) -> None:
        self.tokens, self.state, self.marks = [], None, []

    def prepare(self, prompt: list[int]) -> Prepared:
        """Makes the cache hold exactly `prompt`, reusing what it can, and
        returns the logits for the next token."""
        if not prompt:
            raise ValueError("empty prompt")
        t0 = time.perf_counter()
        shared = common_prefix(self.tokens, prompt)
        # At least one token has to be fed, for its logits.
        usable = min(shared, len(prompt) - 1)

        if self.state is not None and usable == len(self.tokens):
            start = usable  # a plain append
        else:
            best = None
            for pos, m in self.marks:
                if pos <= usable:
                    best = (pos, m)
            if best is None or self.state is None:
                self.state, self.marks, start = self.ops.new_state(), [], 0
            else:
                start = best[0]
                self.state = self.ops.rewind(self.state, best[1])
                self.marks = [(p, m) for p, m in self.marks if p <= start]

        logits = None
        pos = start
        end = len(prompt)
        while pos < end:
            step = min(self.chunk, end - pos)
            # Stop at the next mark position inside the last stretch.
            nxt = self._next_mark_at(pos, end)
            if nxt is not None and pos < nxt < pos + step:
                step = nxt - pos
            logits = self.ops.feed(self.state, prompt[pos : pos + step])
            pos += step
            if pos == nxt or pos == end:
                self.marks.append((pos, self.ops.mark(self.state)))
        self.tokens = list(prompt)
        self._trim_marks()
        return Prepared(logits, start, end - start, (time.perf_counter() - t0) * 1000)

    def advance(self, token: int) -> Any:
        """Feeds a generated token; returns the logits after it. Generated
        tokens aren't marked: the next prompt rewinds to its own end at worst."""
        self.tokens.append(token)
        return self.ops.feed(self.state, [token])

    def _next_mark_at(self, pos: int, end: int) -> int:
        """The first mark position after `pos`: multiples of `mark_every`
        within the last `mark_span` tokens, and the end."""
        first = max(0, end - self.mark_span)
        k = max(pos + 1, first)
        k = -(-k // self.mark_every) * self.mark_every
        return min(k, end)

    def _trim_marks(self) -> None:
        """Keeps marks within the last stretch, and the newest few beyond it."""
        if not self.tokens:
            return
        first = len(self.tokens) - self.mark_span
        old = [x for x in self.marks if x[0] < first][-2:]
        self.marks = old + [x for x in self.marks if x[0] >= first]
