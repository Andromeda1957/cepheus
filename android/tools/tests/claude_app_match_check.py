#!/usr/bin/env python3
"""The chat as the Claude app shows it, on a disposable pool guest with an isolated gateway.

Owner, 2026-11-03, with screenshots of the Claude app beside the phone: a run
of tool calls folds into one line, a sent screenshot shows as a picture, the
summary Claude Code writes after compacting is not his message, and the
working time counts from his newest message rather than the first turn.
Each part is checked and reported; the check fails if any part does.
"""

from __future__ import annotations

import json
import os
import shutil
import struct
import sys
import tempfile
import time
import zlib
from base64 import b64encode
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from chat_ui_check import Device
from chat_ui_check import assert_private_guest
from chat_ui_check import m
from device_waits import poll


def png(width: int, height: int) -> bytes:
    """A small striped picture, so the screenshot shows it was drawn."""
    rows = bytearray()
    for y in range(height):
        rows.append(0)
        for x in range(width):
            band = (x // 20) % 2
            rows += (
                bytes((40 + 180 * band, 110, 220 - 160 * band))
                if y < height - 10
                else bytes((20, 20, 20))
            )

    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return (
            struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(bytes(rows)))
        + chunk(b"IEND", b"")
    )


def stamp() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime())


def main():
    assert_private_guest()
    m.guard()
    root = Path(tempfile.mkdtemp(prefix="claude-app-match-"))
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    shots.mkdir(parents=True, exist_ok=True)
    m.STAND_IN = m.STAND_IN.replace(
        '    out("thinking (esc to interrupt)")',
        '    out("thinking (esc to interrupt)")\n    if text == "keep-working":\n        time.sleep(900)',
    )
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    summary: dict = {"failures": []}

    def failed(part: str, why: str) -> None:
        summary["failures"].append(f"{part}: {why}")
        print(f"FAILED {part}: {why}", flush=True)

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

        def record(value: dict) -> None:
            with transcript.open("a") as output:
                output.write(json.dumps(value) + "\n")

        def latest() -> None:
            button = device.find(desc="Latest messages")
            if button:
                device.tap(button[0])

        def seen(what: str, timeout: float = 30, **query) -> dict | None:
            try:
                latest()
                return device.wait(what, timeout=timeout, **query)
            except m.CheckFailed:
                return None

        def timer() -> str:
            texts = [n["text"].strip() for n in device.nodes() if n["text"].strip().startswith("·")]
            return texts[0].lstrip("· ").strip() if texts else ""

        def timer_in(unit: str, seconds: float) -> str:
            """The timer once it counts in `unit` ("45s", "1m"), or its last reading after `seconds`."""
            reading = ""

            def counts() -> bool:
                nonlocal reading
                reading = timer()
                return reading.endswith(unit)

            poll(seconds, counts)
            return reading

        # 1. The working time: a minute into the turn, then after a newer message.
        deck.ctl("send", str(session), "keep-working\n")
        device.wait("working agent", contains="Thinking…")
        # It reads minutes from 60 s into the turn and must by 65 s.
        summary["timer_a_minute_in"] = timer_in("m", 65)
        device.screenshot("timer-a-minute-in")
        picture = {
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": "image/png",
                "data": b64encode(png(200, 120)).decode(),
            },
        }
        record(
            {
                "type": "user",
                "timestamp": stamp(),
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "Here is the screen"}, picture],
                },
            }
        )
        if not seen("the newer message", contains="Here is the screen"):
            failed("timer", "the newer message never showed")
        # Within 3 s of the message it counts seconds again, from that message.
        summary["timer_after_newer_message"] = after = timer_in("s", 3)
        device.screenshot("timer-after-newer-message")
        if not summary["timer_a_minute_in"].endswith("m"):
            failed("timer", f"a minute in it read {summary['timer_a_minute_in']!r}")
        if not after.endswith("s"):
            failed("timer", f"after a newer message it still read {after!r}")

        # 2. The picture sent with it, and the viewer it opens.
        thumbnail = seen("the sent picture", desc="Picture you sent")
        if thumbnail is None:
            failed(
                "picture",
                "no picture beside the message: "
                + ("'[image]' shown" if device.find(contains="[image]") else "nothing shown"),
            )
        else:
            device.screenshot("sent-picture")
            device.tap(thumbnail)
            if seen("the picture viewer", desc="Close", timeout=20) and device.find(
                desc="Picture you sent"
            ):
                device.screenshot("picture-viewer")
                device.tap(device.find(desc="Close")[0])
            else:
                failed("picture", "tapping it opened no picture")

        # 3. A run of tool calls as one line that opens to the calls.
        calls = [
            (
                "w",
                "Write",
                {"file_path": "/r/notes.md", "content": "one\ntwo\nthree\n"},
                "File created",
                False,
            ),
            (
                "b1",
                "Bash",
                {"command": "cargo build", "description": "Build the gateway"},
                "ok",
                False,
            ),
            (
                "b2",
                "Bash",
                {"command": "cargo test", "description": "Run the gateway tests"},
                "1 failed",
                True,
            ),
            (
                "b3",
                "Bash",
                {"command": "git status", "description": "Show the worktree"},
                "clean",
                False,
            ),
        ]
        for call, name, given, output, error in calls:
            record(
                {
                    "type": "assistant",
                    "timestamp": stamp(),
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "tool_use", "id": call, "name": name, "input": given}],
                    },
                }
            )
            record(
                {
                    "type": "user",
                    "timestamp": stamp(),
                    "message": {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": call,
                                "content": output,
                                "is_error": error,
                            }
                        ],
                    },
                }
            )
        record(
            {
                "type": "assistant",
                "timestamp": stamp(),
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "All four steps ran."}],
                },
            }
        )
        seen("the reply after the calls", contains="All four steps ran.")
        folded = seen(
            "the folded calls", contains="Created a file, ran 3 commands (1 failed)", timeout=10
        )
        device.screenshot("folded-calls")
        cards = len(device.find(desc="Show details"))
        if folded is None:
            failed("calls", f"no folded line; {cards} calls have cards of their own")
        else:
            summary["folded_shows_lines"] = bool(device.find(contains="+3")) and bool(
                device.find(contains="−0")
            )
            if not summary["folded_shows_lines"]:
                failed("calls", "the folded line does not show +3 −0")
            if cards:
                failed("calls", f"{cards} calls still have cards while folded")
            device.tap(folded)
            if seen("the opened calls", contains="Run the gateway tests", timeout=10):
                summary["opened_cards"] = len(device.find(desc="Show details"))
                device.screenshot("opened-calls")
                if summary["opened_cards"] < len(calls):
                    failed(
                        "calls", f"opened, it shows {summary['opened_cards']} of {len(calls)} calls"
                    )
            else:
                failed("calls", "opening the line showed no calls")

        # 4. The summary written after compacting is not the owner's message.
        record({"type": "system", "subtype": "compact_boundary", "timestamp": stamp()})
        record(
            {
                "type": "user",
                "isCompactSummary": True,
                "timestamp": stamp(),
                "message": {
                    "role": "user",
                    "content": "This session is being continued from a previous conversation that ran out of context.\n"
                    "<system-reminder>notes</system-reminder>",
                },
            }
        )
        record(
            {
                "type": "assistant",
                "timestamp": stamp(),
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Picked up where we left off."}],
                },
            }
        )
        if not seen("the reply after compacting", contains="Picked up where we left off."):
            failed("compaction", "the reply after compacting never showed")
        device.screenshot("after-compaction")
        if device.find(contains="This session is being continued"):
            failed("compaction", "the summary shows as the owner's message")

        summary["ok"] = not summary["failures"]
        if summary["failures"]:
            raise m.CheckFailed("; ".join(summary["failures"]))
    except Exception as error:
        summary["error"] = str(error)
        device.screenshot("failure")
        (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
        raise
    finally:
        deck.stop()
        (shots / "claude-app-match-summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
