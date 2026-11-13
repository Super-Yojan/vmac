"""Where a completion ends, and tidying it before it's shown."""

from __future__ import annotations


def indent_of(line: str) -> int:
    return len(line) - len(line.lstrip(" \t"))


def cut(text: str, stop: str, line_before: str, max_lines: int = 12) -> tuple[str, bool]:
    """The part of `text` (generated so far) to show, and whether it's done.

    ``line``: up to the end of the line. If the cursor's line is finished
    (only whitespace typed, or the model starts with a line break), the next
    line instead.
    ``block``: whole lines, until one is indented less than the cursor's
    line, two lines in a row are blank, or `max_lines`.
    """
    if stop == "line":
        if text.startswith("\n"):
            nl = text.find("\n", 1)
            return (text, False) if nl < 0 else (text[:nl], True)
        nl = text.find("\n")
        return (text, False) if nl < 0 else (text[:nl], True)

    base = indent_of(line_before) if line_before.strip() else len(line_before)
    lines = text.split("\n")
    # The last piece may be a line still being generated.
    complete, partial = lines[:-1], lines[-1]
    blank_run = 0
    for i, line in enumerate(complete):
        if i == 0:
            continue  # the rest of the cursor's line
        if not line.strip():
            blank_run += 1
            if blank_run >= 2:
                return "\n".join(complete[:i - 1]), True
            continue
        blank_run = 0
        if indent_of(line) < base:
            return "\n".join(complete[:i]).rstrip("\n"), True
        if i >= max_lines:
            return "\n".join(complete[:i]), True
    if complete and partial.strip() and indent_of(partial) < base:
        return "\n".join(complete), True
    return text, False


def tidy(text: str, line_after: str) -> str:
    """Drops trailing whitespace, and the end of a one-line completion that
    the rest of the line already has (`foo(|)` completed as `x)`)."""
    text = text.rstrip()
    rest = line_after.strip()
    if rest and "\n" not in text:
        for k in range(min(len(rest), len(text)), 0, -1):
            if text.endswith(rest[:k]):
                return text[:-k].rstrip() if text[:-k].strip() else text[:-k]
    return text
