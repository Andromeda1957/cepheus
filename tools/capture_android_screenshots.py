#!/usr/bin/env python3
"""Capture Android screens from the real APK against an isolated synthetic deck.

Set CEPHEUS_BIN, CEPHEUS_APK, CEPHEUS_SHOTS, CEPHEUS_TEST_TMPDIR, and both
ANDROID_SERIAL and CEPHEUS_TEST_EMULATOR_SERIAL to an explicitly disposable
emulator. The selected debug app's data is reset on that emulator only.
Captures never connect to an installed owner service or account.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT / "android/tools"), str(ROOT / "android/tools/tests")]
import device_check as m
import chat_ui_check as ui
import deck_tools_check as tools
from test_support import storage_root
from capture_android_overlays import capture_overlays


class CaptureJourney(tools.Journey):
    def tools(self, page: str | None = None) -> None:
        super().tools()
        if page:
            # The tool index retains its scroll position when a page closes.
            self.top()
            self.tap(page)


def main() -> None:
    m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"]).resolve()
    if not shots.is_relative_to(storage_root()):
        raise RuntimeError("CEPHEUS_SHOTS must be beneath CEPHEUS_TEST_TMPDIR")
    shots.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="cepheus-docs-", dir=storage_root()))
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]).resolve(), root)
    deck.env["CEPHEUS_TEMP_DIR"] = str(root / "client-tmp")
    device = ui.Device(os.environ["ANDROID_SERIAL"], shots)
    journey = CaptureJourney(device, deck, {})
    entries: list[dict] = []

    def shot(name: str, title: str) -> None:
        device.hide_keyboard()
        time.sleep(0.8)
        path = device.screenshot(name)
        entries.append({"id": name, "title": title, "image": path.name})

    def back() -> None:
        device.hide_keyboard()
        device.shell("input keyevent 4")
        time.sleep(0.5)

    def settings() -> None:
        journey.home()
        m.open_screen(device, "Settings")
        device.wait("Settings", text="Stats")

    try:
        _, extension = tools.seed(deck)
        deck.grok_id = deck.seed_conversations()
        config = json.loads(deck.config.read_text())
        config["host_name"] = "workstation"
        config["short_titles"] = False
        deck.config.write_text(json.dumps(config))
        deck.start()
        if device.shell(f"pm path {m.PACKAGE}", check=False).strip():
            device.adb("uninstall", m.PACKAGE)
        device.adb("install", "-g", str(Path(os.environ["CEPHEUS_APK"]).resolve()), timeout=300)
        if device.shell(f"pm clear {m.PACKAGE}").strip() != "Success":
            raise m.CheckFailed("Could not reset the selected disposable app")
        device.shell(f"pm grant {m.PACKAGE} android.permission.POST_NOTIFICATIONS", check=False)
        device.shell("cmd uimode night no")
        m.settle(device)
        device.shell("pm disable-user --user 0 com.google.android.tts", check=False)
        device.shell("settings put secure show_ime_with_hard_keyboard 0")
        device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
        device.wait("pairing form", contains="Pair this phone")
        shot("01-pairing", "Pair this phone")
        code = deck.open_pairing()
        for index, value in enumerate(["10.0.2.2", str(deck.port), deck.fingerprint(), code]):
            device.type_into(device.fields()[index], value)
        device.hide_keyboard()
        journey.tap("Pair")
        device.wait("connected home", timeout=45, text="Local agents")
        shot("02-home-empty", "Home before starting sessions")

        # Queue stays disabled. These jobs/tasks are bounded synthetic examples.
        deck.ctl("new", "--name", "Parser diagnostics", "claude")
        m.wait_for("synthetic session", 30, lambda: deck.sessions())
        session = deck.sessions()[0]
        slug = "".join(c if c.isascii() and c.isalnum() else "-" for c in str(root / "work"))
        transcript = deck.claude_dir / "projects" / slug / f"{session['conversation']}.jsonl"
        transcript.parent.mkdir(parents=True, exist_ok=True)
        records = [
            {"type": "user", "message": {"content": "Improve the parser diagnostics."}},
            {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "I updated the diagnostics to preserve the source location.\n\n- Nested expressions are covered.\n- Missing delimiters now have a useful message.\n- The parser checks pass."}]}},
        ]
        transcript.write_text("".join(json.dumps(record) + "\n" for record in records))
        deck.ctl("new", "--name", "Documentation review", "claude")
        deck.ctl("queue", "add", json.dumps({"key": "docs-parser-checks", "title": "Run parser checks", "kind": "job", "cwd": str(root / "work"), "argv": ["/bin/printf", "Parser checks passed\\n"], "placement": "local"}))
        deck.ctl("job", "start", json.dumps({"key": "docs-build", "title": "Build documentation", "cwd": str(root / "work"), "argv": ["/bin/printf", "Documentation built successfully\\n42 pages generated\\n"], "placement": "local"}))
        time.sleep(1)
        shot("03-home", "Sessions home")
        device.tap(device.wait("drawer", desc="Menu"))
        shot("04-drawer", "Session drawer")
        device.tap(device.wait("home", text="Home"))

        device.tap(device.wait("resume", desc="Resume a conversation"))
        device.wait("resume list", timeout=45, text="Tidy the parser")
        shot("05-resume", "Resume a conversation")
        back()
        device.tap(device.wait("new session", desc="New session"))
        device.wait("new session sheet", text="Start")
        shot("06-new-session", "New session")
        journey.tap("Advanced")
        device.wait("advanced options", text="Session name")
        shot("07-new-session-options", "Session options")
        journey.tap("Done")
        back()

        journey.tap("Parser diagnostics")
        device.wait("chat menu", timeout=45, desc="More")
        device.wait("synthetic reply", timeout=45, contains="preserve the source location")
        shot("08-chat", "Agent conversation")
        device.tap(device.wait("session menu", desc="More"))
        shot("09-session-menu", "Session actions")
        journey.tap("Rename session")
        device.wait("rename", text="Session name")
        shot("10-rename", "Rename session")
        journey.tap("Cancel")

        # Additional app-owned chat surfaces; no Android system pickers are captured.
        device.tap(device.wait("attachment menu", desc="Attach"))
        shot("45-attachments", "Attachments menu")
        journey.tap("Skills and commands")
        device.wait("command suggestions", timeout=30, contains="Skills and commands")
        shot("46-commands", "Skills and commands")
        if device.fields():
            device.type_into(device.fields()[-1], "")
        device.hide_keyboard()
        model = device.find(contains="Model:")
        if model:
            device.tap(model[0])
            device.wait("model picker", text="Model")
            shot("47-model", "Model and effort selection")
            back()
        device.tap(device.wait("session menu", desc="More"))
        journey.tap("Send when idle")
        shot("48-idle-delivery", "Send when idle composer")
        device.tap(device.wait("session menu", desc="More"))
        journey.tap("Use normal delivery")
        device.tap(device.wait("session menu", desc="More"))
        journey.tap("Close session")
        shot("49-close-confirmation", "Close session confirmation")
        journey.tap("Cancel")
        hook = {"session_id": session["conversation"], "hook_event_name": "Stop", "background_tasks": [
            {"id": "docs-parser", "type": "subagent", "status": "running", "description": "Inspect parser edge cases", "agent_type": "Explore"},
            {"id": "docs-review", "type": "subagent", "status": "running", "description": "Review diagnostics", "agent_type": "Explore"},
        ], "session_crons": []}
        for task in hook["background_tasks"]:
            event = {"session_id": session["conversation"], "hook_event_name": "SubagentStart", "agent_id": task["id"], "agent_type": "Explore"}
            subprocess.run([str(deck.binary), "hook", "claude"], input=json.dumps(event), text=True, check=True, timeout=15,
                           env={**deck.env, "CEPHEUS_SESSION": str(session["id"])})
        subprocess.run([str(deck.binary), "hook", "claude"], input=json.dumps(hook), text=True, check=True, timeout=15,
                       env={**deck.env, "CEPHEUS_SESSION": str(session["id"])})
        device.tap(device.wait("subagent summary", timeout=30, text="2 sub-agents active"))
        device.wait("subagents sheet", text="Sub-agents")
        shot("50-subagents", "Sub-agent details")
        journey.tap("Done")
        deck.ctl("keys", str(session["id"]), "please ask me\\r")
        device.wait("question", timeout=45, contains="Do you want to proceed?")
        time.sleep(6)  # Let the app's transient heads-up notification fade.
        shot("51-question", "Agent question")
        device.tap(device.wait("message instead", desc="Message instead"))
        device.tap(device.wait("conversation message", contains="Improve the parser diagnostics."))
        time.sleep(2)  # Allow the composer and any keyboard transition to settle.
        shot("52-question-collapsed", "Message while a question waits")
        deck.ctl("keys", str(session["id"]), "\\r")

        journey.home()
        device.tap(device.wait("drawer", desc="Menu"))
        search = device.fields()[0]
        device.type_into(search, "parser")
        device.hide_keyboard()
        shot("53-session-search", "Search sessions")
        device.type_into(device.fields()[0], "")
        device.hide_keyboard()
        device.tap(device.wait("home", text="Home"))

        settings()
        shot("11-settings", "Settings")
        journey.tap("Stats")
        device.wait("statistics", timeout=30, text="Agent usage")
        shot("12-stats", "Usage and machine statistics")
        back()
        journey.tap("Errors")
        device.wait("errors", text="Errors")
        shot("13-errors", "Service errors")
        back()
        journey.tap("Saved connections")
        device.wait("connections", text="Saved connections")
        shot("14-connections", "Saved connections")
        back()
        journey.tap("Theme")
        device.wait("theme options", text="Theme")
        shot("15-theme", "Theme selection")
        journey.tap("Cancel")
        device.shell("cmd uimode night yes")
        journey.home()
        shot("16-home-dark", "Sessions home in dark theme")

        journey.tools()
        shot("17-tools", "Deck tools")
        pages = [(18, "Queue"), (19, "Jobs"), (20, "Scheduler"), (21, "Branches"),
                 (23, "Broadcast"), (24, "Terminals"), (25, "Scans"), (26, "Extensions"),
                 (27, "Scripting"), (28, "Service")]
        for index, page in pages:
            journey.tools(page)
            time.sleep(1.5)
            shot(f"{index:02}-{page.lower()}", page)

        journey.tools("Queue")
        device.wait("queue ready", timeout=30, text="Refresh queue")
        journey.tap("Run parser checks")
        shot("29-queue-detail", "Queued work details")
        journey.tap("Sources")
        shot("30-task-sources", "Task sources")
        journey.tap("Chores")
        shot("31-chores", "Scheduled chores")
        journey.tap("Register scheduled work")
        shot("32-chore-form", "Register scheduled work")
        back()
        journey.tap("Work")
        journey.tap("Resume draft")
        journey.tap("Discard draft")
        device.wait("discard scheduled draft", text="Discard this draft?")
        journey.tap("Discard draft")
        journey.tap("Add work")
        shot("33-queue-form", "Add queued work")
        back()
        journey.tap("Batch")
        shot("34-batch-form", "Batch queued work")
        back()

        journey.tools("Jobs")
        device.wait("example job", timeout=30, text="Build documentation")
        journey.tap("Build documentation")
        device.wait("job details", timeout=30, text="Read-only output")
        shot("35-job-output", "Headless job output")
        journey.tap("Show raw log")
        shot("36-job-raw", "Headless job raw log")
        journey.tools("Jobs")
        journey.tap("Start job")
        shot("37-job-form", "New headless job")

        journey.tools("Terminals")
        journey.tap("Options")
        shot("38-terminal-options", "Terminal options")
        journey.tap("Done")
        journey.tap("Open terminal")
        device.wait("terminal", timeout=45, desc="Terminal screen. Tap to type.")
        shot("39-terminal", "Hosted terminal")
        journey.tools("Scans")
        journey.tap("Read report")
        device.wait("scan report", timeout=30, contains="Fixture saved report")
        shot("40-scan-report", "Saved security report")
        journey.tools("Extensions")
        journey.fill("Extension source", "../" + extension.name)
        journey.tap("Preview installation")
        device.wait("extension preview", timeout=45, contains="phone-fixture-skill")
        shot("41-extension-preview", "Extension installation preview")
        journey.tools("Scripting")
        journey.tap("Run phone-fixture")
        journey.top()
        device.wait("Lua picker", timeout=30, text="Fixture choice")
        shot("42-plugin-picker", "Lua plugin picker")
        journey.tap("Cancel")
        journey.top()
        journey.tap("Advanced")
        shot("43-scripting-options", "Lua scripting options")
        journey.tools("Service")
        journey.tap("Restart deck services")
        device.wait("service confirmation", text="Confirm")
        shot("44-service-confirmation", "Service action confirmation")
        journey.tap("Cancel")
        capture_overlays(device, deck, journey, shot, back)
    finally:
        (shots / "manifest.json").write_text(json.dumps({"schema": 1, "renderer": "Android APK via adb screencap", "data": "isolated synthetic fixtures", "screens": entries}, indent=2) + "\n")
        deck.stop()
        device.shell(f"am force-stop {m.PACKAGE}", check=False)
        shutil.rmtree(root)
    print(f"Captured {len(entries)} Android screens in {shots}")


if __name__ == "__main__":
    main()
