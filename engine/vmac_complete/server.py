"""The protocol: one JSON object per line on stdin and stdout
(docs/design/autocomplete/03-protocol.md).

The reading thread applies document messages as they come, so edits never
wait behind a completion. Completions run one at a time on a worker; a new
`complete` cancels the one in flight, and only the newest waiting one runs.
"""

from __future__ import annotations

import json
import sys
import threading
import traceback
from typing import Any, Callable, TextIO

from . import backends
from .documents import Documents, Edit
from .engine import Engine, Request
from .prompt import PromptConfig, resolve_fim


class Server:
    def __init__(
        self,
        out: TextIO = sys.stdout,
        make_backend: Callable[[dict], backends.Backend] | None = None,
        log: TextIO = sys.stderr,
    ) -> None:
        self.out = out
        self.log = log
        self.out_lock = threading.Lock()
        self.docs = Documents()
        self.docs_lock = threading.Lock()
        self.make_backend = make_backend or (lambda p: backends.make(p.get("backend", "mlx"), p.get("model")))
        self.engine: Engine | None = None
        self.pending: Request | None = None
        self.current: Request | None = None
        self.wake = threading.Condition()
        self.stopping = False
        self.inits: list[threading.Thread] = []
        self.worker = threading.Thread(target=self._work, daemon=True)
        self.worker.start()

    # ---- output
    def send(self, msg: dict) -> None:
        line = json.dumps(msg, ensure_ascii=False)
        with self.out_lock:
            self.out.write(line + "\n")
            self.out.flush()

    def status(self, state: str, detail: str = "") -> None:
        self.send({"method": "status", "params": {"state": state, "detail": detail}})

    # ---- input
    def serve(self, inp: TextIO = sys.stdin) -> None:
        for line in inp:
            line = line.strip()
            if not line:
                continue
            try:
                self.handle(json.loads(line))
            except Exception as e:  # a bad message never kills the engine
                print(f"vmac-complete: {e}\n{traceback.format_exc()}", file=self.log, flush=True)
        # Input ended (vmac quit): finish what was asked, then go.
        for t in self.inits:
            t.join()
        with self.wake:
            while self.pending is not None or self.current is not None:
                self.wake.wait(0.05)
        self.stop()

    def handle(self, msg: dict) -> None:
        method, params, mid = msg.get("method"), msg.get("params") or {}, msg.get("id")
        if method == "initialize":
            t = threading.Thread(target=self._initialize, args=(mid, params), daemon=True)
            self.inits.append(t)
            t.start()
        elif method == "open":
            with self.docs_lock:
                self.docs.open(params["doc"], params.get("path", ""), params.get("language", ""), params.get("text", ""), params.get("revision", 0))
        elif method == "edit":
            with self.docs_lock:
                d = self.docs.get(params["doc"])
                if d is None:
                    self.send({"method": "resync", "params": {"doc": params["doc"]}})
                    return
                edits = [Edit(e["start"], e["end"], e.get("text", "")) for e in params.get("edits", [])]
                d.apply(edits, params.get("revision", d.revision + 1))
        elif method == "replace":
            with self.docs_lock:
                d = self.docs.get(params["doc"])
                if d is None:
                    self.send({"method": "resync", "params": {"doc": params["doc"]}})
                else:
                    d.replace(params.get("text", ""), params.get("revision", d.revision + 1))
        elif method == "close":
            with self.docs_lock:
                self.docs.close(params["doc"])
        elif method == "complete":
            self._enqueue(Request(
                id=mid,
                doc=params["doc"],
                revision=params.get("revision", 0),
                pos=params.get("pos", 0),
                max_tokens=int(params.get("max_tokens", 64)),
                stop=params.get("stop", "block"),
            ))
        elif method == "cancel":
            with self.wake:
                for r in (self.pending, self.current):
                    if r is not None and r.id == params.get("id"):
                        r.cancelled.set()
                if self.pending is not None and self.pending.cancelled.is_set():
                    self.pending = None
        elif method in ("focus", "save", "outcome"):
            pass  # hints for later milestones
        elif method == "shutdown":
            self.stop()
            if mid is not None:
                self.send({"id": mid, "result": None})
        elif mid is not None:
            self.send({"id": mid, "error": {"message": f"unknown method {method}"}})

    def _initialize(self, mid: Any, params: dict) -> None:
        self.status("loading", params.get("model") or "")
        try:
            backend = self.make_backend(params)
            cfg = PromptConfig.from_dict(params.get("prompt"))
            # FIM layouts need the model's FIM tokens; without them (and
            # unless asked for anyway), complete from the prefix alone.
            asked = (params.get("prompt") or {}).get("mode")
            has_fim = resolve_fim(cfg, backend.special)
            if cfg.mode != "prefix" and asked is None and not has_fim:
                cfg.mode = "prefix"
            self.engine = Engine(backend, cfg, params.get("root"))
            self.send({"id": mid, "result": {"ready": True, "backend": backend.name, "model": backend.model, "mode": cfg.mode, "fim": [cfg.fim_prefix, cfg.fim_suffix, cfg.fim_middle] if has_fim else None}})
            self.status("ready", backend.model)
        except Exception as e:
            print(traceback.format_exc(), file=self.log, flush=True)
            self.send({"id": mid, "error": {"message": f"couldn't load the model: {e}"}})
            self.status("error", str(e))

    # ---- completions
    def _enqueue(self, req: Request) -> None:
        with self.wake:
            if self.current is not None:
                self.current.cancelled.set()
            if self.pending is not None:
                self.send({"id": self.pending.id, "result": {"revision": self.pending.revision, "text": "", "cancelled": True}})
            self.pending = req
            self.wake.notify()

    def _work(self) -> None:
        while True:
            with self.wake:
                while self.pending is None and not self.stopping:
                    self.wake.wait()
                if self.stopping:
                    return
                req, self.pending = self.pending, None
                self.current = req
            try:
                self._run(req)
            except Exception as e:
                print(traceback.format_exc(), file=self.log, flush=True)
                self.send({"id": req.id, "error": {"message": str(e)}})
            finally:
                with self.wake:
                    self.current = None

    def _run(self, req: Request) -> None:
        # Asked while the model is still loading: wait for it.
        while self.engine is None and any(t.is_alive() for t in self.inits) and not req.cancelled.is_set():
            for t in self.inits:
                t.join(0.05)
        if req.cancelled.is_set():
            self.send({"id": req.id, "result": {"revision": req.revision, "text": "", "cancelled": True}})
            return
        if self.engine is None:
            self.send({"id": req.id, "error": {"message": "not ready"}})
            return
        with self.docs_lock:
            doc = self.docs.get(req.doc)
            stale = doc is not None and doc.revision != req.revision
        if doc is None or stale:
            self.send({"id": req.id, "result": {"revision": req.revision, "text": "", "stale": True}})
            return

        def partial(text: str) -> None:
            self.send({"method": "partial", "params": {"id": req.id, "revision": req.revision, "text": text}})

        result = self.engine.complete(req, doc, partial, self.docs_lock)
        if result is None:
            self.send({"id": req.id, "result": {"revision": req.revision, "text": "", "cancelled": True}})
        else:
            self.send({"id": req.id, "result": {"revision": result.revision, "text": result.text, "stats": result.stats}})

    def stop(self) -> None:
        with self.wake:
            self.stopping = True
            if self.current is not None:
                self.current.cancelled.set()
            self.wake.notify()
