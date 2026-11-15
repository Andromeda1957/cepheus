#!/usr/bin/env python3
"""Live closed-app alerts without a foreground notice on a battery-exempt guest.

Run under a disposable emulator with CEPHEUS_BIN, CEPHEUS_APK and
CEPHEUS_SHOTS. All sessions and device state belong to the fixture.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import device_check as m  # noqa: E402
from chat_ui_check import Device  # noqa: E402
from chat_ui_check import assert_private_guest  # noqa: E402

from test_support import scaled  # noqa: E402
from test_support import is_managed_path  # noqa: E402

# Notifier.FOREGROUND_ID.
NOTICE_ID = 1


def records(dump: str) -> list[str]:
    """The posted notifications of `dumpsys notification`, one block each.

    Only the "Notification List" holds what is showing; the archive further
    down repeats removed ones, so has_notification() cannot tell a gone notice.
    """
    lines = dump.splitlines()
    start = next((i for i, line in enumerate(lines) if line.strip() == "Notification List:"), None)
    if start is None:
        return []
    blocks: list[list[str]] = []
    indent = 0
    for line in lines[start + 1 :]:
        stripped = line.lstrip()
        if stripped.startswith("NotificationRecord("):
            indent = len(line) - len(stripped)
            blocks.append([line])
        elif blocks and stripped and len(line) - len(stripped) > indent:
            blocks[-1].append(line)
        elif stripped:
            break
    return ["\n".join(block) for block in blocks]


def notice(device: Device) -> str | None:
    """The foreground service's notice as `dumpsys` shows it, or None."""
    for block in records(device.notifications()):
        head = block.splitlines()[0]
        if f"pkg={m.PACKAGE} " in head and f" id={NOTICE_ID} " in head and "tag=null" in head:
            return block
    return None


def needs_you_showing(device: Device, session: int) -> bool:
    return any(
        f"pkg={m.PACKAGE} " in block.splitlines()[0] and "tag=needs_you" in block.splitlines()[0]
        and f" id={session} " in block.splitlines()[0]
        for block in records(device.notifications())
    )


def check(device: Device, deck: m.Deck, apk: Path, summary: dict) -> None:
    settle = m.settle

    def settle_without_voice(current: Device) -> None:
        settle(current)
        current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
        current.shell("settings put secure show_ime_with_hard_keyboard 0")

    m.settle = settle_without_voice
    m.stats = lambda device, summary: None
    # The journey grants the same exemption as Android's battery dialog.
    m.journey(device, deck, apk, summary, stats_only=True)
    m.background_on(device, deck, summary)
    device.shell("input keyevent 3")
    m.wait_for("the background connection", scaled(30), lambda: m.gateway_connections(deck.port) == 1)
    if notice(device) is not None:
        raise m.CheckFailed("battery-exempt connection still posts a service notice")
    summary["no_service_notice"] = True
    device.shell("dumpsys battery unplug")
    try:
        device.shell("input keyevent 223")
        device.shell("dumpsys deviceidle force-idle")
        device.shell(f"am make-uid-idle {m.PACKAGE}")
        summary["idle_state"] = device.shell("dumpsys deviceidle get deep").strip()
        for count in range(1, 4):
            deck.ctl("new", "grok")
            m.wait_for("the new session", scaled(40), lambda count=count: sum(
                1 for s in deck.sessions() if s["alive"] and s["status"] != "starting"
            ) == count)
            session = max(s["id"] for s in deck.sessions() if s["alive"])
            m.wait_for("the idle session", scaled(30), lambda session=session: m.status_of(deck, session) != "working")
            deck.ctl("send", str(session), "please ask me\\r")
            m.wait_for("the closed app's alert", scaled(60), lambda session=session: needs_you_showing(device, session))
            if notice(device) is not None or not m.service_running(device) or \
                    "isForeground=true" in device.shell(f"dumpsys activity services {m.PACKAGE}"):
                raise m.CheckFailed("alert delivery lost its service or restored its notice")
        summary["three_live_alerts_in_doze_without_notice"] = True
        device.screenshot("alerts-without-service-notice")
    finally:
        device.shell("dumpsys deviceidle unforce", check=False)
        device.shell("dumpsys battery reset", check=False)
        device.shell("input keyevent 224", check=False)
        device.shell("wm dismiss-keyguard", check=False)

    # Readers who decline the exemption retain the foreground fallback.
    # Granting it afterwards removes that notice while keeping delivery.
    for exempt in (False, True):
        sign = "+" if exempt else "-"
        device.shell(f"dumpsys deviceidle whitelist {sign}{m.PACKAGE}")
        device.shell("input keyevent 3")
        # Home is asynchronous: relaunching before onStop runs can keep the
        # activity started, without the onStart that rechecks battery permission.
        def app_resumed() -> bool:
            return any(
                ("mResumedActivity:" in line or "topResumedActivity=" in line) and f"{m.PACKAGE}/" in line
                for line in device.shell("dumpsys activity activities").splitlines()
            )

        m.wait_for("Home before the permission transition", scaled(25), lambda: not app_resumed())
        device.shell(f"am start -W -n {m.PACKAGE}/{m.ACTIVITY}")
        m.wait_for("the resumed app", scaled(25), app_resumed)
        try:
            # Android may suppress a foreground notice; the service state is
            # authoritative for whether the required fallback was promoted.
            m.wait_for(f"the service mode (battery exempt={exempt})", scaled(25), lambda exempt=exempt: (
                ("isForeground=true" not in device.shell(f"dumpsys activity services {m.PACKAGE}")) == exempt
                and (not exempt or notice(device) is None)
            ))
        except m.CheckFailed:
            summary["failed_transition"] = {
                "battery_exempt": exempt,
                "whitelist": device.shell("dumpsys deviceidle whitelist"),
                "service": device.shell(f"dumpsys activity services {m.PACKAGE}"),
                "service_notice": notice(device),
            }
            raise
        if not m.service_running(device):
            raise m.CheckFailed("changing battery permission stopped delivery")
    summary["optimized_fallback_and_exempt_transition"] = True


def main() -> int:
    assert_private_guest()
    m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not is_managed_path(shots):
        raise m.CheckFailed("Captures must remain in managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    apk = Path(os.environ["CEPHEUS_APK"])
    with apk.open("rb") as source:
        summary: dict = {"apk_sha256": hashlib.file_digest(source, "sha256").hexdigest()}
    fixture = Path(tempfile.mkdtemp(prefix="notice-"))
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), fixture)
    ok = False
    try:
        deck.start()
        check(device, deck, apk, summary)
        ok = True
    except (m.CheckFailed, subprocess.SubprocessError, OSError, StopIteration, ValueError) as error:
        summary["error"] = f"{type(error).__name__}: {error}"
        m.log(f"FAILED: {summary['error']}")
        try:
            device.screenshot("failure")
            (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
            (shots / "failure-notifications.txt").write_text(device.notifications())
        except (m.CheckFailed, subprocess.SubprocessError, OSError):
            pass
    finally:
        deck.stop()
        summary["ok"] = ok
        (shots / "summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
        shutil.rmtree(fixture, ignore_errors=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
