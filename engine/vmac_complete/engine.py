"""Completions: the prompt, the cached sequence, and decoding."""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass, field
from typing import Callable

from . import prompt as prompts
from . import stops
from .backends import Backend
from .cache import SequenceCache
from .documents import Document


@dataclass
class Request:
    id: int
    doc: int
    revision: int
    pos: int
    max_tokens: int = 64
    stop: str = "block"  # line | block
    cancelled: threading.Event = field(default_factory=threading.Event)


@dataclass
class Result:
    id: int
    revision: int
    text: str
    stats: dict


class Engine:
    def __init__(self, backend: Backend, cfg: prompts.PromptConfig | None = None, root: str | None = None) -> None:
        self.backend = backend
        self.cfg = cfg or prompts.PromptConfig()
        self.root = root
        self.seq = SequenceCache(backend)

    def complete(
        self,
        req: Request,
        doc: Document,
        on_partial: Callable[[str], None] | None = None,
        lock: threading.Lock | None = None,
    ) -> Result | None:
        """Builds the prompt from `doc` (read under `lock`, since edits keep
        arriving), prepares the cache, and decodes. None when cancelled."""
        t0 = time.perf_counter()
        if lock:
            with lock:
                p = prompts.build(doc, req.pos, self.cfg, self.root)
        else:
            p = prompts.build(doc, req.pos, self.cfg, self.root)
        ids = self.backend.encode_parts(p.parts)
        prep = self.seq.prepare(ids)
        if req.cancelled.is_set():
            return None

        logits = prep.logits
        generated: list[int] = []
        shown = ""
        first_ms = None
        done = False
        for _ in range(req.max_tokens):
            tok = self.backend.sample(logits)
            if tok in self.backend.eos:
                break
            generated.append(tok)
            if first_ms is None:
                first_ms = (time.perf_counter() - t0) * 1000
            text = self.backend.decode(generated)
            # A multi-byte character split over tokens decodes as U+FFFD
            # until its last token comes.
            if not text.endswith("�"):
                cut, done = stops.cut(text, req.stop, p.line_before)
                if on_partial and cut != shown and "\n" in cut[len(shown):]:
                    on_partial(stops.tidy(cut, p.line_after))
                shown = cut
                if done:
                    break
            if req.cancelled.is_set():
                return None
            logits = self.seq.advance(tok)

        text = stops.tidy(shown, p.line_after)
        total = (time.perf_counter() - t0) * 1000
        return Result(
            req.id,
            req.revision,
            text,
            {
                "ms_to_first": round(first_ms if first_ms is not None else total, 1),
                "ms_total": round(total, 1),
                "ms_prefill": round(prep.ms, 1),
                "tokens_prompt": len(ids),
                "tokens_cached": prep.reused,
                "tokens_prefilled": prep.fed,
                "tokens_decoded": len(generated),
            },
        )
