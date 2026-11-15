#!/usr/bin/env python3
"""Verify pending first prompts on the staging UI with a private gateway and pool guest."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

fixture_tools = Path(os.environ.get("CEPHEUS_FIXTURE_TOOLS", Path(__file__).parent))
sys.path.insert(0, str(fixture_tools))
from chat_ui_check import Device  # noqa: E402
from chat_ui_check import assert_private_guest  # noqa: E402
from chat_ui_check import m  # noqa: E402
from device_waits import poll  # noqa: E402

from test_support import is_managed_path  # noqa: E402

# Seconds the first prompt is watched for its terminal or a lost bubble. The
# three looks a second apart this replaces spanned about as long: each look is
# three UI dumps of a second or more.
FIRST_PROMPT_WATCH = 10


def link_codex(deck, session, message, conversation=None):
    conversation = conversation or str(uuid.uuid4())
    path = (
        Path(deck.env["CODEX_HOME"]) / "sessions/2026/11/03" / f"rollout-test-{conversation}.jsonl"
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    rows = [
        {"type": "session_meta", "payload": {"id": conversation, "cwd": str(deck.root / "work")}},
        {"type": "event_msg", "payload": {"type": "user_message", "message": message}},
        {
            "type": "event_msg",
            "payload": {"type": "agent_message", "message": f"Native answer: {message}"},
        },
    ]
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))
    subprocess.run(
        [
            str(deck.binary),
            "hook",
            "codex",
            json.dumps({"type": "agent-turn-complete", "thread-id": conversation}),
        ],
        env={**deck.env, "CEPHEUS_SESSION": str(session)},
        check=True,
    )


def check(root: Path, build_apk: bool):
    artifacts = Path(os.environ.get("CEPHEUS_ANDROID_ARTIFACTS", os.environ["TMPDIR"]))
    apk = Path(os.environ.get("CEPHEUS_APK", artifacts.parent / "candidate.apk"))
    binary = Path(os.environ.get("CEPHEUS_BIN", Path.home() / ".local/bin/cepheus"))
    shots = Path(os.environ.get("CEPHEUS_SHOTS", artifacts / "first-message"))
    if not is_managed_path(shots):
        raise m.CheckFailed("Captures must remain in managed scratch")
    if build_apk:
        android = root / "android"
        shutil.copytree(
            m.ROOT / "android",
            android,
            ignore=shutil.ignore_patterns("build", ".gradle", ".kotlin", "local.properties"),
        )
        subprocess.run(
            [
                "./gradlew",
                "--no-daemon",
                "--max-workers=1",
                "-Dorg.gradle.jvmargs=-Xmx1024m",
                "-Pkotlin.compiler.execution.strategy=in-process",
                "assembleDebug",
            ],
            cwd=android,
            timeout=1200,
            check=True,
        )
        apk = android / "app/build/outputs/apk/debug/app-debug.apk"
    if not apk.is_file() or not binary.is_file():
        raise m.CheckFailed("Provide the candidate APK and isolated gateway binary")
    shots.mkdir(parents=True, exist_ok=True)
    # Keep the terminal fallback active long enough for several real UI captures.
    m.STAND_IN = m.STAND_IN.replace(
        '    out("thinking (esc to interrupt)")',
        '    out("thinking (esc to interrupt)")\n    time.sleep(90)',
    )
    deck = m.Deck(binary, root)
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    with apk.open("rb") as source:
        summary = {"apk_sha256": hashlib.file_digest(source, "sha256").hexdigest()}
    with binary.open("rb") as source:
        summary["gateway_sha256"] = hashlib.file_digest(source, "sha256").hexdigest()
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
        # Reuse the real pairing flow, then run the focused journey below.
        m.chat_before_log = lambda *_: None
        os.environ["CEPHEUS_ONLY"] = "chat"
        m.journey(device, deck, apk, summary)
        for index, (tool, first_in_dialog) in enumerate(
            [("Codex", True), ("Codex", False), ("Antigravity", True)]
        ):
            message = f"first prompt case {index + 1}"
            before = {s["id"] for s in deck.sessions()}
            device.tap(device.wait("New session", text="New session"))
            device.tap(device.wait(f"{tool} chip", text=tool))
            if first_in_dialog:
                device.type_into(device.fields()[-1], message)
                device.hide_keyboard()
            device.tap(device.wait("Start", text="Start"))
            m.wait_for(
                "new session", 40, lambda before=before: {s["id"] for s in deck.sessions()} - before
            )
            session = max({s["id"] for s in deck.sessions()} - before)
            device.wait("session chat", timeout=60, desc="More")
            if not first_in_dialog:
                m.send_from_app(device, deck, session, message)
            m.wait_for(
                "native pending bubble", 15, lambda message=message: device.chat_messages(message)
            )
            device.wait("native thinking", timeout=15, contains="Thinking")
            m.wait_for(
                "prompt reached real PTY",
                20,
                lambda message=message, session=session: message in deck.screen(session),
            )

            def pending_problem(tool=tool, index=index, message=message) -> str | None:
                if device.find(text="Terminal") or device.find(contains="you said:"):
                    return f"{tool} case {index + 1} displayed its terminal during the first prompt"
                if len(device.chat_messages(message)) != 1:
                    return f"{tool} case {index + 1} lost or duplicated its pending bubble"
                return None

            problem = poll(FIRST_PROMPT_WATCH, pending_problem)
            if problem:
                raise m.CheckFailed(problem)
            device.screenshot(f"case-{index + 1}-pending")
            summary[f"case_{index + 1}_native_pending"] = True
            if tool == "Codex":
                link_codex(deck, session, message)
                device.wait(
                    "native transcript answer", timeout=30, contains=f"Native answer: {message}"
                )
                if len(device.chat_messages(message)) != 1 or device.find(text="Terminal"):
                    raise m.CheckFailed(
                        "The linked transcript lost or duplicated the message or retained Terminal"
                    )
                device.screenshot(f"case-{index + 1}-linked")
                summary[f"case_{index + 1}_native_handoff"] = True
            device.shell("input keyevent 4")
            device.wait("sessions", text="Local agents")
        summary["ok"] = True
    except Exception as error:
        summary["error"] = str(error)
        device.screenshot("failure")
        (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
        raise
    finally:
        deck.stop()
        for name in ("service.log", "state/remote.log"):
            log = root / name
            if log.exists():
                shutil.copyfile(log, shots / log.name)
        (shots / "first-message-summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-apk", action="store_true", help="Build this checkout in scratch")
    args = parser.parse_args()
    assert_private_guest()
    m.guard()
    with tempfile.TemporaryDirectory(prefix="first-message-") as root:
        check(Path(root), args.build_apk)


if __name__ == "__main__":
    main()
