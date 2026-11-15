#!/usr/bin/env python3
"""Exercise chat UI changes only on a task-owned, disposable Android pool guest."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
import device_check as m  # noqa: E402

from test_support import is_managed_path  # noqa: E402


class Device(m.Device):
    def type_into(self, node: dict, text: str) -> None:
        index = next(
            index for index, field in enumerate(self.fields()) if field["bounds"] == node["bounds"]
        )
        self.tap(node)

        def delete_key(key: int, count: int) -> None:
            # A single giant input command can exceed ADB's deadline on a busy
            # guest. Keep each command short and avoid deleting an empty field.
            for start in range(0, count, 24):
                keys = " ".join([str(key)] * min(24, count - start))
                self.shell("input keyevent " + keys)

        if node["text"]:
            self.shell("input keyevent 123")
            delete_key(67, len(node["text"]) + 2)
            remaining = self.fields()[index]["text"]
            if remaining:
                delete_key(112, len(remaining) + 2)
            if self.fields()[index]["text"]:
                raise m.CheckFailed("The fixture could not clear the focused field")
        for start in range(0, len(text), 16):
            self.shell("input text " + m.shlex.quote(text[start : start + 16].replace(" ", "%s")))
        time.sleep(0.4)


def touch_target(nodes: list[dict], node: dict) -> dict:
    x1, y1, x2, y2 = node["bounds"]
    targets = [
        target
        for target in nodes
        if target["clickable"]
        and target["bounds"][0] <= x1
        and target["bounds"][1] <= y1
        and target["bounds"][2] >= x2
        and target["bounds"][3] >= y2
    ]
    return min(
        targets,
        key=lambda target: (
            (target["bounds"][2] - target["bounds"][0])
            * (target["bounds"][3] - target["bounds"][1])
        ),
    )


def lightest(screenshot: Path, bounds: list[int]) -> int:
    """The highest luminance, 0-255, inside `bounds` of a screenshot."""
    from PIL import Image

    with Image.open(screenshot) as image:
        return image.convert("L").crop(tuple(bounds)).getextrema()[1]


def assert_private_guest() -> None:
    m.guard()


def main() -> None:
    assert_private_guest()
    m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not is_managed_path(shots):
        raise m.CheckFailed("Captures must remain in managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    fixture = Path(tempfile.mkdtemp(prefix="chat-ui-"))
    apk = Path(os.environ["CEPHEUS_APK"])
    m.LOG_DELAY = 30
    m.STAND_IN = m.STAND_IN.replace("time.sleep(8)", "time.sleep(30)").replace(
        "    if not text:",
        '    if text == "end-fixture":\n        break\n    if not text:',
    )
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), fixture)
    with apk.open("rb") as source:
        summary = {"apk_sha256": hashlib.file_digest(source, "sha256").hexdigest()}
    base_settle = m.settle

    def settle_without_voice(current: Device) -> None:
        base_settle(current)
        # The pool image also supplies a TTS voice IME. Disabling Gboard alone
        # makes Android select that voice panel over the pairing fields.
        current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
        current.shell("settings put secure show_ime_with_hard_keyboard 0")

    m.settle = settle_without_voice
    try:
        m.settle(device)
        device.shell(
            "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
        )
        device.shell("am force-stop " + m.PACKAGE, check=False)
        device.shell("pm clear " + m.PACKAGE, check=False)
        device.shell("wm density reset")
        device.shell("settings put system font_scale 1.0")
        device.shell("cmd uimode night no")
        deck.start()
        os.environ["CEPHEUS_ONLY"] = "chat"
        m.journey(device, deck, apk, summary)
        session = int(next(s["id"] for s in deck.sessions() if s["tool"] == "claude"))
        transcript = next((deck.claude_dir / "projects").glob("*/*.jsonl"))

        def append(kind: str, text: str) -> None:
            content = text if kind == "user" else [{"type": "text", "text": text}]
            record = {
                "type": kind,
                "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime()),
                "message": {"role": kind, "content": content},
            }
            with transcript.open("a") as output:
                output.write(json.dumps(record) + "\n")

        for batch in range(1, 4):
            for row in range(2):
                append("user", f"Review batch {batch}, checkpoint {row + 1}.")
                append(
                    "assistant",
                    f"Batch {batch}, checkpoint {row + 1}. "
                    + "Keep the session title readable and the conversation easy to scan. " * 5,
                )
            marker = f"Batch {batch} complete."
            append("assistant", marker)
            device.wait(f"automatic following after batch {batch}", contains=marker)
        summary["three_batches_followed"] = True

        device.shell("input swipe 360 350 360 1100 400")
        device.wait("return-to-latest control", desc="Latest messages")
        append("assistant", "A new response arrived while you were reading older messages.")
        time.sleep(2)
        assert device.find(desc="Latest messages")
        assert not device.find(contains="A new response arrived while")
        device.screenshot("reading-older")
        device.tap(device.wait("return to latest", desc="Latest messages"))
        device.wait("latest response", contains="A new response arrived while")
        assert not device.find(desc="Latest messages")
        summary["reading_position_preserved_and_latest_works"] = True

        code = (
            'def describe():\n    title = "# still a string"\n    count = 42\n'
            "    return title, count\n\n# Keep complete lines when copying.\n"
            "# " + "Text beyond the horizontal viewport; " * 7 + "\n# End of code."
        )
        fence = chr(96) * 3
        append(
            "assistant",
            "## Readable code\n\n"
            + fence
            + "python\n"
            + code
            + "\n"
            + fence
            + "\n\nLatest checkpoint.",
        )
        device.wait("code Copy action", desc="Copy code")
        device.wait("composer model control", contains="Model:")
        device.screenshot("chat-light")
        model = device.wait("model control", contains="Model:")
        assert model["bounds"][1] > device.fields()[-1]["bounds"][1]
        if model["enabled"]:
            device.tap(model)
            device.wait("model selection sheet", text="Model")
            device.screenshot("model-sheet")
            device.shell("input keyevent 4")
            summary["model_sheet_opened"] = True
        else:
            summary["model_sheet_opened"] = "disabled by tool capability"

        device.tap(device.wait("Copy code", desc="Copy code"))
        device.tap(device.fields()[-1])
        device.shell("input keyevent 279")
        assert device.fields()[-1]["text"] == code
        summary["complete_code_copied"] = True
        device.type_into(device.fields()[-1], "")
        device.shell("cmd uimode night yes")
        device.wait("dark composer", contains="Model:")
        dark = device.screenshot("chat-dark")
        # Replies take the theme's text colour. Drawn on a plain page
        # background instead of a Surface, they stayed black on the dark page
        # (emulator capture, 2026-11-01).
        reply = device.wait("dark reply text", text="Latest checkpoint.")
        assert lightest(dark, reply["bounds"]) >= 160, reply
        summary["dark_reply_text_light"] = True
        device.shell("wm density 360")
        device.shell("settings put system font_scale 1.6")
        device.wait("large-font model control", contains="Model:")
        nodes = device.nodes()
        model = next(n for n in nodes if n["desc"].startswith("Model:"))
        attach = next(n for n in nodes if n["desc"] == "Attach")
        dictate = next(n for n in nodes if n["desc"] == "Dictate")
        # One row of controls, as the Claude app's composer: the model pill
        # shortens between + and the mic. A row of its own made the composer
        # three rows tall on the owner's phone (screenshots, 2026-11-01).
        middle = (model["bounds"][1] + model["bounds"][3]) // 2
        assert attach["bounds"][1] <= middle <= attach["bounds"][3], (attach, model)
        assert attach["bounds"][2] <= model["bounds"][0], (attach, model)
        assert model["bounds"][2] <= dictate["bounds"][0], (model, dictate)
        for label in ("Attach", "Dictate", "Send"):
            node = next(n for n in nodes if n["desc"] == label)
            target = touch_target(nodes, node)
            x1, y1, x2, y2 = target["bounds"]
            assert 0 <= x1 < x2 <= 720 and y1 < y2 <= 1560, (label, target)
            assert x2 - x1 >= 100 and y2 - y1 >= 100, (label, target)
        device.screenshot("chat-narrow-large-text")
        summary["narrow_320dp_text_160_percent"] = True

        device.shell("pm enable --user 0 com.google.android.inputmethod.latin", check=False)

        def latin_ime() -> str | None:
            return next(
                (ime for ime in device.shell("ime list -a -s").split() if "latin" in ime.lower()),
                None,
            )

        m.wait_for("Gboard to register its input method", 30, latin_ime)
        latin = latin_ime()
        assert latin is not None
        device.shell("ime enable " + m.shlex.quote(latin))
        device.shell("ime set " + m.shlex.quote(latin))
        device.shell("settings put secure show_ime_with_hard_keyboard 1")
        device.tap(device.fields()[-1])
        device.shell("input keyevent 279")
        assert device.fields()[-1]["text"] == code
        m.wait_for(
            "visible keyboard",
            20,
            lambda: (
                "mInputShown=true" in device.shell("dumpsys input_method")
                or "mIsInputViewShown=true" in device.shell("dumpsys input_method")
            ),
        )
        device.wait("Send with the long draft and keyboard", desc="Send")
        device.screenshot("chat-long-draft-keyboard")
        summary["long_draft_with_keyboard"] = True
        device.type_into(device.fields()[-1], "")
        device.shell("input keyevent 4")
        m.settle(device)
        device.shell("settings put system font_scale 1.0")
        device.shell("wm density reset")
        device.shell("cmd uimode night no")
        device.tap(device.wait("the + menu", desc="Attach"))
        device.tap(device.wait("skills control", text="Skills and commands"))
        device.type_into(device.fields()[-1], "/mod")
        device.tap(device.wait("model command", text="/model"))
        assert device.fields()[-1]["text"] == "/model "
        device.type_into(device.fields()[-1], "")
        summary["slash_command_draft"] = True

        def stop_hook(tasks: list[dict]) -> None:
            # What Claude Code's Stop hook hands the deck as a turn ends.
            payload = {"hook_event_name": "Stop", "background_tasks": tasks, "session_crons": []}
            subprocess.run(
                [str(deck.binary), "hook", "claude"],
                input=json.dumps(payload),
                text=True,
                check=True,
                timeout=10,
                env={**deck.env, "CEPHEUS_SESSION": str(session)},
            )

        # A turn that ended with a shell still running says so instead of
        # "Thinking…" (owner, 2026-11-01: "it still shows thinking when your
        # waiting on a shell").
        shell = {"id": "b1", "type": "shell", "status": "running", "description": "Serve the APK"}
        stop_hook([shell])
        device.wait("the wait on a background shell", text="Waiting on a shell")
        assert not device.find(text="Thinking…")
        device.screenshot("waiting-on-shell")
        stop_hook([])
        m.wait_for("the wait to end", 30, lambda: not device.find(text="Waiting on a shell"))
        summary["waiting_on_shell_shown"] = True

        m.send_from_app(device, deck, session, "end-fixture")
        device.wait("ended session", text="This session has ended.")
        device.wait("ended-session model metadata", contains="Model:")
        device.screenshot("ended-session")
        device.tap(device.wait("Reopen", text="Reopen"))
        device.wait("reopened composer", timeout=45, cls="android.widget.EditText")
        summary["ended_model_and_reopen"] = True
        summary["ok"] = True
    except Exception as error:
        summary.update(ok=False, error=str(error))
        device.screenshot("failure")
        (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
        raise
    finally:
        deck.stop()
        (shots / "summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
        shutil.rmtree(fixture, ignore_errors=True)


if __name__ == "__main__":
    main()
