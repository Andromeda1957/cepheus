#!/usr/bin/env python3
"""Capture the real desktop renderer with synthetic examples and render PNGs.

Requires Foot, Hyprland, grim with toplevel capture, and JetBrainsMono Nerd Font.
Rust/Cargo is needed only to regenerate the production ANSI buffers. This does not
connect to a running Cepheus service or read saved conversations.
"""
from __future__ import annotations

import argparse
import html
import json
import math
import os
from pathlib import Path
import re
import select
import shlex
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import time
import tty
import uuid

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


def load(source: Path, *, ansi: bool = False) -> dict:
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
        path = source / basename(screen.get("ansi"), ".ansi") if ansi else source / name
        plain_path(path)
        if not path.exists() or not stat.S_ISREG(path.stat().st_mode):
            raise ValueError(f"Screenshot input is not a regular file: {path}")
        if ansi and (path.stat().st_size > 8 << 20 or b"\x1b]" in path.read_bytes()):
            raise ValueError("Expected a bounded terminal frame without OSC side effects")
        if ansi and any(type(screen.get(key)) is not int or not 20 <= screen[key] <= 300 for key in ("columns", "rows")):
            raise ValueError("Expected bounded terminal columns and rows")
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


def hypr(*arguments: str):
    result = subprocess.run(["hyprctl", "-j", *arguments], capture_output=True, text=True, check=True, timeout=10)
    return json.loads(result.stdout)


def window_action(app_id: str, action: str) -> None:
    command = ("for _, w in ipairs(hl.get_windows()) do if w.class == " + json.dumps(app_id) +
               " then hl.dispatch(hl.dsp.window." + action + ") end end")
    subprocess.run(["hyprctl", "eval", command], check=True, capture_output=True, text=True, timeout=10)


def replay(ansi: Path, ready: Path, columns: int, rows: int) -> None:
    """The Foot child: replay a production frame once its exact viewport fits."""
    tty.setraw(sys.stdin.fileno())
    os.write(sys.stdout.fileno(), b"\x1b[16t")
    response = b""
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        if select.select([sys.stdin], [], [], 0.1)[0]:
            response += os.read(sys.stdin.fileno(), 1024)
            if b"t" in response:
                break
    cell = re.search(rb"\x1b\[6;(\d+);(\d+)t", response)
    if cell is None:
        raise RuntimeError("Foot did not report its native cell dimensions")
    height, width = map(int, cell.groups())
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        size = os.get_terminal_size()
        fits = (size.columns, size.lines) == (columns, rows)
        if fits:
            sys.stdout.buffer.write(ansi.read_bytes())
            sys.stdout.buffer.flush()
            time.sleep(0.25)
        ready.write_text(json.dumps({"ready": fits, "columns": size.columns, "rows": size.lines,
                                     "cell_width": width, "cell_height": height}))
        if fits:
            # The parent owns this Foot process and closes it after screencopy.
            select.select([sys.stdin], [], [], 90)
            return
        time.sleep(0.1)
    raise RuntimeError("The terminal never reached the requested native viewport")


def terminal_config(manifest: dict) -> str:
    terminal = manifest["terminal"]
    def rgb(values) -> str:
        if not isinstance(values, list) or len(values) != 3 or any(type(n) is not int or not 0 <= n <= 255 for n in values):
            raise ValueError("Invalid capture palette")
        return "".join(f"{n:02x}" for n in values)
    colors = terminal["palette"]
    if not isinstance(colors, list) or len(colors) != 16:
        raise ValueError("Expected the terminal's 16 ANSI colors")
    lines = ["[main]", "font=JetBrainsMono Nerd Font:size=12", "pad=14x14", "term=xterm-256color", "workers=0",
             "[cursor]", "blink=no", "[colors-dark]", f"foreground={rgb(terminal['foreground'])}",
             f"background={rgb(terminal['background'])}"]
    for number, color in enumerate(colors):
        lines.append(f"{'regular' if number < 8 else 'bright'}{number % 8}={rgb(color)}")
    return "\n".join(lines) + "\n"


