#!/usr/bin/env python3
"""Prove Claude's reported sub-agent launches/completions reach the phone without restarting."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from chat_ui_check import Device
from chat_ui_check import assert_private_guest
from chat_ui_check import m

NAMES = ("Inspect parser", "Review parser tests")


def main():
    assert_private_guest()
    m.guard()
    root = Path(tempfile.mkdtemp(prefix="phone-subagents-"))
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    shots.mkdir(parents=True, exist_ok=True)
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    summary = {}
    try:
        original_settle = m.settle

        def settle(current):
            original_settle(current)
            current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            current.shell(
                "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
            )

        m.settle = settle
        deck.start()
        os.environ["CEPHEUS_ONLY"] = "chat"
        m.journey(device, deck, Path(os.environ["CEPHEUS_APK"]), summary)
        session = next(s for s in deck.sessions() if s["tool"] == "claude")
        session_id = session["id"]

        def hook(payload):
            # What Claude Code hands the deck's hook (Claude Code 2.1.319).
            subprocess.run(
                [str(deck.binary), "hook", "claude"],
                input=json.dumps({"session_id": session["conversation"], **payload}),
                text=True,
                check=True,
                timeout=10,
                env={**deck.env, "CEPHEUS_SESSION": str(session_id)},
            )

        def running(indexes):
            return [
                {
                    "id": f"agent-{index}",
                    "type": "subagent",
                    "status": "running",
                    "description": NAMES[index],
                    "agent_type": "Explore",
                }
                for index in indexes
            ]

        def native_count():
            current = next(s for s in deck.sessions() if s["id"] == session_id)
            return sum(child["active"] for child in current.get("subagents", []))

        def finish(index, still):
            hook(
                {
                    "hook_event_name": "SubagentStop",
                    "agent_id": f"agent-{index}",
                    "agent_type": "Explore",
                    "stop_hook_active": False,
                    "background_tasks": running(still),
                    "session_crons": [],
                }
            )

        for index in range(len(NAMES)):
            agent = f"agent-{index}"
            hook({"hook_event_name": "SubagentStart", "agent_id": agent, "agent_type": "Explore"})
        # The turn that launched them ends; its Stop hook names their tasks.
        tasks = running(range(len(NAMES)))
        hook({"hook_event_name": "Stop", "background_tasks": tasks, "session_crons": []})

        m.wait_for("two real native launches", 30, lambda: native_count() == 2)
        device.tap(device.wait("chat active count", text="2 sub-agents active"))
        device.wait("details title", text="Sub-agents")
        for name in NAMES:
            device.wait("task name", text=name)
        device.wait("start time", contains="Started ")
        device.wait("last activity", contains="Last active ")
        device.screenshot("subagents-two-active")

        finish(0, [1])
        m.wait_for("one native child active", 20, lambda: native_count() == 1)
        device.wait("live completion in open sheet", text="1 active · 1 recent")
        device.screenshot("subagents-live-completion")
        device.tap(device.wait("close details", text="Done"))
        device.tap(device.wait("drawer", desc="Menu"))
        device.wait("drawer active count", contains="1 sub-agent active")
        device.screenshot("subagents-drawer")
        device.tap(device.wait("home", text="Home"))
        device.wait("home active count", contains="1 sub-agent active")
        device.screenshot("subagents-home")
        summary["live_launch_and_one_completion_in_all_surfaces"] = True

        finish(1, [])
        m.wait_for("both native tasks finished", 20, lambda: native_count() == 0)
        recent = "0 active · 2 recent sub-agents"
        device.wait("home updates to zero active", contains=recent)
        device.tap(device.wait("conversation", contains="show me at once"))
        device.wait("chat keeps recent tasks discoverable", text=recent)
        device.tap(device.wait("drawer after completion", desc="Menu"))
        device.wait("drawer updates to zero active", contains=recent)
        device.shell("input keyevent 4")
        device.tap(device.wait("recent task details", text=recent))
        device.wait("both tasks remain recent", text="0 active · 2 recent")
        for name in NAMES:
            device.wait("recent task name", text=name)
        device.screenshot("subagents-recent")
        summary["two_to_one_to_zero_without_restart"] = True

        device.shell("cmd uimode night yes")
        device.wait("dark details", text="Sub-agents")
        device.screenshot("subagents-dark")
        device.shell("settings put system font_scale 1.3")
        device.wait("large-font task name", text=NAMES[1])
        device.screenshot("subagents-large-text")
        device.tap(device.wait("close large-font details", text="Done"))
        device.tap(device.wait("large-font drawer", desc="Menu"))
        device.wait("large-font drawer recent count", contains=recent)
        device.screenshot("subagents-drawer-large-text")
        device.tap(device.wait("large-font home", text="Home"))
        device.wait("large-font home recent count", contains=recent)
        device.screenshot("subagents-home-large-text")
        device.tap(device.wait("large-font conversation", contains="show me at once"))
        device.wait("large-font chat recent count", text=recent)
        device.shell("cmd connectivity airplane-mode enable")
        device.wait("saved counts while offline", contains="Saved:", timeout=60)
        device.screenshot("subagents-offline")
        summary["offline_counts_marked_saved"] = True
        summary["ok"] = True
        print("SUBAGENTS_RESULT", json.dumps(summary), flush=True)
        print("SUBAGENTS_CAPTURE_DIR", shots, flush=True)
        if os.environ.get("CEPHEUS_HOLD_REVIEW") == "1":
            input()
    except Exception as error:
        summary["error"] = str(error)
        device.screenshot("subagents-failure")
        (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
        print("SUBAGENTS_FAILURE", repr(error), "CAPTURES", shots, flush=True)
        if os.environ.get("CEPHEUS_HOLD_REVIEW") == "1":
            input()
        raise
    finally:
        device.shell("cmd connectivity airplane-mode disable", check=False)
        deck.stop()
        (shots / "summary.json").write_text(json.dumps(summary, indent=2))
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
