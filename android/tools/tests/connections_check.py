#!/usr/bin/env python3
"""Pair three private gateways, lose the first, and start sessions on the others.

Run through a disposable emulator with CEPHEUS_BIN and CEPHEUS_APK. The
gateway fixtures, fake agents and screenshots belong to managed scratch;
this never stops or signs in to any of the owner's decks.
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path

import chat_ui_check as ui
from device_waits import poll

from test_support import is_managed_path
from test_support import storage_root

m = ui.m


def pair(device: ui.Device, deck: m.Deck, name: str) -> None:
    device.wait("pairing form", contains="Pair this phone")
    device.hide_keyboard()
    values = ["10.0.2.2", str(deck.port), deck.fingerprint(), deck.open_pairing()]
    for index, value in enumerate(values):
        device.type_into(device.fields()[index], value)
        device.hide_keyboard()
    m.scroll_to(device, "connection name", contains="Connection name (optional)")
    device.type_into(device.fields()[-1], name)
    device.hide_keyboard()
    for _ in range(4):
        if device.find(text="Pair"):
            break
        device.shell("input swipe 360 1100 360 500 300")
    device.tap(device.wait("Pair button", text="Pair"))
    device.wait("sessions after pairing", timeout=45, text="Local agents")


def connections(device: ui.Device) -> None:
    if device.find(text="Connections"):
        device.tap(device.wait("offline connection picker", text="Connections"))
    else:
        device.tap(device.wait("Menu", desc="Menu"))
        device.tap(device.wait("current connection", contains="Connection: "))
    device.wait("saved connection list", text="Saved connections")


def select(device: ui.Device, name: str) -> None:
    connections(device)
    device.tap(device.wait(f"saved {name} connection", text=name))
    device.wait("session list", text="Local agents")


def start_session(device: ui.Device, deck: m.Deck, name: str) -> int:
    before = {session["id"] for session in deck.sessions()}
    device.tap(device.wait("New session", desc="New session"))
    device.tap(device.wait("Codex", text="Codex"))
    device.tap(device.wait("Advanced session options", text="Advanced"))
    label = m.scroll_to(device, "first message", contains="First message")
    x = (label["bounds"][0] + label["bounds"][2]) // 2
    y = label["bounds"][1]
    field = next(
        (
            field
            for field in device.fields()
            if field["bounds"][0] <= x <= field["bounds"][2]
            and field["bounds"][1] <= y <= field["bounds"][3]
        ),
        None,
    )
    if field is None:
        raise m.CheckFailed("Advanced options did not expose the first message field")
    device.type_into(field, name)
    device.hide_keyboard()
    device.tap(device.wait("Done with Advanced options", text="Done"))
    button = poll(
        45, lambda: next((node for node in device.find(text="Start") if node["enabled"]), None)
    )
    if not button:
        raise m.CheckFailed("Start did not become available on the selected independent gateway")
    device.tap(button)
    created = poll(45, lambda: {session["id"] for session in deck.sessions()} - before)
    if not created:
        raise m.CheckFailed("The phone did not start a session on its selected gateway")
    session = max(created)
    device.wait("session chat", desc="More")
    if not poll(40, lambda: name in deck.screen(session)):
        raise m.CheckFailed("The first prompt did not reach the selected gateway's PTY")
    return session


def main() -> None:
    ui.assert_private_guest()
    m.guard()
    apk = Path(os.environ["CEPHEUS_APK"])
    binary = Path(os.environ["CEPHEUS_BIN"])
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not is_managed_path(shots):
        raise m.CheckFailed("Screenshots require managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    print(f"Connection review screenshots: {shots}", flush=True)
    device = ui.Device(os.environ["ANDROID_SERIAL"], shots)
    summary: dict = {}
    with tempfile.TemporaryDirectory(prefix="deck-connections-", dir=storage_root()) as temporary:
        root = Path(temporary)
        decks = {
            name: m.Deck(binary, root / name.lower()) for name in ["Omarchy", "Fedora", "Ubuntu"]
        }
        try:
            for name, deck in decks.items():
                config = json.loads(deck.config.read_text())
                config["host_name"] = name
                deck.config.write_text(json.dumps(config))
                deck.start()
                deck.ctl("new", "--name", f"{name} kept", "codex")
            m.settle(device)
            device.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            device.shell(
                "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
            )
            device.adb("install", "-r", "-g", str(apk), timeout=300)
            if device.shell(f"pm clear {m.PACKAGE}").strip() != "Success":
                raise m.CheckFailed("Could not reset the private app installation")
            device.shell(f"pm grant {m.PACKAGE} android.permission.POST_NOTIFICATIONS", check=False)
            device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
            for index, (name, deck) in enumerate(decks.items()):
                if index:
                    connections(device)
                    device.tap(device.wait("Add connection", text="Add connection"))
                pair(device, deck, name)
                device.wait(f"{name}'s independent session list", text=f"{name} kept")
            # Cold opening keeps the active pairing and every saved alternative.
            device.shell(f"am force-stop {m.PACKAGE}")
            device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
            device.wait("last selected connection after restart", text="Ubuntu kept")
            connections(device)
            for name in decks:
                device.wait(f"persisted {name} connection", text=name)
            device.screenshot("01-saved-connections")
            device.tap(device.wait("Omarchy", text="Omarchy"))
            device.wait("Omarchy session list", text="Omarchy kept")
            decks["Omarchy"].stop()
            device.wait("unreachable Omarchy", timeout=45, contains="Reconnecting")
            device.screenshot("02-omarchy-unreachable")
            for name in ["Fedora", "Ubuntu"]:
                select(device, name)
                device.wait(f"{name} session list", text=f"{name} kept")
                message = f"independent {name} prompt"
                started = start_session(device, decks[name], message)
                other = "Ubuntu" if name == "Fedora" else "Fedora"
                if message in decks[other].screen(1):
                    raise m.CheckFailed("A prompt crossed gateway boundaries")
                summary[name] = {"session": started, "started_without_omarchy": True}
                device.screenshot(f"03-{name.lower()}-chat")
                if name == "Fedora":
                    draft = "Fedora unsent draft"
                    device.type_into(device.fields()[-1], draft)
                    device.hide_keyboard()
                    connections(device)
                    device.tap(device.wait("Back from connection picker", desc="Back"))
                    device.wait("same chat after cancelling picker", desc="More")
                    if not any(draft in field["text"] for field in device.fields()):
                        raise m.CheckFailed("Opening the picker discarded the current chat draft")
                    summary["picker_keeps_unsent_draft"] = True
            summary["saved_connections_survive_restart"] = True
            summary["ok"] = True
        except BaseException:
            device.screenshot("failure")
            (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
            raise
        finally:
            for deck in decks.values():
                deck.stop()
            (shots / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == "__main__":
    main()