def native_capture(source: Path, screen: dict, destination: Path, config: Path, scratch: Path, workspace: int) -> tuple[int, int]:
    """Capture only our dedicated Foot surface, without displaying its workspace."""
    app_id = "cepheus-capture-" + uuid.uuid4().hex
    before = (hypr("activeworkspace")["id"], hypr("activewindow").get("address"))
    if before[0] == workspace or any(c["workspace"]["id"] == workspace for c in hypr("clients")):
        raise RuntimeError("Use an empty, inactive capture workspace")
    ready = scratch / (app_id + ".json")
    log = scratch / (app_id + ".log")
    command = shlex.join([
        "foot", "--config", str(config), "--app-id", app_id, "--title", "Cepheus documentation capture",
        "--window-size-chars", f"{screen['columns']}x{screen['rows']}",
        sys.executable, str(Path(__file__).resolve()), "_replay", str(source / screen["ansi"]), str(ready),
        str(screen["columns"]), str(screen["rows"]),
    ]) + " > /dev/null 2> " + shlex.quote(str(log))
    # Per-launch rules apply before the window maps and expire with this launch.
    # Routing after map can briefly expose the test window or steal focus.
    rules = ('{ workspace=' + json.dumps(f"{workspace} silent") +
             ', float=true, render_unfocused=true, no_initial_focus=true, suppress_event="activate activatefocus" }')
    subprocess.run(["hyprctl", "eval", "hl.exec_cmd(" + json.dumps(command) + ", " + rules + ")"],
                   check=True, capture_output=True, text=True, timeout=10)
    client = None
    try:
        deadline = time.monotonic() + 20
        last_grid = None
        resize_count = 0
        state = None
        while time.monotonic() < deadline:
            client = next((c for c in hypr("clients") if c.get("class") == app_id), None)
            if client is None:
                time.sleep(0.1)
                continue
            if client["workspace"]["id"] != workspace:
                raise RuntimeError(f"The dedicated terminal mapped to workspace {client['workspace']['id']} instead of {workspace}")
            try:
                state = json.loads(ready.read_text())
            except (FileNotFoundError, ValueError):
                time.sleep(0.1)
                continue
            if state["ready"]:
                time.sleep(0.5)
                break
            grid = (state["columns"], state["rows"])
            if grid != last_grid and resize_count < 5:
                monitor = next(m for m in hypr("monitors") if m["id"] == client["monitor"])
                # Foot's cell query uses its integer Wayland buffer scale,
                # including on a fractionally scaled monitor. Adjust the
                # existing native surface by whole cells so padding stays exact.
                scale = math.ceil(monitor["scale"])
                width = round(client["size"][0] + (screen["columns"] - state["columns"]) * state["cell_width"] / scale)
                height = round(client["size"][1] + (screen["rows"] - state["rows"]) * state["cell_height"] / scale)
                window_action(app_id, f"resize({{x={width}, y={height}, relative=false, window=w}})")
                last_grid = grid
                resize_count += 1
                time.sleep(0.5)
            time.sleep(0.1)
        else:
            raise RuntimeError(f"Timed out waiting for the exact Foot capture viewport: {state}; window={client.get('size') if client else None}")
        identifier = client.get("stableId")
        if identifier is None:
            raise RuntimeError("Hyprland did not expose this window's stable toplevel identifier")
        subprocess.run(["grim", "-T", str(identifier), str(destination)], check=True, timeout=20)
        after = (hypr("activeworkspace")["id"], hypr("activewindow").get("address"))
        if after != before:
            raise RuntimeError("The active desktop changed during capture; no focus restoration was attempted")
        header = destination.read_bytes()[:24]
        if header[:8] != b"\x89PNG\r\n\x1a\n":
            raise RuntimeError("The terminal capture did not produce a PNG")
        return struct.unpack(">II", header[16:24])
    finally:
        owned = next((c for c in hypr("clients") if c.get("class") == app_id), None)
        if owned:
            window_action(app_id, "close({window=w})")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and any(c.get("class") == app_id for c in hypr("clients")):
                time.sleep(0.1)
            if any(c.get("class") == app_id for c in hypr("clients")):
                raise RuntimeError("The owned capture terminal did not close")


