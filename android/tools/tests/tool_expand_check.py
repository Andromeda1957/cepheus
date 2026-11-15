#!/usr/bin/env python3
"""Tap tool-card arrows on a disposable pool guest, using an isolated gateway."""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from chat_ui_check import Device
from chat_ui_check import assert_private_guest
from chat_ui_check import m
from device_waits import poll


def main():
    assert_private_guest()
    m.guard()
    root = Path(tempfile.mkdtemp(prefix="tool-expand-"))
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    shots.mkdir(parents=True, exist_ok=True)
    thinking = '\n    out("thinking (esc to interrupt)")\n'
    if m.STAND_IN.count(thinking) != 1:
        raise m.CheckFailed("The fixture requires one normal thinking output")
    m.STAND_IN = m.STAND_IN.replace(
        thinking,
        thinking + '    if text == "keep-working":\n        time.sleep(600)\n',
        1,
    )
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    summary = {}
    try:
        base_settle = m.settle

        def settle(current):
            base_settle(current)
            current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            current.shell(
                "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
            )

        m.settle = settle
        deck.start()
        os.environ["CEPHEUS_ONLY"] = "chat"
        m.journey(device, deck, Path(os.environ["CEPHEUS_APK"]), summary)
        transcript = next((deck.claude_dir / "projects").glob("*/*.jsonl"))
        session = next(s["id"] for s in deck.sessions() if s["tool"] == "claude")
        deck.ctl("send", str(session), "keep-working\n")
        device.wait("working agent", contains="Thinking…")

        def record(kind, content):
            with transcript.open("a") as output:
                output.write(
                    json.dumps({"type": kind, "message": {"role": kind, "content": content}}) + "\n"
                )

        def exercise(stage):
            failures = []
            for index in range(3):
                call = f"{stage}-{index}"
                marker = f"Output for {call}"
                record(
                    "assistant",
                    [
                        {
                            "type": "tool_use",
                            "id": call,
                            "name": "Bash",
                            "input": {"command": f"fixture-{call}"},
                        }
                    ],
                )
                record("user", [{"type": "tool_result", "tool_use_id": call, "content": marker}])
                latest = device.find(desc="Latest messages")
                if latest:
                    device.tap(latest[0])
                # A call is one line until it is opened (owner, 2026-11-03).
                device.tap(device.wait("folded call", contains=f"Ran fixture-{call}"))

                def card(call=call):
                    return [
                        n
                        for n in device.find(contains=f"fixture-{call}")
                        if "Ran fixture" not in n["text"] + n["desc"]
                    ]

                m.wait_for("opened call", 10, card)
                target = card()[0]
                arrows = [
                    n
                    for n in device.nodes()
                    if "Show details" in n["desc"]
                    and abs(sum(n["bounds"][1::2]) - sum(target["bounds"][1::2])) < 20
                ]
                if not arrows:
                    raise m.CheckFailed("The target tool has no visible arrow")
                x1, y1, x2, y2 = arrows[0]["bounds"]
                x, y = (x1 + x2) // 2, (y1 + y2) // 2
                device.shell(f"input motionevent DOWN {x} {y}")
                incoming = f"Incoming update during {call}."
                try:
                    record("assistant", [{"type": "text", "text": f"{incoming} " * 4}])
                    # The finger stays down until the update is in the chat under it.
                    poll(5, lambda incoming=incoming: device.find(contains=incoming))
                finally:
                    device.shell(f"input motionevent UP {x} {y}")
                try:
                    device.wait("result after incoming update", contains=marker, timeout=7)
                except m.CheckFailed:
                    failures.append(call)
                device.screenshot(f"{call}-after-tap")
                opened = device.find(contains="Hide details")
                for node in opened:
                    device.tap(node)
            summary[stage] = failures
            print(f"{stage.upper()}_FAILURES={failures}", flush=True)
            return failures

        if exercise("working"):
            raise m.CheckFailed("A tool tap was lost while new output arrived")
        summary["ok"] = True
    except Exception as error:
        summary["error"] = str(error)
        device.screenshot("failure")
        (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
        raise
    finally:
        deck.stop()
        (shots / "tool-summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
