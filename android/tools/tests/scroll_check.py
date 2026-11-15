#!/usr/bin/env python3
"""Exercise long-lived history, streaming swipes and restart on a private guest."""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from chat_ui_check import Device  # noqa: E402
from chat_ui_check import assert_private_guest  # noqa: E402
from chat_ui_check import m  # noqa: E402


def main():
    assert_private_guest()
    m.guard()
    root = Path(tempfile.mkdtemp(prefix="scroll-check-")).resolve()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    summary = {}
    original_settle = m.settle

    def settle(current):
        original_settle(current)
        current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
        current.shell(
            "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
        )

    m.settle = settle
    try:
        deck.start()
        os.environ["CEPHEUS_ONLY"] = "chat"
        m.journey(device, deck, Path(os.environ["CEPHEUS_APK"]), summary)
        session = next(s["id"] for s in deck.sessions() if s["tool"] == "claude")
        transcript = next((deck.claude_dir / "projects").glob("*/*.jsonl"))

        def record(role, content):
            with transcript.open("a") as output:
                output.write(
                    json.dumps({"type": role, "message": {"role": role, "content": content}}) + "\n"
                )

        def text(value):
            record("assistant", [{"type": "text", "text": value}])

        # Every batch is observed before the next: this reproduces a client
        # retaining deltas continuously as the gateway evicts its oldest items.
        for batch in range(12):
            for index in range(400):
                number = batch * 400 + index
                text(
                    f"History {number}. Readable **chat** with ordinary prose. "
                    + "Review the current work. " * 3
                )
                if index % 20 == 0:
                    call = f"scroll-{number}"
                    record(
                        "assistant",
                        [
                            {
                                "type": "tool_use",
                                "id": call,
                                "name": "Bash",
                                "input": {"command": call},
                            }
                        ],
                    )
                    record(
                        "user",
                        [
                            {
                                "type": "tool_result",
                                "tool_use_id": call,
                                "content": f"Result {number}",
                            }
                        ],
                    )
            text(f"Scroll batch {batch} complete.")
            device.wait("latest batch", contains=f"Scroll batch {batch} complete.", timeout=30)
        summary["long_lived_history_over_5000_items"] = True
        device.hide_keyboard()
        device.screenshot("03-long-lived-chat")
        (shots / "long-lived-memory.txt").write_text(device.shell(f"dumpsys meminfo {m.PACKAGE}"))
        device.shell(f"dumpsys gfxinfo {m.PACKAGE} reset")

        # Keep older content visible while more messages arrive.
        for _ in range(3):
            device.shell("input swipe 360 350 360 1100 400")
        device.wait("return to latest", desc="Latest messages")
        visible = {n["text"] for n in device.nodes() if n["text"].startswith("History ")}
        assert visible, "The swipe did not reach older chat"
        device.shell("input motionevent DOWN 360 700")
        try:
            for index in range(100):
                text(f"Streaming update {index}. " + "New output while reading. " * 4)
            # Wait for an independent gateway round trip while the finger is down.
            deck.ctl("status")
        finally:
            device.shell("input motionevent UP 360 700")
        device.wait("older position remains", desc="Latest messages")
        current = {n["text"] for n in device.nodes() if n["text"].startswith("History ")}
        assert visible & current, "Incoming output moved the older reading position"
        assert not device.find(contains="Streaming update 99.")
        summary["streaming_preserves_reading_position"] = True
        device.screenshot("04-reading-during-updates")
        device.tap(device.wait("Latest messages", desc="Latest messages"))
        device.wait("latest streamed update", contains="Streaming update 99.")
        device.shell("input swipe 360 350 360 1100 400")
        device.wait("subsequent swipe", desc="Latest messages")
        summary["scroll_after_streaming"] = True
        (shots / "frames.txt").write_text(device.shell(f"dumpsys gfxinfo {m.PACKAGE} framestats"))

        # A restart still opens the same conversation and paging remains usable.
        device.shell(f"am force-stop {m.PACKAGE}")
        device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY} --el session {session}")
        device.wait("same chat after restart", contains="Streaming update 99.", timeout=30)
        device.shell("input swipe 360 350 360 1100 400")
        device.wait("scroll after restart", desc="Latest messages")
        summary["restart_and_paging"] = True
        device.screenshot("05-scroll-after-restart")
        summary["ok"] = True
    finally:
        deck.stop()
        (shots / "scroll-summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
