#!/usr/bin/env python3
"""Run IQ Tables locally (standard library only; works from PyCharm's Run button).

    python run.py              rebuild if the Rust/web sources changed, then serve
    python run.py --no-build   just serve the committed build in site/
    python run.py --port 9000  pick a different port
    python run.py --no-open    don't open a browser tab

Serving over http://localhost gives the page a normal web origin, so browser
storage ("remember my account on this device") and file drops behave as they
will on IQ Pages. Rebuilding needs Rust with the WebAssembly target:
    rustup target add wasm32-unknown-unknown
If only rust-src is available, --build-std builds the standard library from source.
"""
import argparse
import base64
import functools
import http.server
import os
import shutil
import socketserver
import subprocess
import sys
import webbrowser
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SITE = ROOT / "site"
WASM = ROOT / "target" / "wasm32-unknown-unknown" / "release" / "iq_tables.wasm"

IQPAGES = """{
  "name": "iq-tables",
  "version": "0.1.0",
  "description": "IQ Tables: browse, draft and inscribe databases on IQ Labs tables",
  "entry": "index.html"
}
"""


def newest_source_mtime():
    files = list((ROOT / "src").rglob("*.rs")) + [ROOT / "web" / "index.html", ROOT / "web" / "host.js", ROOT / "Cargo.toml"]
    return max(f.stat().st_mtime for f in files if f.exists())


def needs_build():
    out = SITE / "index.html"
    return not out.exists() or newest_source_mtime() > out.stat().st_mtime


def have_wasm_target():
    rustup = shutil.which("rustup")
    if not rustup:
        return True  # plain cargo install; let cargo report it if missing
    r = subprocess.run([rustup, "target", "list", "--installed"], capture_output=True, text=True)
    return "wasm32-unknown-unknown" in r.stdout


def build(build_std=False):
    cargo = shutil.which("cargo")
    if not cargo:
        print("Rust isn't installed (no `cargo` on PATH) — serving the existing build.")
        print("Install it from https://rustup.rs, then: rustup target add wasm32-unknown-unknown")
        return False
    cmd = [cargo, "build", "--release", "--lib", "--target", "wasm32-unknown-unknown"]
    env = dict(os.environ)
    if build_std:
        cmd += ["-Z", "build-std=std,panic_abort"]
        env["RUSTC_BOOTSTRAP"] = "1"
    elif not have_wasm_target():
        print("The WebAssembly target is missing. Run:  rustup target add wasm32-unknown-unknown")
        print("Serving the existing build for now.")
        return False
    print("Building:", " ".join(cmd[1:]))
    if subprocess.run(cmd, cwd=ROOT, env=env).returncode != 0:
        print("Build failed — see the errors above.")
        return False
    assemble()
    return True


def assemble():
    """Same output as build.sh: one self-contained index.html + iqpages.json."""
    b64 = base64.b64encode(WASM.read_bytes()).decode("ascii")
    host_js = (ROOT / "web" / "host.js").read_text(encoding="utf-8")
    out = []
    for line in (ROOT / "web" / "index.html").read_text(encoding="utf-8").splitlines():
        if "<!--WASM-->" in line:
            out.append('<script type="application/wasm-base64" id="wasm-b64">%s</script>\n' % b64)
        elif '<script src="host.js"></script>' in line:
            out.append("<script>\n" + host_js + "</script>\n")
        else:
            out.append(line + "\n")
    SITE.mkdir(exist_ok=True)
    (SITE / "index.html").write_text("".join(out), encoding="utf-8", newline="\n")
    (SITE / "iqpages.json").write_text(IQPAGES, encoding="utf-8", newline="\n")
    print("Built site/index.html (%d KB, wasm %d KB)" % ((SITE / "index.html").stat().st_size // 1024, WASM.stat().st_size // 1024))


def serve(port, open_browser):
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(SITE))
    httpd = None
    for p in range(port, port + 20):
        try:
            httpd = http.server.ThreadingHTTPServer(("127.0.0.1", p), handler)
            port = p
            break
        except OSError:
            continue
    if httpd is None:
        sys.exit("No free port found from %d" % port)
    url = "http://localhost:%d/" % port
    print("IQ Tables running at", url, "(Ctrl+C or PyCharm's Stop button to quit)")
    if open_browser:
        webbrowser.open(url)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        httpd.server_close()


def main():
    ap = argparse.ArgumentParser(description="Build (if needed) and serve IQ Tables locally.")
    ap.add_argument("--no-build", action="store_true", help="serve site/ as is")
    ap.add_argument("--rebuild", action="store_true", help="build even if nothing changed")
    ap.add_argument("--build-std", action="store_true", help="build Rust's std from source (if the wasm target can't be installed)")
    ap.add_argument("--port", type=int, default=8000)
    ap.add_argument("--no-open", action="store_true", help="don't open a browser")
    args = ap.parse_args()
    if not args.no_build and (args.rebuild or needs_build()):
        build(args.build_std)
    if not (SITE / "index.html").exists():
        sys.exit("site/index.html is missing and couldn't be built.")
    serve(args.port, not args.no_open)


if __name__ == "__main__":
    main()
