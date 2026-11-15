#!/usr/bin/env python3
"""Capture the real desktop renderer with synthetic examples and render PNGs.

Requires Rust/Cargo, rsvg-convert (librsvg), and a monospace font. This does not
connect to a running Cepheus service or read saved conversations.
"""
from __future__ import annotations

import argparse
import html
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def plain_path(path: Path) -> None:
    """Reject symlinks in either a file or any containing directory."""
    for candidate in (path, *path.parents):
        if candidate.is_symlink():
            raise ValueError(f"Screenshot paths must not be symlinks: {candidate}")


def basename(value: object, suffix: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*" + re.escape(suffix), value):
        raise ValueError(f"Expected a safe {suffix} basename, got {value!r}")
    return value


def load(source: Path, *, svg: bool = False) -> dict:
    plain_path(source)
    manifest_path = source / "manifest.json"
    plain_path(manifest_path)
    if not source.is_dir() or not manifest_path.is_file() or manifest_path.stat().st_size > 1 << 20:
        raise ValueError("Expected a screenshot directory with a bounded manifest.json")
    manifest = json.loads(manifest_path.read_text())
    screens = manifest.get("screens")
    if manifest.get("schema") != 1 or not isinstance(screens, list) or not 1 <= len(screens) <= 500:
        raise ValueError("Expected a schema 1 manifest containing 1–500 screens")
    seen = set()
    for screen in screens:
        if not isinstance(screen, dict):
            raise ValueError("Every screenshot manifest entry must be an object")
        name = basename(screen.get("image"), ".png")
        if name in seen:
            raise ValueError(f"Duplicate screenshot name: {name}")
        seen.add(name)
        title = screen.get("title")
        if not isinstance(title, str) or not title or len(title) > 200 or any(ord(c) < 32 for c in title):
            raise ValueError("Every screenshot needs a short, single-line title")
        path = source / basename(screen.get("svg"), ".svg") if svg else source / name
        plain_path(path)
        if not path.exists() or not stat.S_ISREG(path.stat().st_mode):
            raise ValueError(f"Screenshot input is not a regular file: {path}")
    return manifest


def output_directory(folder: str, manifest: dict) -> Path:
    destination = ROOT / "docs/screenshots" / folder
    plain_path(destination)
    # Check the complete output set before creating or replacing any file.
    for path in [destination / "manifest.json", *(destination / screen["image"] for screen in manifest["screens"]),
                 ROOT / "docs/screenshots/README.md", ROOT / "docs/screenshots" / ("DESKTOP.md" if folder == "desktop" else "ANDROID.md")]:
        plain_path(path)
        if path.exists() and not path.is_file():
            raise ValueError(f"Screenshot output is not a regular file: {path}")
    destination.mkdir(parents=True, exist_ok=True)
    return destination


def render(source: Path) -> None:
    manifest = load(source, svg=True)
    destination = output_directory("desktop", manifest)
    for screen in manifest["screens"]:
        subprocess.run(["rsvg-convert", "-o", str(destination / screen["image"]), str(source / screen["svg"])], check=True)
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    gallery = ["# Desktop screenshots", "", "Captured from Cepheus's production terminal renderer with synthetic project data. No owner sessions, accounts or credentials appear. The pairing code is inert.", "", "Regenerate with `python3 tools/capture_screenshots.py`. A fixed 144 × 44 terminal and neutral dark palette keep the gallery reproducible.", ""]
    for screen in manifest["screens"]:
        gallery.extend([f"## {screen['title']}", "", f"![{screen['title']}](desktop/{screen['image']})", ""])
    (ROOT / "docs/screenshots/DESKTOP.md").write_text("\n".join(gallery))
    index()
    print(f"Rendered {len(manifest['screens'])} desktop screenshots in {destination}")


def android(source: Path) -> None:
    manifest = load(source)
    destination = output_directory("android", manifest)
    gallery = ["# Android screenshots", "", "Captured from the real Android APK on a disposable emulator connected to an isolated service with synthetic data. The gallery contains no owner sessions or credentials.", "", "Regenerate with `tools/capture_android_screenshots.py` and import with `python3 tools/capture_screenshots.py --android CAPTURE_DIRECTORY`.", ""]
    for screen in manifest["screens"]:
        shutil.copyfile(source / screen["image"], destination / screen["image"])
        gallery.extend([f"## {screen['title']}", "", f"<img src=\"android/{screen['image']}\" alt=\"{html.escape(screen['title'], quote=True)}\" width=\"360\">", ""])
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (ROOT / "docs/screenshots/ANDROID.md").write_text("\n".join(gallery))
    index()
    print(f"Imported {len(manifest['screens'])} Android screenshots in {destination}")


def index() -> None:
    lines = ["# Screenshots", "", "These captures show Cepheus's real desktop renderer and Android APK using synthetic example data.", ""]
    for label, folder, gallery in [("Desktop", "desktop", "DESKTOP.md"), ("Android", "android", "ANDROID.md")]:
        manifest = ROOT / "docs/screenshots" / folder / "manifest.json"
        plain_path(manifest)
        if manifest.exists():
            count = len(json.loads(manifest.read_text())["screens"])
            lines.append(f"- [{label}: {count} screens]({gallery})")
    lines.extend(["", "The machine names, project paths, conversations, usage and task metadata are examples. Pairing screenshots use inert data or an empty form.", ""])
    output = ROOT / "docs/screenshots/README.md"
    plain_path(output)
    output.write_text("\n".join(lines))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--render-only", type=Path, help="render a previously captured SVG/manifest directory")
    parser.add_argument("--android", type=Path, help="import an Android capture directory and its manifest")
    args = parser.parse_args()
    if args.android:
        android(Path(os.path.abspath(args.android)))
        return
    if not shutil.which("rsvg-convert"):
        parser.error("rsvg-convert is required (install librsvg)")
    if args.render_only:
        render(Path(os.path.abspath(args.render_only)))
        return
    with tempfile.TemporaryDirectory(prefix="cepheus-screenshots-") as temporary:
        env = dict(os.environ, CEPHEUS_SCREENSHOTS_DIR=temporary)
        subprocess.run(["cargo", "test", "--release", "--locked", "view::screenshots::capture_all_screens", "--", "--ignored", "--exact", "--nocapture"], cwd=ROOT, env=env, check=True)
        render(Path(temporary))


if __name__ == "__main__":
    main()
