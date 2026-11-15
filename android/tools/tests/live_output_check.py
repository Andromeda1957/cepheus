#!/usr/bin/env python3
"""Verify unfinished replies in the actual app on a disposable pool emulator."""

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

from test_support import is_managed_path  # noqa: E402

BRANCH = r'''
    if text == "stream reply":
        record("user", text)
        stages = ["First words", "First words arrive live", "First words arrive live before saving."]
        for index, body in enumerate(stages, 1):
            out("\x1b[2J\x1b[H❯ " + text + "\n\n● " + body
                + "\n\n✶ Working… (esc to interrupt)\n"
                + "─" * 60 + "\n❯\n" + "─" * 60 + "\n")
            marker = os.path.join(CHECK_START_DIR, "stage-" + str(index))
            until = time.monotonic() + 120
            while not os.path.exists(marker):
                if time.monotonic() > until:
                    raise RuntimeError("phone never observed live stage " + str(index))
                time.sleep(0.05)
        record("assistant", stages[-1])
        record("assistant", "Saved checkpoint.")
        out("\x1b[2J\x1b[H❯ " + text + "\n\n● " + stages[-1]
            + "\n\n" + "─" * 60 + "\n❯\n" + "─" * 60 + "\n")
        continue
'''


def main() -> None:
    assert_private_guest()
    m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not is_managed_path(shots):
        raise m.CheckFailed("Captures must remain in managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    m.STAND_IN = m.STAND_IN.replace("    if not text:", BRANCH + "    if not text:", 1)
    # Reuse pairing, without running unrelated journeys.
    m.stats = lambda device, summary: None
    base_settle = m.settle

    def settle(device: Device) -> None:
        base_settle(device)
        device.shell("pm disable-user --user 0 com.google.android.tts", check=False)
        device.shell("settings put secure show_ime_with_hard_keyboard 0")

    m.settle = settle
    summary: dict = {}
    with tempfile.TemporaryDirectory(prefix="live-reply-") as root:
        deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), Path(root))
        device = Device(os.environ["ANDROID_SERIAL"], shots)
        try:
            deck.start()
            m.journey(device, deck, Path(os.environ["CEPHEUS_APK"]), summary, stats_only=True)
            device.tap(device.wait("New session", text="New session"))
            device.tap(device.wait("Claude", text="Claude"))
            device.tap(device.wait("Start", text="Start"))
            m.wait_for("Claude session", 30, lambda: deck.sessions())
            session = max(s["id"] for s in deck.sessions() if s["tool"] == "claude")
            device.wait("chat composer", timeout=60, cls="android.widget.EditText")
            m.send_from_app(device, deck, session, "stream reply")
            stages = ["First words", "First words arrive live", "First words arrive live before saving."]
            for index, text in enumerate(stages, 1):
                device.wait(f"unfinished reply stage {index}", timeout=45, contains=text)
                transcripts = list((deck.claude_dir / "projects").glob("*/*.jsonl"))
                assert transcripts and all(
                    '"assistant"' not in path.read_text() for path in transcripts
                ), "assistant was persisted before the phone displayed its partial reply"
                if index in (1, 3):
                    device.screenshot(f"live-stage-{index}")
                (deck.root / "work" / f"stage-{index}").touch()
            m.wait_for(
                "saved answer",
                30,
                lambda: any('"assistant"' in p.read_text() for p in transcripts),
            )
            device.wait("canonical checkpoint", contains="Saved checkpoint.")
            device.wait("canonical answer", contains=stages[-1])
            copies = len(device.find(contains=stages[-1]))
            assert copies == 1, f"canonical answer appeared {copies} times"
            device.screenshot("saved-once")
            summary.update(ok=True, unfinished_stages=3, reconciled_once=True)
        except Exception as error:
            summary.update(ok=False, error=str(error))
            device.screenshot("failure")
            (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
            raise
        finally:
            deck.stop()
            (shots / "summary.json").write_text(json.dumps(summary, indent=2))
            print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
