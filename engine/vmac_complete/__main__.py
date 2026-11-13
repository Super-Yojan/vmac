"""vmac-complete [serve] [--backend mlx|fake] [--model ID]
vmac-complete bench all --source DIR --out DIR
"""

from __future__ import annotations

import argparse
import sys


def main(argv: list[str] | None = None) -> None:
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] == "bench":
        from .bench import main as bench

        return bench(argv[1:])
    if argv and argv[0] == "serve":
        argv = argv[1:]
    ap = argparse.ArgumentParser(prog="vmac-complete", description="vmac's completion engine, on stdio.")
    ap.add_argument("--backend", default=None, help="mlx (default) or fake; overrides initialize")
    ap.add_argument("--model", default=None, help="overrides initialize's model")
    a = ap.parse_args(argv)

    from . import backends
    from .server import Server

    def make(params: dict) -> backends.Backend:
        return backends.make(a.backend or params.get("backend") or "mlx", a.model or params.get("model"))

    Server(make_backend=make).serve()


if __name__ == "__main__":
    main()
