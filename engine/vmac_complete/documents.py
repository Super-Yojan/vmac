"""The engine's copy of vmac's open buffers.

Positions are character offsets, as in vmac's buffers (a Python str is
indexed by code point, the same as ropey's chars). Each document also keeps
two anchors, where the prompt's prefix starts and its suffix ends. Edits
shift them like any other position, so the prompt's text, and the tokens
cached for it, stay the same while you type in between.
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class Edit:
    start: int
    end: int
    text: str


def shift(pos: int, edit: Edit) -> int:
    """Where `pos` is after `edit`: after it, moved by the change in length;
    inside the replaced range, at its start."""
    if pos >= edit.end:
        return pos - (edit.end - edit.start) + len(edit.text)
    if pos > edit.start:
        return edit.start
    return pos


@dataclass
class Document:
    path: str
    language: str
    text: str
    revision: int
    # Prompt anchors (character offsets), or None until a completion sets them.
    prefix_start: int | None = None
    suffix_end: int | None = None
    edits_seen: int = field(default=0)

    def apply(self, edits: list[Edit], revision: int) -> None:
        """Applies edits in order, each against the text the one before left."""
        for e in edits:
            start = max(0, min(e.start, len(self.text)))
            end = max(start, min(e.end, len(self.text)))
            e = Edit(start, end, e.text)
            self.text = self.text[:start] + e.text + self.text[end:]
            if self.prefix_start is not None:
                self.prefix_start = shift(self.prefix_start, e)
            if self.suffix_end is not None:
                # An insert right at the suffix's end stays outside it.
                self.suffix_end = shift(self.suffix_end, e) if e.start < self.suffix_end else self.suffix_end
            self.edits_seen += 1
        self.revision = revision

    def replace(self, text: str, revision: int) -> None:
        """The whole text (undo, reload): sent as the smallest edit, so the
        anchors survive when the change is small."""
        old = self.text
        a = 0
        limit = min(len(old), len(text))
        while a < limit and old[a] == text[a]:
            a += 1
        b = 0
        while b < limit - a and old[len(old) - 1 - b] == text[len(text) - 1 - b]:
            b += 1
        self.apply([Edit(a, len(old) - b, text[a : len(text) - b])], revision)


class Documents:
    def __init__(self) -> None:
        self.docs: dict[int, Document] = {}

    def open(self, doc: int, path: str, language: str, text: str, revision: int) -> None:
        self.docs[doc] = Document(path, language or "", text, revision)

    def get(self, doc: int) -> Document | None:
        return self.docs.get(doc)

    def close(self, doc: int) -> None:
        self.docs.pop(doc, None)