def render(source: Path, workspace: int = 10) -> None:
    manifest = load(source, ansi=True)
    destination = output_directory("desktop", manifest)
    with tempfile.TemporaryDirectory(prefix="cepheus-terminal-") as temporary:
        scratch = Path(temporary)
        config = scratch / "foot.ini"
        config.write_text(terminal_config(manifest))
        for screen in manifest["screens"]:
            dimensions = native_capture(source, screen, destination / screen["image"], config, scratch, workspace)
            screen["pixel_width"], screen["pixel_height"] = dimensions
            print(f"Captured {screen['id']}: {dimensions[0]} × {dimensions[1]} native Foot pixels", flush=True)
    manifest["renderer"] = "production ratatui CrosstermBackend replayed in Foot; grim toplevel screencopy"
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    gallery = ["# Desktop screenshots", "", "Captured as native Foot terminal pixels from Cepheus's production ratatui renderer, using synthetic project data. No owner sessions, accounts or credentials appear. The pairing code is inert.", "", "The terminal uses JetBrainsMono Nerd Font at 12 pt and a near-black palette. The README workspace image uses a compact 120 × 34 viewport for readability; the full gallery uses 160 × 44. Bold text, dim text, ANSI colors, Nerd Font glyphs and terminal cell widths are drawn by Foot.", "", "Regenerate with `python3 tools/capture_screenshots.py`, or replay offloaded buffers with `python3 tools/capture_screenshots.py --render-only CAPTURE_DIRECTORY`. Capture uses an empty, inactive Hyprland workspace (10 by default) and requires Foot, grim with toplevel capture, and JetBrainsMono Nerd Font.", ""]
    for screen in manifest["screens"]:
        gallery.extend([f"## {screen['title']}", "", f"![{screen['title']}](desktop/{screen['image']})", ""])
    (ROOT / "docs/screenshots/DESKTOP.md").write_text("\n".join(gallery))
    index()
    print(f"Captured {len(manifest['screens'])} desktop screenshots in {destination}")


def android(source: Path) -> None:
    manifest = load(source)
    destination = output_directory("android", manifest)
    gallery = ["# Android screenshots", "", "Captured from the real Android APK on a disposable emulator connected to an isolated service with synthetic data. The gallery contains no owner sessions or credentials. Dark is the default on new installs; the gallery also includes the optional Light theme.", "", "Regenerate with `tools/capture_android_screenshots.py` and import with `python3 tools/capture_screenshots.py --android CAPTURE_DIRECTORY`.", ""]
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
    parser.add_argument("--render-only", type=Path, help="capture previously exported ANSI buffers in a native Foot terminal")
    parser.add_argument("--android", type=Path, help="import an Android capture directory and its manifest")
    parser.add_argument("--workspace", type=int, default=10, help="empty inactive Hyprland workspace for native capture (default: 10)")
    args = parser.parse_args()
    if args.android:
        android(Path(os.path.abspath(args.android)))
        return
    for required in ("foot", "grim", "hyprctl"):
        if not shutil.which(required):
            parser.error(f"{required} is required for native terminal capture")
    if args.render_only:
        render(Path(os.path.abspath(args.render_only)), args.workspace)
        return
    with tempfile.TemporaryDirectory(prefix="cepheus-screenshots-") as temporary:
        env = dict(os.environ, CEPHEUS_SCREENSHOTS_DIR=temporary)
        subprocess.run(["cargo", "test", "--release", "--locked", "view::screenshots::capture_all_screens", "--", "--ignored", "--exact", "--nocapture"], cwd=ROOT, env=env, check=True)
        render(Path(temporary), args.workspace)


if __name__ == "__main__":
    if len(sys.argv) == 6 and sys.argv[1] == "_replay":
        replay(Path(sys.argv[2]), Path(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]))
    else:
        main()
