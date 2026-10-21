"""Builds the fnOS package (ADR 0012): target/fpk/fncompress-<version>.fpk.

    cd web; npm run build; cd ..; cargo nas
    python -I scripts/build-fpk.py [--version 0.1.0-test] [--runtime DIR] [--runtime-sha256 HEX] [--local]

Packages web/dist (embedded in the binary) and the static musl binary, so
build both first: the Microsoft Store Python cannot run zig, whose
%LOCALAPPDATA% install it sees through its own virtualized AppData.

By default the official fnpack builds the package on the NAS (`ssh fnnas`):
the script lays out an fnpack project (manifest, app/ with the binary, ui/
and runtime/, cmd/, config/, wizard/, icons) in a tar, copies it to
/tmp/fnc-fpk, runs `fnpack build` there and copies the .fpk back. fnpack on
Windows loses the executable bits, so it runs on Linux.

--local writes the package here without fnpack, in fnpack 1.2.3's layout:
a gzip tar with manifest (plus `checksum`, the MD5 of app.tgz), app.tgz,
cmd/, config/, wizard/, ICON.PNG and ICON_256.PNG; app.tgz holds what
installs as $TRIM_APPDEST (the binary, ui/, runtime/, LICENSE and
THIRD_PARTY_NOTICES.md) plus copies of
config/privilege and config/resource.

Either way the result is read back: the checksum and the executable bits.

The scoring runtime (ADR 0006) is not in the repository: download
fncompress-scoring-runtime-*.zip from the GitHub Releases and unpack it into
runtime/ (ignored by git), or pass --runtime. scoring-linux-x86_64.zip in it is
checked against RUNTIME_SHA256; a runtime of your own needs --runtime-sha256.
cmd/main unpacks it on first start.
"""

import argparse
import gzip
import hashlib
import io
import json
import re
import subprocess
import sys
import tarfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PKG = ROOT / "packaging" / "fnos"
BINARY = ROOT / "target" / "x86_64-unknown-linux-musl" / "release" / "fncompress"
RUNTIME = ROOT / "runtime"
RUNTIME_ZIP = "scoring-linux-x86_64.zip"
RUNTIME_SHA256 = "2bf5029c1683ed0ac862279f4da5dc807a5d315b67b8a64a85df0d8d6a1f70aa"
NAS = "fnnas"
NAS_DIR = "/tmp/fnc-fpk"
REQUIRED = ["appname", "version", "display_name", "desc", "platform", "source",
            "desktop_uidir", "desktop_applaunchname", "service_port"]


def cargo_version():
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    section = text.split("[workspace.package]", 1)[1]
    return re.search(r'^version\s*=\s*"([^"]+)"', section, re.M).group(1)


def manifest_fields(text):
    fields = {}
    for line in text.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            fields[key.strip()] = value.strip()
    return fields


def add(tar, name, data=None, mode=0o644, mtime=0):
    info = tarfile.TarInfo(name)
    info.uid = info.gid = 0
    info.uname = info.gname = "root"
    info.mtime = mtime
    if data is None:
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
        tar.addfile(info)
    else:
        info.size = len(data)
        info.mode = mode
        tar.addfile(info, io.BytesIO(data))


def tgz(entries, mtime):
    """entries: [(name, bytes or None for a directory, mode)]"""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, data, mode in entries:
            add(tar, name, data, mode, mtime)
    return gzip.compress(buf.getvalue(), compresslevel=6, mtime=mtime)


