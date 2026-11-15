#!/usr/bin/env python3
"""Capture a live, isolated Cepheus TUI as native terminal pixels.

Requires Foot, tmux, Hyprland, grim toplevel capture, and JetBrainsMono Nerd Font.
The caller provisions an isolated service and dedicated tmux server; this tool
attaches only to the supplied socket and never launches an agent or reads auth.
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


def load(source: Path) -> dict:
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
        path = source / name
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


def hypr(*arguments: str):
    result = subprocess.run(["hyprctl", "-j", *arguments], capture_output=True, text=True, check=True, timeout=10)
    return json.loads(result.stdout)


def window_action(app_id: str, action: str) -> None:
    command = ("for _, w in ipairs(hl.get_windows()) do if w.class == " + json.dumps(app_id) +
               " then hl.dispatch(hl.dsp.window." + action + ") end end")
    subprocess.run(["hyprctl", "eval", command], check=True, capture_output=True, text=True, timeout=10)


def attach(socket: Path, target: str, ready: Path, columns: int, rows: int) -> None:
    """Wait for the native viewport, then attach to the caller's live TUI."""
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
        ready.write_text(json.dumps({"ready": fits, "columns": size.columns, "rows": size.lines,
                                     "cell_width": width, "cell_height": height}))
        if fits:
            environment = dict(os.environ, COLORTERM="truecolor")
            os.execvpe("tmux", ["tmux", "-S", str(socket), "attach-session", "-t", target], environment)
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


def _capture_live(socket: Path, target: str, screen: dict, destination: Path, config: Path, scratch: Path, workspace: int) -> tuple[int, int]:
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
        sys.executable, str(Path(__file__).resolve()), "_attach", str(socket), target, str(ready),
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
                time.sleep(1.5)
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


def capture_live(socket: Path, target: str, destination: Path, terminal: dict,
                 columns: int = 160, rows: int = 38, workspace: int = 10) -> tuple[int, int]:
    """Capture a real TUI in a dedicated caller-owned tmux server."""
    info = socket.stat()
    if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
        raise ValueError("Expected a caller-owned tmux Unix socket")
    if not re.fullmatch(r"[A-Za-z0-9_:-][A-Za-z0-9_.:-]{0,100}", target):
        raise ValueError("Expected a bounded tmux target")
    if not 20 <= columns <= 300 or not 20 <= rows <= 300:
        raise ValueError("Expected bounded terminal columns and rows")
    plain_path(destination)
    basename(destination.name, ".png")
    if destination.exists() and not destination.is_file():
        raise ValueError("Expected a regular PNG destination")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="cepheus-terminal-") as temporary:
        scratch = Path(temporary)
        config = scratch / "foot.ini"
        config.write_text(terminal_config({"terminal": terminal}))
        return _capture_live(socket, target, {"columns": columns, "rows": rows}, destination,
                             config, scratch, workspace)


def desktop(source: Path) -> None:
    manifest = load(source)
    destination = output_directory("desktop", manifest)
    old_manifest = destination / "manifest.json"
    previous = load(destination)["screens"] if old_manifest.exists() else []
    gallery = ["# Desktop screenshots", "",
               "Captured from the running public Cepheus binary and a real Codex client in an isolated demo repository. Foot draws the native terminal pixels; these are live TUI captures, with no handwritten conversation or renderer replay.", "",
               "The demo contains a small Git repository, a feature worktree, Markdown task sources, and a bounded coding task completed by Codex. Empty reports show facilities with no configured data. No owner sessions, project data, credentials, or private conversations appear.", "",
               "The terminal uses a 160 × 38 viewport, JetBrainsMono Nerd Font at 12 pt, and a near-black palette. Capture requires Foot, tmux, Hyprland, and grim with toplevel capture. See CONTRIBUTING.md for the isolated live capture workflow.", ""]
    for screen in manifest["screens"]:
        shutil.copyfile(source / screen["image"], destination / screen["image"])
        gallery.extend([f"## {screen['title']}", "", f"![{screen['title']}](desktop/{screen['image']})", ""])
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (ROOT / "docs/screenshots/DESKTOP.md").write_text("\n".join(gallery))
    retained = {screen["image"] for screen in manifest["screens"]}
    for screen in previous:
        if screen["image"] not in retained:
            (destination / screen["image"]).unlink()
    actual = {path.name for path in destination.glob("*.png")}
    if actual != retained:
        raise ValueError(f"Desktop PNG set differs from its manifest: {sorted(actual ^ retained)}")
    index()
    print(f"Imported {len(manifest['screens'])} live desktop captures in {destination}")


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
    lines = ["# Screenshots", "", "These captures show the running Cepheus desktop TUI and Android APK with isolated example data.", ""]
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
    actions = parser.add_mutually_exclusive_group(required=True)
    actions.add_argument("--live", type=Path, metavar="TMUX_SOCKET", help="capture a live TUI in a dedicated caller-owned tmux server")
    actions.add_argument("--desktop", type=Path, help="import an actual desktop capture directory and its manifest")
    actions.add_argument("--android", type=Path, help="import an Android capture directory and its manifest")
    parser.add_argument("--target", default="capture:0.0", help="target pane in the private tmux server")
    parser.add_argument("--capture", type=Path, help="PNG destination for a live capture")
    parser.add_argument("--columns", type=int, default=160)
    parser.add_argument("--rows", type=int, default=38)
    parser.add_argument("--workspace", type=int, default=10, help="empty inactive Hyprland workspace for capture")
    args = parser.parse_args()
    if args.android:
        android(Path(os.path.abspath(args.android)))
        return
    if args.desktop:
        desktop(Path(os.path.abspath(args.desktop)))
        return
    if args.capture is None:
        parser.error("--live requires --capture OUTPUT.png")
    for required in ("foot", "grim", "hyprctl", "tmux"):
        if not shutil.which(required):
            parser.error(f"{required} is required for native terminal capture")
    terminal = json.loads((ROOT / "docs/screenshots/desktop/manifest.json").read_text())["terminal"]
    dimensions = capture_live(args.live, args.target, Path(os.path.abspath(args.capture)), terminal,
                              args.columns, args.rows, args.workspace)
    print(json.dumps({"image": args.capture.name, "pixel_width": dimensions[0], "pixel_height": dimensions[1]}))


if __name__ == "__main__":
    if len(sys.argv) == 7 and sys.argv[1] == "_attach":
        attach(Path(sys.argv[2]), sys.argv[3], Path(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6]))
    else:
        main()
