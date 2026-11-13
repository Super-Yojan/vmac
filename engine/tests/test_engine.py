import io
import json
import threading
import time

from vmac_complete import stops
from vmac_complete.backends import FakeBackend
from vmac_complete.cache import SequenceCache
from vmac_complete.documents import Document, Edit
from vmac_complete.engine import Engine, Request
from vmac_complete.prompt import PromptConfig, build
from vmac_complete.server import Server


# ---- documents

def test_edits_apply_in_order_and_shift_anchors():
    d = Document("/p/a.py", "python", "hello world", 0)
    d.prefix_start, d.suffix_end = 6, 11
    d.apply([Edit(0, 0, "say "), Edit(4, 9, "HELLO")], 2)
    assert d.text == "say HELLO world"
    assert (d.prefix_start, d.suffix_end) == (10, 15)
    assert d.revision == 2


def test_replace_finds_the_small_change():
    d = Document("/p/a.py", "python", "abc def ghi", 0)
    d.prefix_start = 8
    d.replace("abc dXf ghi", 1)
    assert d.text == "abc dXf ghi" and d.prefix_start == 8


# ---- prompt

def test_window_stays_put_while_typing():
    text = "".join(f"line {i}\n" for i in range(400))
    d = Document("/r/a.py", "python", text, 0)
    cfg = PromptConfig(mode="spm", prefix_lines=50, suffix_lines=10)
    pos = text.index("line 200")
    p1 = build(d, pos, cfg, "/r")
    d.apply([Edit(pos, pos, "x = 1\n")], 1)
    p2 = build(d, pos + 6, cfg, "/r")
    # Same suffix (it's after the cursor), and the prefix only grew at its end.
    assert p1.parts[1] == p2.parts[1]
    assert p2.parts[3][0].startswith(p1.parts[3][0])
    assert p1.parts[3][0].startswith("# a.py\n")


def test_prefix_mode_has_only_the_prefix():
    d = Document("/r/a.rs", "rust", "fn a() {}\nfn b", 0)
    p = build(d, len(d.text), PromptConfig(mode="prefix"), "/r")
    assert p.parts == [("// a.rs\nfn a() {}\nfn b", False)]


# ---- stops

def test_line_and_block_stops():
    assert stops.cut("foo()\nbar", "line", "x = ") == ("foo()", True)
    assert stops.cut("\n    y = 2\nz", "line", "x = 1") == ("\n    y = 2", True)
    got, done = stops.cut("\n    a = 1\n\n\nprint(a)\n", "block", "def f():")
    assert done and got == "\n    a = 1"
    got, done = stops.cut("x\n    a = 1\n  b\n", "block", "    if y:")
    assert done and got == "x\n    a = 1"


def test_tidy_drops_what_the_line_has():
    assert stops.tidy("a, b)", ")") == "a, b"
    assert stops.tidy("value  ", "") == "value"


# ---- the cache

def test_append_reuses_everything():
    be = FakeBackend()
    seq = SequenceCache(be)
    seq.prepare(list(range(1, 1001)))
    be.fed_total = 0
    p = seq.prepare(list(range(1, 1003)))
    assert (p.reused, p.fed) == (1000, 2) and be.fed_total == 2


def test_backspace_rewinds_to_a_mark():
    be = FakeBackend()
    seq = SequenceCache(be, mark_every=32, mark_span=256)
    seq.prepare(list(range(1, 1001)))
    p = seq.prepare(list(range(1, 991)))  # 10 tokens shorter
    assert p.reused == 960 and p.fed == 30  # the mark at 960
    assert seq.state == list(range(1, 991))


def test_generated_tokens_are_dropped_on_the_next_prompt():
    be = FakeBackend()
    seq = SequenceCache(be)
    seq.prepare([1, 2, 3, 4])
    seq.advance(9)
    seq.advance(9)
    p = seq.prepare([1, 2, 3, 4, 5])
    assert p.reused == 4 and seq.state == [1, 2, 3, 4, 5]