def tree(base, prefix, executable=False):
    """Directory entries then files under base, as tar entries named prefix/..."""
    out = [(prefix, None, 0)]
    for path in sorted(base.rglob("*")):
        name = f"{prefix}/{path.relative_to(base).as_posix()}"
        if path.is_dir():
            out.append((name, None, 0))
        else:
            out.append((name, path.read_bytes(), 0o755 if executable else 0o644))
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", help="package version (default: the Cargo workspace version)")
    parser.add_argument("--runtime", type=Path, default=RUNTIME, help="scoring runtime directory (default: runtime/)")
    parser.add_argument("--runtime-sha256", default=RUNTIME_SHA256, help=f"SHA-256 of {RUNTIME_ZIP} (default: the released runtime)")
    parser.add_argument("--local", action="store_true", help="build here without fnpack (fnpack 1.2.3 layout)")
    args = parser.parse_args()

    index = ROOT / "web" / "dist" / "index.html"
    if not BINARY.is_file() or not index.is_file():
        sys.exit("build first: cd web; npm run build; cd ..; cargo nas")
    if BINARY.read_bytes()[:4] != b"\x7fELF":
        sys.exit(f"{BINARY} is not an ELF binary")
    if BINARY.stat().st_mtime < index.stat().st_mtime:
        sys.exit("web/dist is newer than the binary: run cargo nas")

    zip_path = args.runtime / RUNTIME_ZIP
    if not zip_path.is_file():
        sys.exit(f"missing scoring runtime {zip_path}: download it from the GitHub Releases into runtime/, or pass --runtime")
    zip_data = zip_path.read_bytes()
    if hashlib.sha256(zip_data).hexdigest() != args.runtime_sha256.lower():
        sys.exit(f"scoring runtime checksum mismatch: {zip_path} (a runtime of your own needs --runtime-sha256)")

    version = args.version or cargo_version()
    manifest = (PKG / "manifest").read_text(encoding="utf-8")
    manifest = re.sub(r"^(version\s*=\s*).*$", lambda m: m.group(1) + version, manifest, count=1, flags=re.M)
    fields = manifest_fields(manifest)
    missing = [k for k in REQUIRED if not fields.get(k)]
    if missing:
        sys.exit(f"manifest lacks {missing}")
    appname = fields["appname"]

    # Every lifecycle script runs with /bin/sh on the NAS: LF only.
    for script in (PKG / "cmd").iterdir():
        data = script.read_bytes()
        if b"\r" in data or not data.startswith(b"#!/bin/sh\n"):
            sys.exit(f"{script}: needs LF line endings and a #!/bin/sh line")
    for path in [PKG / "config" / "privilege", PKG / "config" / "resource", PKG / "app" / "ui" / "config",
                 *sorted((PKG / "wizard").iterdir())]:
        json.loads(path.read_text(encoding="utf-8"))
    ui = json.loads((PKG / "app" / "ui" / "config").read_text(encoding="utf-8"))
    if fields["desktop_applaunchname"] not in ui[".url"]:
        sys.exit("desktop_applaunchname is not an entry in app/ui/config")

    mtime = int(time.time())
    binary = BINARY.read_bytes()
    runtime = [("runtime", None, 0)]
    for path in sorted(args.runtime.iterdir()):
        if path.is_file():
            runtime.append((f"runtime/{path.name}", path.read_bytes(), 0o644))
    runtime.append((f"runtime/{RUNTIME_ZIP}.sha256", f"{args.runtime_sha256.lower()}  {RUNTIME_ZIP}\n".encode(), 0o644))
    config = [("config", None, 0)] + [
        (f"config/{n}", (PKG / "config" / n).read_bytes(), 0o644) for n in ("privilege", "resource")
    ]
    # The licence travels with the app ($TRIM_APPDEST).
    legal = [(n, (ROOT / n).read_bytes(), 0o644) for n in ("LICENSE", "THIRD_PARTY_NOTICES.md")]
    icons = [("ICON.PNG", (PKG / "ICON.PNG").read_bytes(), 0o644),
             ("ICON_256.PNG", (PKG / "ICON_256.PNG").read_bytes(), 0o644)]

    out_dir = ROOT / "target" / "fpk"
    out_dir.mkdir(parents=True, exist_ok=True)
    out = out_dir / f"{appname}-{version}.fpk"

    if args.local:
        app = tgz([("fncompress", binary, 0o755)] + tree(PKG / "app" / "ui", "ui") + runtime + legal + config, mtime)
        manifest = manifest.rstrip("\n") + f"\nchecksum              = {hashlib.md5(app).hexdigest()}\n"
        outer = (
            [("app.tgz", app, 0o644)]
            + tree(PKG / "cmd", "cmd", executable=True)
            + config
            + icons
            + [("manifest", manifest.encode("utf-8"), 0o644)]
            + tree(PKG / "wizard", "wizard")
        )
        out.write_bytes(tgz(outer, mtime))
    else:
        # The fnpack project, with the modes fnpack should keep.
        project = (
            [("manifest", manifest.encode("utf-8"), 0o644)]
            + icons
            + [("app", None, 0), ("app/fncompress", binary, 0o755)]
            + tree(PKG / "app" / "ui", "app/ui")
            + [(f"app/{name}", data, mode) for name, data, mode in runtime + legal]
            + tree(PKG / "cmd", "cmd", executable=True)
            + config
            + tree(PKG / "wizard", "wizard")
        )
        src = out_dir / f"{appname}-{version}-fnpack.tgz"
        src.write_bytes(tgz([(f"{appname}/{n}" if n else appname, d, m) for n, d, m in [("", None, 0)] + project], mtime))
        work = f"{NAS_DIR}/fnpack-{version}"
        remote = (
            f"set -e; rm -rf {work}; mkdir -p {work}; cd {work}; tar -xzf ../{src.name}; "
            f"fnpack build -d {appname} >fnpack.log 2>&1 || {{ cat fnpack.log; exit 1; }}; cat fnpack.log; "
            f"f=$(ls *.fpk {appname}/*.fpk 2>/dev/null | head -n 1); [ -n \"$f\" ] || {{ echo no fpk; ls -la . {appname}; exit 1; }}; "
            f"mv \"$f\" {NAS_DIR}/{out.name}"
        )
        for cmd in (["ssh", NAS, f"mkdir -p {NAS_DIR}"],
                    ["scp", "-q", str(src), f"{NAS}:{NAS_DIR}/"],
                    ["ssh", NAS, remote],
                    ["scp", "-q", f"{NAS}:{NAS_DIR}/{out.name}", str(out)]):
            if subprocess.run(cmd).returncode != 0:
                sys.exit(f"failed: {' '.join(cmd[:2])} ...")

    fpk = out.read_bytes()
    digest = hashlib.sha256(fpk).hexdigest()
    (out_dir / f"{out.name}.sha256").write_text(f"{digest}  {out.name}\n", encoding="ascii")

    # Read it back the way an installer would.
    with tarfile.open(out) as t:
        inner = t.extractfile("app.tgz").read()
        saved = manifest_fields(t.extractfile("manifest").read().decode("utf-8"))
        assert saved["checksum"] == hashlib.md5(inner).hexdigest()
        assert saved["version"] == version, saved["version"]
        cmds = [m for m in t.getmembers() if m.name.startswith("cmd/") and m.isfile()]
        assert len(cmds) == 9 and all(m.mode & 0o777 == 0o755 for m in cmds), [(m.name, oct(m.mode)) for m in cmds]
        with tarfile.open(fileobj=io.BytesIO(inner)) as a:
            names = {m.name.removeprefix("./"): m for m in a.getmembers()}
            assert names["fncompress"].mode & 0o777 == 0o755
            assert names["ui/config"].isfile()
            assert f"runtime/{RUNTIME_ZIP}" in names
            assert all(n in names for n, _, _ in legal)
    print(f"{out}  {len(fpk) / 1e6:.1f} MB  sha256 {digest}")


if __name__ == "__main__":
    main()
