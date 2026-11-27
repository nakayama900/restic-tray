#!/usr/bin/env python3
"""
Deterministic generator for THIRD_PARTY_NOTICES.md.

Uses `cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc --locked`
and reads actual license/notice texts directly from the Cargo registry cache for every
normal runtime crate linked into `restic-tray.exe`, alongside the full BSD 2-Clause license
for the bundled `restic.exe` binary.

Uses only the Python standard library. Run with `--check` to verify THIRD_PARTY_NOTICES.md
is up to date without modifying it.
"""

from __future__ import annotations

import argparse
import collections
import json
import pathlib
import re
import subprocess
import sys

TARGET_TRIPLE = "x86_64-pc-windows-msvc"

RESTIC_LICENSE_TEXT = """\
BSD 2-Clause License

Copyright (c) 2014, Alexander Neumann <alexander@bumpern.de>
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE."""


def read_restic_version(repo_root: pathlib.Path) -> str:
    script = (repo_root / "scripts" / "download-restic.ps1").read_text(encoding="utf-8")
    m = re.search(r'\[string\]\$Version\s*=\s*"([^"]+)"', script)
    return m.group(1) if m else "0.17.3"


def is_proc_macro_only(pkg: dict) -> bool:
    lib_kinds = {
        k
        for t in pkg.get("targets", [])
        for k in t.get("kind", [])
        if k in ("lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro")
    }
    return lib_kinds == {"proc-macro"}


def resolve_runtime_crates(repo_root: pathlib.Path) -> list[dict]:
    raw = subprocess.check_output(
        [
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            TARGET_TRIPLE,
            "--locked",
        ],
        cwd=repo_root,
        text=True,
        encoding="utf-8",
    )
    meta = json.loads(raw)
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]

    visited: set[str] = set()
    queue: collections.deque[str] = collections.deque([root])
    while queue:
        cur = queue.popleft()
        if cur in visited:
            continue
        visited.add(cur)
        for dep in nodes[cur]["deps"]:
            if any(dk.get("kind") is None for dk in dep.get("dep_kinds", [])):
                dep_id = dep["pkg"]
                # Skip proc-macro crates and their macro-only dependencies (not linked into final exe).
                if is_proc_macro_only(pkgs[dep_id]):
                    continue
                queue.append(dep_id)

    visited.discard(root)
    result = [pkgs[pid] for pid in visited]
    result.sort(key=lambda p: (p["name"].lower(), p["version"]))
    return result


def normalize_text(text: str) -> str:
    lines = [line.rstrip() for line in text.replace("\r\n", "\n").replace("\r", "\n").split("\n")]
    return "\n".join(lines).strip()


