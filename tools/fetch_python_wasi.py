#!/usr/bin/env python3
"""Fetch the sandboxed Python for effect scripts into target/python-wasi (development builds).

Users get it from oqman, which installs the same files next to the app. This is CPython built for
WebAssembly (WASI) by a CPython core developer (github.com/brettcannon/cpython-wasi-build), pinned
by version and SHA-256. Only python.wasm and a small part of the standard library are kept (the
modules effect scripts need); OpenQ runs it inside wasmtime with no files, network or programs.

Usage: python3 -I tools/fetch_python_wasi.py [destination]
Keep VERSION, URL, SHA256 and KEEP in step with oqman (src/python.rs).
"""

import hashlib
import io
import os
import shutil
import sys
import urllib.request
import zipfile

VERSION = "3.15.0"
URL = f"https://github.com/brettcannon/cpython-wasi-build/releases/download/v{VERSION}/python-{VERSION}-wasi_sdk-33.zip"
SHA256 = "00b791b3729230f97a625087f534d71978433fb1e068e13a9cac8cd3f2541742"
LIB = "lib/python3.15/"

# Standard library modules (files or packages) kept; everything else is left out.
KEEP = """
__future__ _collections_abc _colorize _compat_pickle _opcode_metadata _py_abc _py_warnings _pydatetime
_pydecimal _pylong _sitebuiltins _strptime _sysconfigdata__wasi_wasm32-wasi _weakrefset abc annotationlib ast
bisect calendar codecs codeop collections colorsys contextlib contextvars copy copyreg dataclasses datetime decimal
dis enum fractions functools genericpath heapq inspect io itertools json keyword linecache locale numbers
opcode operator os posixpath pprint random re reprlib stat statistics string struct textwrap token tokenize
traceback types typing warnings weakref importlib pathlib
encodings/__init__ encodings/aliases encodings/utf_8 encodings/utf_8_sig encodings/ascii encodings/latin_1
""".split()


def kept(rel):
    """Whether a path inside lib/python3.15/ is kept."""
    if "__pycache__" in rel or "/test" in rel or rel.startswith("test"):
        return False
    stem = rel[:-3] if rel.endswith(".py") else rel
    for k in KEEP:
        if "/" in k:
            if stem == k:
                return True
        elif stem == k or rel.startswith(k + "/") and not rel.startswith("encodings/"):
            return True
    return False


def main():
    dest = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "target", "python-wasi"))
    cached = os.environ.get("OPENQ_PYTHON_WASI_ZIP")  # an already downloaded zip (still checked)
    if cached:
        data = open(cached, "rb").read()
    else:
        print(f"downloading {URL}")
        data = urllib.request.urlopen(URL, timeout=120).read()
    digest = hashlib.sha256(data).hexdigest()
    if digest != SHA256:
        sys.exit(f"checksum mismatch: {digest}")
    if os.path.isdir(dest):
        shutil.rmtree(dest)
    os.makedirs(dest)
    n = 0
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        for info in z.infolist():
            name = info.filename
            if info.is_dir() or ".." in name.split("/") or name.startswith("/"):
                continue
            if name in ("python.wasm", "LICENSE") or name == LIB + "lib-dynload/":
                out = name
            elif name.startswith(LIB) and kept(name[len(LIB):]):
                out = name
            else:
                continue
            path = os.path.join(dest, out)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as f:
                f.write(z.read(info))
            n += 1
    print(f"{n} files in {dest}")


if __name__ == "__main__":
    main()