def test_far_change_starts_over():
    be = FakeBackend()
    seq = SequenceCache(be)
    seq.prepare(list(range(1, 1001)))
    p = seq.prepare([7] + list(range(2, 1001)))
    assert p.reused == 0 and p.fed == 1000


# ---- the engine

def test_engine_completes_and_keeps_its_cache():
    be = FakeBackend({"def ": "main():"})
    eng = Engine(be, PromptConfig(mode="prefix"))
    d = Document("/r/a.py", "python", "x = 1\ndef ", 0)
    r = eng.complete(Request(1, 0, 0, len(d.text), stop="line"), d)
    assert r.text == "main():"
    d.apply([Edit(len(d.text), len(d.text), "m")], 1)
    r2 = eng.complete(Request(2, 0, 1, len(d.text), stop="line"), d)
    assert r2.text == "ain():"
    assert r2.stats["tokens_prefilled"] <= 2 and r2.stats["tokens_cached"] > 5


# ---- the protocol

class Pipe(io.StringIO):
    def __init__(self):
        super().__init__()
        self.lines = []
        self.cv = threading.Condition()

    def write(self, s):
        with self.cv:
            self.lines.extend(json.loads(l) for l in s.splitlines() if l.strip())
            self.cv.notify_all()
        return len(s)

    def wait_for(self, pred, timeout=5):
        end = time.time() + timeout
        with self.cv:
            while True:
                for m in self.lines:
                    if pred(m):
                        return m
                left = end - time.time()
                if left <= 0:
                    raise AssertionError(f"no such message in {self.lines}")
                self.cv.wait(left)


def test_protocol_round_trip():
    out = Pipe()
    s = Server(out=out, make_backend=lambda p: FakeBackend({"fn ": "main() {}"}), log=io.StringIO())
    s.handle({"id": 1, "method": "initialize", "params": {"root": "/r"}})
    init = out.wait_for(lambda m: m.get("id") == 1)
    assert init["result"]["ready"] and init["result"]["mode"] == "prefix"
    s.handle({"method": "open", "params": {"doc": 3, "path": "/r/a.rs", "language": "rust", "revision": 0, "text": "fn"}})
    s.handle({"method": "edit", "params": {"doc": 3, "revision": 1, "edits": [{"start": 2, "end": 2, "text": " "}]}})
    s.handle({"id": 2, "method": "complete", "params": {"doc": 3, "revision": 1, "pos": 3, "stop": "line"}})
    r = out.wait_for(lambda m: m.get("id") == 2)
    assert r["result"]["text"] == "main() {}" and r["result"]["revision"] == 1


def test_stale_request_answers_empty():
    out = Pipe()
    s = Server(out=out, make_backend=lambda p: FakeBackend(), log=io.StringIO())
    s.handle({"id": 1, "method": "initialize", "params": {}})
    out.wait_for(lambda m: m.get("id") == 1)
    s.handle({"method": "open", "params": {"doc": 1, "path": "/a.py", "revision": 5, "text": "def "}})
    s.handle({"id": 2, "method": "complete", "params": {"doc": 1, "revision": 4, "pos": 4}})
    r = out.wait_for(lambda m: m.get("id") == 2)
    assert r["result"]["text"] == "" and r["result"]["stale"]


def test_edit_to_unknown_doc_asks_for_resync():
    out = Pipe()
    s = Server(out=out, make_backend=lambda p: FakeBackend(), log=io.StringIO())
    s.handle({"method": "edit", "params": {"doc": 9, "revision": 1, "edits": []}})
    assert out.wait_for(lambda m: m.get("method") == "resync")["params"]["doc"] == 9


def test_fim_tokens_found_under_other_spellings():
    from vmac_complete.prompt import resolve_fim

    vocab = {"<|fim_pre|>": 1, "<|fim_suf|>": 2, "<|fim_mid|>": 3}
    cfg = PromptConfig()
    assert resolve_fim(cfg, vocab.get)
    assert (cfg.fim_prefix, cfg.fim_suffix, cfg.fim_middle) == ("<|fim_pre|>", "<|fim_suf|>", "<|fim_mid|>")
    assert not resolve_fim(PromptConfig(), {}.get)