def select_license_files(pkg: dict) -> list[tuple[str, str]]:
    """
    Returns a sorted list of (relative_path, normalized_text) for the license and notice files
    required to satisfy redistribution of `pkg` under our elected permissive license(s).
    """
    pdir = pathlib.Path(pkg["manifest_path"]).parent
    name = pkg["name"]
    spdx = (pkg.get("license") or "").strip()

    def read_rel(path: pathlib.Path) -> tuple[str, str]:
        rel = path.relative_to(pdir).as_posix()
        txt = normalize_text(path.read_text(encoding="utf-8", errors="replace"))
        return (rel, txt)

    root_files = sorted(
        [f for f in pdir.iterdir() if f.is_file()],
        key=lambda f: f.name.lower(),
    )

    def find_by_prefixes(prefixes: tuple[str, ...]) -> list[pathlib.Path]:
        return [
            f
            for f in root_files
            if any(f.name.upper().startswith(pre) for pre in prefixes)
        ]

    selected: list[pathlib.Path] = []

    # 1. Explicit license_file declared in Cargo.toml
    lic_file = pkg.get("license_file")
    if lic_file:
        candidate = pdir / lic_file
        if candidate.is_file():
            selected.append(candidate)

    # 2. Bundled font licenses in epaint_default_fonts
    if name == "epaint_default_fonts":
        fonts_dir = pdir / "fonts"
        if fonts_dir.is_dir():
            for f in sorted(fonts_dir.glob("*.txt"), key=lambda x: x.name.lower()):
                selected.append(f)

    # 3. Any NOTICE or COPYRIGHT files in crate root must always be preserved
    for f in root_files:
        upper = f.name.upper()
        if "NOTICE" in upper or upper.startswith("COPYRIGHT"):
            if f not in selected:
                selected.append(f)

    # 4. Collect root LICENSE / LICENCE / COPYING / UNLICENSE files
    lic_candidates = [
        f
        for f in find_by_prefixes(("LICENSE", "LICENCE", "COPYING", "UNLICENSE"))
        if "GPL" not in f.name.upper()
    ]

    if "AND" in spdx:
        # Compound license (e.g. `(MIT OR Apache-2.0) AND Unicode-3.0`, `Apache-2.0 AND MIT`):
        # keep all non-GPL license files needed by every required clause.
        has_mit = any("MIT" in f.name.upper() for f in lic_candidates)
        has_apache = any("APACHE" in f.name.upper() for f in lic_candidates)
        for f in lic_candidates:
            upper = f.name.upper()
            # If the crate is `(MIT OR Apache-2.0) AND X`, we elect MIT for the OR clause
            # while preserving X (e.g. LICENSE-UNICODE).
            if (
                "(MIT OR APACHE-2.0)" in spdx.upper()
                or "(APACHE-2.0 OR MIT)" in spdx.upper()
            ) and has_mit and has_apache and "APACHE" in upper:
                continue
            if f not in selected:
                selected.append(f)
    elif lic_candidates:
        # Single or OR-choice license: elect one permissive license file consistently
        # (prefer MIT -> BSD -> ISC -> Zlib -> Apache-2.0 -> 0BSD -> Unlicense -> generic LICENSE).
        priority = (
            "LICENSE-MIT",
            "LICENCE-MIT",
            "LICENSE-BSD",
            "LICENSE-ISC",
            "LICENSE-ZLIB",
            "LICENSE-APACHE",
            "LICENCE-APACHE",
            "LICENSE-0BSD",
            "UNLICENSE",
            "LICENSE-UNLICENSE",
        )
        chosen = None
        for pref in priority:
            for f in lic_candidates:
                if f.name.upper().startswith(pref):
                    chosen = f
                    break
            if chosen is not None:
                break
        if chosen is None:
            chosen = lic_candidates[0]
        if chosen not in selected:
            selected.append(chosen)

    # Deduplicate preserving order
    seen: set[pathlib.Path] = set()
    unique: list[tuple[str, str]] = []
    for f in selected:
        if f not in seen:
            seen.add(f)
            rel, txt = read_rel(f)
            if txt:
                unique.append((rel, txt))
    return unique


