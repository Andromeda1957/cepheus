#!/usr/bin/env python3
"""Exercise completed-reply actions on a disposable phone with a real gateway."""

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
from chat_ui_check import touch_target

REPLY = (
    "## Ready to share\n\nThe **complete reply** keeps both items:\n\n- First item\n- Second item\n\n"
    + chr(96) * 3
    + "python\nprint('hello')\n"
    + chr(96) * 3
    + "\n\nEnd of the reply."
)


def main():
    assert_private_guest()
    m.guard()
    root = Path(tempfile.mkdtemp(prefix="reply-actions-"))
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    shots.mkdir(parents=True, exist_ok=True)
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    summary = {}
    try:
        original_settle = m.settle

        def settle(current):
            original_settle(current)
            # Disable only voice typing during pairing; restore TTS for its action.
            current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            current.shell(
                "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
            )

        m.settle = settle
        deck.start()
        os.environ["CEPHEUS_ONLY"] = "chat"
        m.journey(device, deck, Path(os.environ["CEPHEUS_APK"]), summary)
        transcript = next((deck.claude_dir / "projects").glob("*/*.jsonl"))
        with transcript.open("a") as output:
            for kind, text in (("user", "Review reply actions."), ("assistant", REPLY)):
                content = text if kind == "user" else [{"type": "text", "text": text}]
                output.write(
                    json.dumps({"type": kind, "message": {"role": kind, "content": content}}) + "\n"
                )

        device.wait("the completed reply", contains="End of the reply.")
        latest = device.find(desc="Latest messages")
        if latest:
            device.tap(latest[0])

        def action(label):
            device.wait(label, desc=label)
            return max(device.find(desc=label), key=lambda node: node["bounds"][1])

        device.tap(action("Copy reply"))
        device.tap(device.fields()[-1])
        device.shell("input keyevent 279")
        plain = device.fields()[-1]["text"]
        for text in (
            "Ready to share",
            "complete reply",
            "First item",
            "Second item",
            "print('hello')",
            "End of the reply.",
        ):
            assert text in plain, plain
        assert "**" not in plain and chr(96) * 3 not in plain and "done: show me" not in plain, (
            plain
        )
        summary["full_plain_reply_copied"] = True
        device.type_into(device.fields()[-1], "")
        device.tap(action("More reply actions"))
        device.tap(device.wait("Copy Markdown", text="Copy Markdown"))
        device.tap(device.fields()[-1])
        device.shell("input keyevent 279")
        assert device.fields()[-1]["text"] == REPLY
        summary["exact_markdown_copied"] = True
        device.type_into(device.fields()[-1], "")

        device.tap(action("Share reply"))
        m.wait_for(
            "Android share sheet",
            15,
            lambda: "ChooserActivity" in device.shell("dumpsys activity activities"),
        )
        assert "ChooserActivity" in device.shell("dumpsys activity activities")
        device.screenshot("reply-share-sheet")
        device.shell("input keyevent 4")
        summary["native_share_sheet"] = True

        device.tap(action("More reply actions"))
        device.tap(device.wait("Select text", text="Select text"))
        device.wait("selection sheet", text="Copy all")
        device.wait("whole selected text", contains="End of the reply.")
        device.screenshot("reply-select-text")
        device.tap(device.wait("Copy all", text="Copy all"))
        device.shell("input keyevent 4")
        device.tap(device.fields()[-1])
        device.shell("input keyevent 279")
        assert device.fields()[-1]["text"] == plain
        device.type_into(device.fields()[-1], "")
        summary["select_text_and_copy_all"] = True

        device.shell("pm enable --user 0 com.google.android.tts", check=False)
        device.tap(action("Read aloud"))

        def reading_or_error():
            return [
                node
                for node in device.nodes()
                if node["desc"] == "Stop reading"
                or "text-to-speech settings" in node["text"]
                or "Install a voice" in node["text"]
            ]

        m.wait_for("speech start or voice guidance", 20, reading_or_error)
        stop = device.find(desc="Stop reading")
        if stop:
            device.tap(stop[-1])
            device.wait("speech stopped", desc="Read aloud")
            summary["read_aloud"] = "started and stopped"
        else:
            summary["read_aloud"] = "Android voice data unavailable; settings guidance shown"
        device.screenshot("reply-speech")
        # Recreate the screen in both themes, then at a larger system font.
        for night, scale, name in (
            ("no", "1.0", "reply-light"),
            ("yes", "1.0", "reply-dark"),
            ("yes", "1.3", "reply-large-text"),
        ):
            device.shell(f"cmd uimode night {night}")
            device.shell(f"settings put system font_scale {scale}")
            device.wait("reply actions after configuration change", desc="More reply actions")
            latest = device.find(desc="Latest messages")
            if latest:
                device.tap(latest[0])
            nodes = device.nodes()
            density = int(device.shell("wm density").split()[-1]) / 160
            for label in ("Copy reply", "Read aloud", "Share reply", "More reply actions"):
                choices = [node for node in nodes if node["desc"] == label]
                assert choices, (name, label)
                target = touch_target(nodes, max(choices, key=lambda node: node["bounds"][1]))
                x1, y1, x2, y2 = target["bounds"]
                assert x2 - x1 >= 48 * density - 1 and y2 - y1 >= 48 * density - 1, (label, target)
                assert x1 >= 0 and x2 <= 720 and y2 <= 1560, (label, target)
            device.screenshot(name)
        summary["themes_font_scale_and_touch_targets"] = True
        summary["ok"] = True
        print("REPLY_UI_CAPTURE_DIR", shots, flush=True)
        if os.environ.get("CEPHEUS_HOLD_REVIEW") == "1":
            print("REPLY_UI_REVIEW_READY", flush=True)
            input()
    except Exception as error:
        summary["error"] = str(error)
        device.screenshot("reply-failure")
        (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
        print("REPLY_UI_CAPTURE_DIR", shots, flush=True)
        if os.environ.get("CEPHEUS_HOLD_REVIEW") == "1":
            print("REPLY_UI_REVIEW_FAILURE", flush=True)
            input()
        raise
    finally:
        deck.stop()
        (shots / "reply-summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
