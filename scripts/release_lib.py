r"""Release helpers for scripts/build-plugin.sh, scripts/release.sh and scripts/publish-github.sh.

Standard library only; run with `python -I scripts/release_lib.py <command> ...`.

Commands
  plugin-version <main.php>              print the plugin version (header, SFC_VERSION and readme Stable tag must agree)
  zip <src_dir> <zip> <root_name>         deterministic zip of src_dir under root_name/, plus <zip>.sha256
  sha256 <file>                           write <file>.sha256 ("<hex>  <name>")
  sums <dir>                              write <dir>/SHA256SUMS.txt over every file (relative, forward slashes)
  notices <version> <out> [license_dir]   write THIRD_PARTY_NOTICES.txt (+ licenses/ next to it when found)
  notes <changelog> <version>             print the CHANGELOG.md section of <version> (exit 1 when missing)
  mirror <src_dir> <dst_dir>              make dst_dir match src_dir, keeping dst_dir/.git
  check-domains <dir> <db>                fail when a real site domain/brand from the live DB (read-only) occurs in dir
"""
import hashlib
import os
import re
import shutil
import sqlite3
import sys
import zipfile

# Fixed timestamp so the same tree always gives the same zip bytes.
ZIP_DATE = (2026, 1, 1, 0, 0, 0)
SKIP_NAMES = {".DS_Store", "Thumbs.db", "desktop.ini"}


def die(msg):
    print(f"release_lib: {msg}", file=sys.stderr)
    sys.exit(1)


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def write_sha(path):
    digest = sha256_file(path)
    with open(path + ".sha256", "w", encoding="utf-8", newline="\n") as f:
        f.write(f"{digest}  {os.path.basename(path)}\n")
    return digest


def walk_files(root):
    out = []
    for base, dirs, files in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d != ".git")
        for name in sorted(files):
            if name in SKIP_NAMES:
                continue
            full = os.path.join(base, name)
            out.append((os.path.relpath(full, root).replace(os.sep, "/"), full))
    return sorted(out)


def cmd_plugin_version(main_php):
    text = open(main_php, encoding="utf-8").read()
    m = re.search(r"^\s*\*\s*Version:\s*([0-9][0-9A-Za-z.\-+]*)\s*$", text, re.M)
    if not m:
        die(f"no 'Version:' header in {main_php}")
    ver = m.group(1)
    c = re.search(r"define\(\s*'SFC_VERSION'\s*,\s*'([^']+)'\s*\)", text)
    if not c or c.group(1) != ver:
        die(f"SFC_VERSION ({c.group(1) if c else 'missing'}) != header Version {ver}")
    readme = os.path.join(os.path.dirname(main_php), "readme.txt")
    if os.path.exists(readme):
        s = re.search(r"^Stable tag:\s*(\S+)", open(readme, encoding="utf-8").read(), re.M)
        if not s or s.group(1) != ver:
            die(f"readme.txt Stable tag ({s.group(1) if s else 'missing'}) != header Version {ver}")
    print(ver)