def summarize_license_heading(text: str, crates: list[tuple[str, str, str]]) -> str:
    first_lines = [line.strip() for line in text.splitlines() if line.strip()][:3]
    head = " ".join(first_lines).lower()
    if "sil open font license" in head:
        return "SIL Open Font License 1.1 (OFL-1.1)"
    if "ubuntu font licence" in head:
        return "Ubuntu Font Licence 1.0 (Ubuntu-font-1.0)"
    if "community data license agreement" in head:
        return "Community Data License Agreement - Permissive 2.0 (CDLA-Permissive-2.0)"
    if "unicode, inc. license agreement" in head or "unicode license v3" in head:
        return "Unicode License v3 (Unicode-3.0)"
    if "apache license" in head and "version 2.0" in head:
        return "Apache License 2.0"
    if "boost software license" in head:
        return "Boost Software License 1.0 (BSL-1.0)"
    if "mit license" in head or "permission is hereby granted, free of charge" in text.lower()[:400]:
        return "MIT License"
    if "redistribution and use in source and binary forms" in text.lower()[:600]:
        if "neither the name of" in text.lower() or "names of its contributors" in text.lower():
            return "BSD 3-Clause License"
        return "BSD 2-Clause License"
    if "isc license" in head or "permission to use, copy, modify, and/or distribute" in text.lower()[:400]:
        return "ISC License"
    if "zlib" in head or "altered source versions must be plainly marked" in text.lower():
        return "Zlib License"
    if "zero-clause bsd" in head or "0bsd" in head:
        return "BSD Zero Clause License (0BSD)"
    if "unlicense" in head:
        return "The Unlicense"
    if "egui-icons.ttf" in head:
        return "egui-icons.ttf Notice (`epaint_default_fonts`)"
    if "hack project" in head:
        return "Hack / DejaVu / Bitstream Vera Font License (`epaint_default_fonts`)"
    spdx_set = sorted({spdx for _, _, spdx in crates if spdx})
    return " / ".join(spdx_set) if spdx_set else "Third-Party License / Notice"


