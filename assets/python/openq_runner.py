# OpenQ's script runner, passed to the sandboxed interpreter with -c.
# Reads {"api", "name", "code", "params"} as JSON on stdin, runs the script with the `openq` module built
# from "api" (openq.param reads "params"), and writes one JSON reply on stdout:
# {"ok", "effects", "params" | "message"/"line"/"traceback", "output"}.
# The script's own prints are captured into "output". The sandbox has no files besides a read-only
# standard library, no network and no other programs; this file only shapes the reply.
import json
import sys
import traceback
import types

MAX_OUTPUT = 1 << 18

req = json.loads(sys.stdin.read())
name = req.get("name") or "<script>"
api = types.ModuleType("openq")
exec(compile(req["api"], "<openq>", "exec"), api.__dict__)
api._values = dict(req.get("params") or {})
sys.modules["openq"] = api
reply_to = sys.stdout


class Capture:
    def __init__(self):
        self.text, self.full = [], False

    def write(self, s):
        if not self.full:
            self.text.append(str(s))
            self.full = sum(len(t) for t in self.text) > MAX_OUTPUT
        return len(s)

    def flush(self):
        pass


cap = Capture()
sys.stdout = sys.stderr = cap
reply = {}
try:
    exec(compile(req["code"], name, "exec"), {"__name__": "__main__", "__file__": name})
    reply = {"ok": True}
except SystemExit as e:
    reply = {"ok": e.code in (None, 0)}
    if not reply["ok"]:
        reply.update(message=f"SystemExit: {e.code}", line=None, traceback="")
except BaseException as e:  # noqa: BLE001
    line = None
    if isinstance(e, SyntaxError) and e.filename == name:
        line = e.lineno
    else:
        for frame in traceback.extract_tb(e.__traceback__):
            if frame.filename == name:
                line = frame.lineno
    reply = {"ok": False, "message": f"{type(e).__name__}: {e}", "line": line,
             "traceback": "".join(traceback.format_exception(type(e), e, e.__traceback__))}
if reply["ok"]:
    try:
        reply["effects"] = [e._json() for e in api._made]
        reply["params"] = api._params
    except BaseException as e:  # noqa: BLE001
        reply = {"ok": False, "message": f"{type(e).__name__}: {e}", "line": None, "traceback": ""}
text = "".join(cap.text)
reply["output"] = text[:MAX_OUTPUT] + ("\n… (output truncated)" if cap.full else "")
reply_to.write(json.dumps(reply, allow_nan=False))
reply_to.flush()
