#!/usr/bin/env python3
"""The starter suite's mock model with a weak mode, for trying
`ferrule eval --variant routing` end to end without an API key (docs/routing.md).
Standard library only.

    python3 evals/routing/weak_mock.py [--port 8765] [--weak]

Without --weak it is exactly evals/starter/mock/model.py, which it loads
unchanged. With --weak it plays a cheap model that can't take feedback: told
that a check fails ("[ferrule] `…` fails"), it says it's done again without
fixing anything. Everything else it does as the starter mock does.

So with the weak one as the cheap model and the plain one as the strong
model, `cheap` fails the suite's verify tasks (the plausible first try never
gets fixed), `strong` passes everything, and `routed` passes everything too:
the failed check moves the task up to the strong model, which fixes it. That's
the mechanism, not a measure of any real pair of models.
"""

import argparse
import importlib.util
import os
import sys
from http.server import ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
MODEL = os.path.join(HERE, "..", "starter", "mock", "model.py")


def load_model():
    spec = importlib.util.spec_from_file_location("starter_mock", MODEL)
    model = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(model)
    return model


def weaken(model):
    """Swap in a decide() that ignores a failed check."""
    strong = model.decide

    def decide(messages):
        last = messages[-1] if messages else {}
        if last.get("role") == "user" and model.CHECK_FAILED.match(model.text(last)):
            system = next((model.text(m) for m in messages if m.get("role") == "system"), "")
            found = model.WORKSPACE.search(system)
            task = model.task_of(found.group(1)) if found else "the task"
            return model.reply(f"{task}: done.")
        return strong(messages)

    model.decide = decide


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--port", type=int, default=8765, help="0 picks a free one")
    ap.add_argument("--weak", action="store_true",
                    help="say done when a check fails, without fixing anything")
    args = ap.parse_args()
    model = load_model()
    if args.weak:
        weaken(model)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), model.Handler)
    kind = "weak mock model" if args.weak else "mock model"
    print(f"{kind} listening on http://127.0.0.1:{server.server_address[1]}/v1", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(main())
