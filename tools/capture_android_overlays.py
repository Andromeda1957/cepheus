#!/usr/bin/env python3
"""Additional app-owned overlays for the disposable Android capture journey.

Call capture_overlays(device, deck, journey, shot, back) before the main
journey's finally block. It uses only that journey's synthetic service and
explicitly selected emulator, and leaves the queue and scheduled work disabled.
"""
from __future__ import annotations

import base64
import json
import os
from pathlib import Path
import shlex
import subprocess
import time
import xml.etree.ElementTree as ET

import device_check as m


def capture_overlays(device, deck, journey, shot, back) -> None:
    m.guard()
    if device.serial != os.environ["CEPHEUS_TEST_EMULATOR_SERIAL"]:
        raise m.CheckFailed("Overlay captures require the guarded disposable emulator")

    # Reuse the real, neutral desktop capture as a message attachment. Both
    # thumbnail and viewer are rendered by the APK through its native image API.
    picture = Path(__file__).resolve().parents[1] / "docs/screenshots/desktop/01-workspace.png"
    deck.ctl("new", "--name", "Image and reply examples", "claude")
    m.wait_for(
        "overlay fixture session", 30,
        lambda: next((s for s in deck.sessions() if s.get("title") == "Image and reply examples"), None),
    )
    session = next(s for s in deck.sessions() if s.get("title") == "Image and reply examples")
    slug = "".join(c if c.isascii() and c.isalnum() else "-" for c in str(deck.root / "work"))
    transcript = deck.claude_dir / "projects" / slug / f"{session['conversation']}.jsonl"
    transcript.parent.mkdir(parents=True, exist_ok=True)
    records = [
        {"type": "user", "message": {"role": "user", "content": [
            {"type": "text", "text": "Review this example workspace."},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                           "data": base64.b64encode(picture.read_bytes()).decode("ascii")}},
        ]}},
        {"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "## Workspace review\n\nThe sessions are easy to scan. The sidebar shows their status and the active conversation stays readable."},
        ]}},
        # A user boundary seals the preceding reply even if the fake native
        # process has not published its next idle-status observation yet.
        {"type": "user", "message": {"role": "user", "content": "I will inspect the full-size picture."}},
    ]
    transcript.write_text("".join(json.dumps(record) + "\n" for record in records))
    journey.home()
    journey.tap("Image and reply examples")
    device.wait("attachment thumbnail", timeout=45, desc="Picture you sent")
    shot("overlay-chat-image", "Conversation with an image attachment")
    device.tap(device.wait("open attachment", desc="Picture you sent"))
    device.wait("full-screen picture", timeout=30, desc="Close")
    shot("overlay-image-viewer", "Full-screen image viewer")
    device.tap(device.wait("close picture", desc="Close"))
    device.tap(device.wait("reply actions", timeout=30, desc="More reply actions"))
    device.wait("reply menu", text="Copy Markdown")
    shot("overlay-reply-actions", "Reply actions menu")
    journey.tap("Select text")
    device.wait("select reply text", text="Copy all")
    shot("overlay-select-text", "Select reply text")
    back()

    # Disabled registrations are sufficient for the future-schedule editor;
    # this never grants admission to a real or synthetic scheduled job.
    chore = {
        "id": "docs-daily-check", "owner": "docs", "repository": "sample-project",
        "title": "Daily documentation check", "target": "job", "enabled": False,
        "schedule": {"cron": "0 9 * * *", "timezone": "UTC"},
        "spec": {"key": "docs-daily-template", "title": "Check documentation", "kind": "job",
                 "cwd": str(deck.root / "work"), "placement": "local",
                 "argv": ["/bin/printf", "Documentation checks passed\\n"]},
    }
    deck.ctl("chore", "register", json.dumps(chore))
    journey.tools("Queue")
    journey.tap("Work")
    journey.top()
    journey.tap("Enable queue")
    device.wait("queue action confirmation", text="Enable queue?")
    shot("overlay-queue-confirmation", "Queue action confirmation")
    journey.tap("Cancel")
    journey.tap("Resume draft" if device.find(text="Resume draft") else "Add work")
    journey.tap("Discard draft")
    device.wait("draft discard confirmation", text="Discard this draft?")
    shot("overlay-discard-draft", "Discard queued-work draft confirmation")
    journey.tap("Keep draft")
    back()
    journey.top()
    journey.tap("Chores")
    device.wait("registered schedule", timeout=30, text="Daily documentation check")
    journey.tap("Edit schedule")
    device.wait("future schedule editor", text="Edit future schedule")
    shot("overlay-edit-schedule", "Edit future chore schedule")
    journey.tap("Cancel")

    _capture_battery_explanation(device, shot)


def _capture_battery_explanation(device, shot) -> None:
    """Exercise Onboarding's app dialog without opening the OS exemption UI.

    This is the debug APK in the guarded guest. Preserve complete private prefs
    in memory, change just the two onboarding inputs while stopped, then restore
    the original files before the app resumes. No pairing values are logged.
    """
    package = shlex.quote(m.PACKAGE)
    paths = ("shared_prefs/settings.xml", "shared_prefs/agentdeck.xml")
    originals: dict[str, str | None] = {}
    whitelisted = any(m.PACKAGE in line.split(",") for line in
                      device.shell("cmd deviceidle whitelist").splitlines())

    def write_private(path: str, contents: str) -> None:
        command = f"run-as {package} sh -c {shlex.quote('cat > ' + shlex.quote(path))}"
        result = subprocess.run(
            [device.adb_path, "-s", device.serial, "shell", command],
            input=contents, capture_output=True, text=True, timeout=30,
        )
        if result.returncode:
            raise m.CheckFailed("Could not write the disposable debug app's onboarding fixture")

    device.shell(f"am force-stop {package}")
    try:
        for path in paths:
            read = f"if test -f {shlex.quote(path)}; then printf 'present\\n'; cat {shlex.quote(path)}; else printf 'absent\\n'; fi"
            contents = device.shell(f"run-as {package} sh -c {shlex.quote(read)}")
            marker, _, contents = contents.partition("\n")
            if marker not in ("present", "absent"):
                raise m.CheckFailed("Could not read the disposable app's onboarding fixture")
            originals[path] = contents if marker == "present" else None
        for path, key, tag, value in (
            (paths[0], "background", "string", "true"),
            (paths[1], "battery_asked", "boolean", "false"),
        ):
            tree = ET.fromstring(originals[path]) if originals[path] is not None else ET.Element("map")
            for child in list(tree):
                if child.get("name") == key:
                    tree.remove(child)
            child = ET.SubElement(tree, tag, {"name": key})
            if tag == "string":
                child.text = value
            else:
                child.set("value", value)
            write_private(path, ET.tostring(tree, encoding="unicode"))
        if whitelisted:
            device.shell(f"cmd deviceidle whitelist -{package}")
        device.shell(f"am start -n {package}/{shlex.quote(m.ACTIVITY)}")
        device.wait("battery onboarding explanation", timeout=45, text="Keep alerts on time")
        shot("overlay-battery-explanation", "Background connection battery explanation")
        device.tap(device.wait("decline battery exemption", text="Not now"))
    finally:
        device.shell(f"am force-stop {package}", check=False)
        for path, contents in originals.items():
            if contents is None:
                device.shell(f"run-as {package} rm -f {shlex.quote(path)}")
            else:
                write_private(path, contents)
        if whitelisted:
            device.shell(f"cmd deviceidle whitelist +{package}")
        device.shell(f"am start -n {package}/{shlex.quote(m.ACTIVITY)}")
        time.sleep(1)