def cmd_zip(src, zip_path, root_name):
    if not os.path.isdir(src):
        die(f"not a directory: {src}")
    os.makedirs(os.path.dirname(os.path.abspath(zip_path)), exist_ok=True)
    tmp = zip_path + ".tmp"
    files = walk_files(src)
    if not files:
        die(f"nothing to zip in {src}")
    with zipfile.ZipFile(tmp, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        dirs = set()
        for rel, _ in files:
            parts = rel.split("/")[:-1]
            for i in range(len(parts)):
                dirs.add("/".join(parts[: i + 1]))
        for d in [""] + sorted(dirs):
            name = f"{root_name}/{d}/" if d else f"{root_name}/"
            info = zipfile.ZipInfo(name.replace("//", "/"), ZIP_DATE)
            info.external_attr = (0o40755 << 16) | 0x10
            z.writestr(info, b"")
        for rel, full in files:
            info = zipfile.ZipInfo(f"{root_name}/{rel}", ZIP_DATE)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            with open(full, "rb") as f:
                z.writestr(info, f.read())
    os.replace(tmp, zip_path)
    digest = write_sha(zip_path)
    size = os.path.getsize(zip_path)
    print(f"{zip_path}  {len(files)} files  {size} bytes  sha256 {digest}")


def cmd_sha256(path):
    print(f"{write_sha(path)}  {os.path.basename(path)}")


def cmd_sums(root):
    lines = [f"{sha256_file(full)}  {rel}" for rel, full in walk_files(root) if rel != "SHA256SUMS.txt"]
    with open(os.path.join(root, "SHA256SUMS.txt"), "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines) + "\n")
    print(f"SHA256SUMS.txt: {len(lines)} files")


def cmd_notices(version, out, license_dir=r"D:\Download\onnxruntime\x"):
    lic_out = os.path.join(os.path.dirname(os.path.abspath(out)), "licenses")
    wanted = [
        ("microsoft.ml.onnxruntime.directml-1.24.4/LICENSE", "onnxruntime-LICENSE.txt"),
        ("microsoft.ml.onnxruntime.directml-1.24.4/ThirdPartyNotices.txt", "onnxruntime-ThirdPartyNotices.txt"),
        ("microsoft.ai.directml-1.15.4/LICENSE.txt", "DirectML-LICENSE.txt"),
        ("microsoft.ai.directml-1.15.4/ThirdPartyNotices.txt", "DirectML-ThirdPartyNotices.txt"),
    ]
    found = 0
    for rel, name in wanted:
        src = os.path.join(license_dir, *rel.split("/"))
        if os.path.exists(src):
            os.makedirs(lic_out, exist_ok=True)
            shutil.copyfile(src, os.path.join(lic_out, name))
            found += 1
    text = f"""SiloForge {version} - third-party notices

SiloForge itself is MIT licensed (see LICENSE). This package also contains:

1. ONNX Runtime 1.24.4 (onnxruntime.dll, onnxruntime_providers_shared.dll)
   Copyright (c) Microsoft Corporation. MIT License.
   https://github.com/microsoft/onnxruntime  -  full text: licenses\\onnxruntime-LICENSE.txt,
   its own third-party notices: licenses\\onnxruntime-ThirdPartyNotices.txt

2. DirectML 1.15.4 (DirectML.dll)
   Copyright (c) Microsoft Corporation. Microsoft Software License Terms for DirectX Machine Learning
   (redistributable with applications; Windows only). https://www.nuget.org/packages/Microsoft.AI.DirectML
   https://github.com/microsoft/DirectML  -  full text: licenses\\DirectML-LICENSE.txt,
   notices: licenses\\DirectML-ThirdPartyNotices.txt

3. Embedded web UI: React, React Router, React Flow (MIT) - see ui/package.json in the source.

4. Rust crates compiled into siloforge.exe (axum, tokio, rusqlite + bundled SQLite (public domain),
   rmcp, ort, tokenizers, rayon, reqwest + rustls/ring, jsonwebtoken, bcrypt, aes-gcm, scraper, sysinfo,
   croner, zip, clap, tracing, serde, ...) are MIT and/or Apache-2.0 (ring: ISC-style + OpenSSL-derived).
   The exact list with versions is Cargo.lock in the source repository.

5. The embedding model all-MiniLM-L6-v2 (sentence-transformers, Apache-2.0; ONNX export by Xenova)
   is not bundled; 'siloforge fetch-model' places it in <home>\\models.

6. SiloForge Connect (the WordPress plugin, GPLv2 or later) ships as its own zip next to this one.
"""
    if found < len(wanted):
        text += f"\n(Some license files were not found under {license_dir} at build time; see the URLs above.)\n"
    with open(out, "w", encoding="utf-8", newline="\r\n") as f:
        f.write(text)
    print(f"THIRD_PARTY_NOTICES.txt written ({found}/{len(wanted)} license files)")


def cmd_notes(changelog, version):
    lines = open(changelog, encoding="utf-8").read().splitlines()
    head = re.compile(r"^## \[?" + re.escape(version) + r"\]?(\s|$)")
    start = next((i for i, l in enumerate(lines) if head.match(l)), None)
    if start is None:
        die(f"no '## [{version}]' section in {changelog}")
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")), len(lines))
    body = "\n".join(lines[start + 1 : end]).strip()
    if not body:
        die(f"empty CHANGELOG section for {version}")
    print(body)


def cmd_mirror(src, dst):
    if not os.path.isdir(src):
        die(f"not a directory: {src}")
    if not os.path.isdir(os.path.join(dst, ".git")):
        die(f"{dst} is not a git clone (refusing to mirror into it)")
    want = dict(walk_files(src))
    have = dict(walk_files(dst))
    added = changed = removed = 0
    for rel, full in want.items():
        target = os.path.join(dst, *rel.split("/"))
        if rel in have:
            if open(full, "rb").read() == open(have[rel], "rb").read():
                continue
            changed += 1
        else:
            added += 1
        os.makedirs(os.path.dirname(target), exist_ok=True)
        shutil.copyfile(full, target)
    for rel, full in have.items():
        if rel not in want:
            os.remove(full)
            removed += 1
    for base, dirs, files in os.walk(dst, topdown=False):
        if ".git" in base.replace(os.sep, "/").split("/"):
            continue
        if base != dst and not os.listdir(base):
            os.rmdir(base)
    print(f"mirror: {added} added, {changed} changed, {removed} removed")


def load_names(db):
    if not os.path.exists(db):
        die(f"live DB not found: {db} (needed for the domain check)")
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    names = set()
    try:
        for (d,) in con.execute("select domain from sites"):
            if not d:
                continue
            d = d.strip().lower()
            d = re.sub(r"^https?://", "", d).split("/")[0].split(":")[0]
            if d.startswith("www."):
                d = d[4:]
            if "." in d and d not in ("localhost",):
                names.add(d)
                brand = d.split(".")[0]
                if len(brand) >= 5 and brand not in ("host", "local", "example"):
                    names.add(brand)
    finally:
        con.close()
    # Hosts that are not real public sites.
    for skip in ("site01.example", "localhost", "127.0.0.1"):
        names.discard(skip)
    return names


def cmd_check_domains(root, db):
    names = load_names(db)
    if not names:
        print("check-domains: no site domains in the DB; nothing to check")
        return
    rx = re.compile("|".join(re.escape(n) for n in sorted(names, key=len, reverse=True)), re.I)
    hits = {}
    for rel, full in walk_files(root):
        try:
            text = open(full, encoding="utf-8").read()
        except (UnicodeDecodeError, OSError):
            continue
        n = len(rx.findall(text))
        if n:
            hits[rel] = n
    if hits:
        # File names and counts only: the matched names stay private.
        for rel, n in sorted(hits.items()):
            print(f"  LEAK {rel}: {n} match(es)", file=sys.stderr)
        die(f"check-domains: real site names found in {len(hits)} file(s) under {root}")
    print(f"check-domains: clean ({len(names)} names checked)")


def main(argv):
    # Windows consoles default to cp1252; CHANGELOG text is UTF-8.
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8")
        except (AttributeError, ValueError):
            pass
    if len(argv) < 2:
        print(__doc__)
        sys.exit(2)
    cmd, args = argv[1], argv[2:]
    table = {
        "plugin-version": (cmd_plugin_version, 1, 1),
        "zip": (cmd_zip, 3, 3),
        "sha256": (cmd_sha256, 1, 1),
        "sums": (cmd_sums, 1, 1),
        "notices": (cmd_notices, 2, 3),
        "notes": (cmd_notes, 2, 2),
        "mirror": (cmd_mirror, 2, 2),
        "check-domains": (cmd_check_domains, 2, 2),
    }
    if cmd not in table:
        die(f"unknown command {cmd!r}")
    fn, lo, hi = table[cmd]
    if not lo <= len(args) <= hi:
        die(f"{cmd}: expected {lo}..{hi} arguments, got {len(args)}")
    fn(*args)


if __name__ == "__main__":
    main(sys.argv)