def render_notices(repo_root: pathlib.Path) -> str:
    restic_version = read_restic_version(repo_root)
    crates = resolve_runtime_crates(repo_root)

    # Group identical license/notice texts while keeping exact copyright headers intact.
    # Map normalized_text -> list of (crate_name, version, spdx, rel_file)
    text_groups: dict[str, list[tuple[str, str, str, str]]] = collections.OrderedDict()
    no_file_crates: list[dict] = []

    for pkg in crates:
        spdx = pkg.get("license") or "See crate manifest"
        files = select_license_files(pkg)
        if not files:
            no_file_crates.append(pkg)
            continue
        for rel, txt in files:
            text_groups.setdefault(txt, []).append((pkg["name"], pkg["version"], spdx, rel))

    # Sort groups deterministically: larger groups first, then first crate name, then text.
    ordered_groups = sorted(
        text_groups.items(),
        key=lambda kv: (-len(kv[1]), kv[1][0][0].lower(), kv[1][0][1], kv[1][0][3].lower(), kv[0]),
    )

    out: list[str] = []
    out.append("# Third-Party Notices")
    out.append("")
    out.append(
        "This application (`Restic Tray` / `restic-tray`) bundles or statically links the "
        "following third-party open-source software components."
    )
    out.append("")
    out.append(
        "> **Reproducibility**: The Rust crate section below is generated deterministically from "
        f"`Cargo.lock` for target `{TARGET_TRIPLE}` by running:"
    )
    out.append(">")
    out.append("> ```powershell")
    out.append("> python scripts/generate-third-party-notices.py")
    out.append("> ```")
    out.append("")

    # Section 1: Bundled Restic binary
    out.append("## 1. Bundled Binary: Restic")
    out.append("")
    out.append(f"- **Component**: `restic.exe` (v{restic_version}, Windows amd64 official release)")
    out.append("- **Project**: [restic/restic](https://github.com/restic/restic)")
    out.append("- **License**: BSD-2-Clause")
    out.append(
        "- **Note**: `restic.exe` is redistributed unmodified from the official upstream GitHub "
        "Release (SHA-256 verified by `scripts/download-restic.ps1`). Go module dependencies "
        "statically compiled inside `restic.exe` are governed by upstream `restic/restic`."
    )
    out.append("")
    out.append("```text")
    out.append(RESTIC_LICENSE_TEXT)
    out.append("```")
    out.append("")

    # Section 2: Bundled SQLite note
    out.append("## 2. Bundled C Library: SQLite")
    out.append("")
    out.append(
        "- **Component**: SQLite (statically compiled via `rusqlite` / `libsqlite3-sys` `bundled` feature)"
    )
    out.append("- **Project**: [sqlite.org](https://www.sqlite.org/)")
    out.append(
        "- **License**: Public Domain ([SQLite Copyright](https://www.sqlite.org/copyright.html))"
    )
    out.append("")

    # Section 3: Summary table of all linked Rust crates
    out.append(f"## 3. Statically Linked Rust Crates ({len(crates)} crates, `{TARGET_TRIPLE}`)")
    out.append("")
    out.append(
        "For dual- or multi-licensed crates (`OR` expressions), this distribution elects a "
        "permissive license allowed by `deny.toml` (such as `MIT`, `Apache-2.0`, `BSD-*`, "
        "`ISC`, `Zlib`, `0BSD`, `CC0-1.0`, `MIT-0`, `BSL-1.0`, or `Unicode-3.0`)."
    )
    out.append("")
    out.append("| Crate | Version | Declared SPDX License | Source |")
    out.append("| :--- | :--- | :--- | :--- |")
    for pkg in crates:
        name = pkg["name"]
        ver = pkg["version"]
        spdx = (pkg.get("license") or "See crate manifest").replace("|", "\\|")
        repo = pkg.get("repository") or pkg.get("homepage") or f"https://crates.io/crates/{name}"
        out.append(f"| `{name}` | `{ver}` | `{spdx}` | <{repo}> |")
    out.append("")

    # Crates that do not package a separate LICENSE file inside their published crates.io tarball
    if no_file_crates:
        out.append("### Crates Without a Packaged License File in Their Crate Archive")
        out.append("")
        out.append(
            "The following workspace-published crates declare their SPDX license in `Cargo.toml` "
            "without bundling a standalone `LICENSE` file in the crate archive. Their applicable "
            "license terms (`MIT`, `Apache-2.0`, `BSL-1.0`) appear in full in Section 4 below."
        )
        out.append("")
        for pkg in no_file_crates:
            authors = ", ".join(pkg.get("authors") or [])
            author_note = f" — Authors: {authors}" if authors else ""
            out.append(
                f"- `{pkg['name']} {pkg['version']}` (`{pkg.get('license')}`){author_note}"
            )
        out.append("")

    # Section 4: Deduplicated full license & copyright texts
    out.append("## 4. Full License Texts and Copyright Notices")
    out.append("")
    for idx, (text, entries) in enumerate(ordered_groups, 1):
        heading = summarize_license_heading(
            text, [(n, v, s) for n, v, s, _ in entries]
        )
        out.append(f"### 4.{idx} {heading}")
        out.append("")
        out.append("Applies to:")
        out.append("")
        for cname, cver, cspdx, crel in sorted(entries, key=lambda e: (e[0].lower(), e[1], e[3])):
            out.append(f"- `{cname} {cver}` (`{cspdx}`, file: `{crel}`)")
        out.append("")
        fence = "~~~~text" if "```" in text else "```text"
        close = "~~~~" if "```" in text else "```"
        out.append(fence)
        out.append(text)
        out.append(close)
        out.append("")

    return "\n".join(out).rstrip() + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate THIRD_PARTY_NOTICES.md from Cargo.lock")
    parser.add_argument(
        "--check",
        action="store_true",
        help="Verify THIRD_PARTY_NOTICES.md matches generated output without writing",
    )
    args = parser.parse_args()

    repo_root = pathlib.Path(__file__).resolve().parent.parent
    notices_path = repo_root / "THIRD_PARTY_NOTICES.md"
    rendered = render_notices(repo_root)

    if args.check:
        current = notices_path.read_text(encoding="utf-8") if notices_path.exists() else ""
        if current.replace("\r\n", "\n") != rendered:
            print(
                "THIRD_PARTY_NOTICES.md is out of date. Run: python scripts/generate-third-party-notices.py",
                file=sys.stderr,
            )
            return 1
        print("THIRD_PARTY_NOTICES.md is up to date.")
        return 0

    notices_path.write_text(rendered, encoding="utf-8", newline="\n")
    print(f"Wrote {notices_path} ({len(rendered.encode('utf-8'))} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
