"""The prompt for a completion at the cursor.

Layouts (docs/design/autocomplete/04-cache-lifecycle.md):

- ``prefix``: the file's name, then the text before the cursor. Works with
  any model; ignores the text after the cursor.
- ``spm``: suffix first, then the prefix, so typing at the cursor only adds
  tokens at the end of the prompt and everything cached before it stays.
- ``psm``: prefix, suffix, then generate. The usual FIM order; every key
  changes the middle of the prompt, so it's only here for comparison.

The window around the cursor starts and ends at anchors kept in the
document (see documents.py), not at a fixed distance from the cursor, so the
prompt's text doesn't change while you type inside it.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field

from .documents import Document

COMMENT = {
    "python": "#", "ruby": "#", "bash": "#", "toml": "#", "yaml": "#", "nix": "#",
    "elixir": "#", "lua": "--", "haskell": "--", "sql": "--",
    "html": "<!--", "markdown": "<!--",
}


@dataclass
class PromptConfig:
    mode: str = "spm"  # spm | psm | prefix (spm falls back to prefix without FIM tokens)
    fim_prefix: str = "<|fim_prefix|>"
    fim_suffix: str = "<|fim_suffix|>"
    fim_middle: str = "<|fim_middle|>"
    prefix_lines: int = 120
    prefix_max_chars: int = 12_000
    suffix_lines: int = 40
    suffix_max_chars: int = 4_000
    header: bool = True

    @classmethod
    def from_dict(cls, d: dict | None) -> "PromptConfig":
        c = cls()
        for k, v in (d or {}).items():
            if hasattr(c, k):
                setattr(c, k, type(getattr(c, k))(v))
        return c


@dataclass
class Prompt:
    # (text, special): special parts are single special tokens.
    parts: list[tuple[str, bool]] = field(default_factory=list)
    # The cursor's line up to the cursor, and after it to the line's end.
    line_before: str = ""
    line_after: str = ""
    # Where the window is, for tests and stats.
    prefix_start: int = 0
    suffix_end: int = 0


def line_start(text: str, pos: int) -> int:
    return text.rfind("\n", 0, pos) + 1


def line_end(text: str, pos: int) -> int:
    i = text.find("\n", pos)
    return len(text) if i < 0 else i


def nth_line_start_before(text: str, pos: int, n: int) -> int:
    """The start of the line `n` lines above `pos`'s line (or 0)."""
    start = line_start(text, pos)
    for _ in range(n):
        if start == 0:
            break
        start = line_start(text, start - 1)
    return start


def nth_line_end_after(text: str, pos: int, n: int) -> int:
    """The end (past the line break) of the line `n` lines below `pos`'s."""
    end = line_end(text, pos)
    for _ in range(n):
        if end >= len(text):
            return len(text)
        end = line_end(text, end + 1)
    return min(len(text), end + 1)


def anchor(doc: Document, pos: int, cfg: PromptConfig) -> tuple[int, int]:
    """The window's start and end for a cursor at `pos`, re-anchored only
    when the cursor has left the window or it has grown too big."""
    text = doc.text
    pos = max(0, min(pos, len(text)))

    start = doc.prefix_start
    if (
        start is None
        or start > pos
        or text.count("\n", start, pos) > cfg.prefix_lines * 3 // 2
        or pos - start > cfg.prefix_max_chars
    ):
        start = nth_line_start_before(text, pos, cfg.prefix_lines)
        if pos - start > cfg.prefix_max_chars:
            start = pos - cfg.prefix_max_chars
            nl = text.find("\n", start, pos)
            if nl >= 0:
                start = nl + 1

    end = doc.suffix_end
    lines_after = None if end is None or end < pos else text.count("\n", pos, end)
    if (
        lines_after is None
        or lines_after > cfg.suffix_lines * 2
        or (lines_after < cfg.suffix_lines // 2 and end < len(text))
        or end - pos > cfg.suffix_max_chars * 2
    ):
        end = nth_line_end_after(text, pos, cfg.suffix_lines)
        if end - pos > cfg.suffix_max_chars:
            end = pos + cfg.suffix_max_chars
            nl = text.rfind("\n", pos, end)
            if nl >= 0:
                end = nl + 1

    doc.prefix_start, doc.suffix_end = start, end
    return start, end


def header(doc: Document, root: str | None) -> str:
    path = doc.path
    if root and path.startswith(root.rstrip("/") + "/"):
        path = os.path.relpath(path, root)
    mark = COMMENT.get(doc.language, "//")
    close = " -->" if mark == "<!--" else ""
    return f"{mark} {path}{close}\n"


def build(doc: Document, pos: int, cfg: PromptConfig, root: str | None = None) -> Prompt:
    start, end = anchor(doc, pos, cfg)
    text = doc.text
    pos = max(0, min(pos, len(text)))
    head = header(doc, root) if cfg.header else ""
    prefix = head + text[start:pos]
    suffix = text[pos:end]
    p = Prompt(
        line_before=text[line_start(text, pos) : pos],
        line_after=text[pos : line_end(text, pos)],
        prefix_start=start,
        suffix_end=end,
    )
    if cfg.mode == "spm":
        p.parts = [(cfg.fim_suffix, True), (suffix, False), (cfg.fim_prefix, True), (prefix, False), (cfg.fim_middle, True)]
    elif cfg.mode == "psm":
        p.parts = [(cfg.fim_prefix, True), (prefix, False), (cfg.fim_suffix, True), (suffix, False), (cfg.fim_middle, True)]
    else:
        p.parts = [(prefix, False)]
    return p
