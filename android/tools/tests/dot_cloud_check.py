#!/usr/bin/env python3
"""Verify cloud grouping and native Dot chat on a disposable pool guest."""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from chat_ui_check import Device  # noqa: E402
from chat_ui_check import assert_private_guest  # noqa: E402
from chat_ui_check import m  # noqa: E402
from device_waits import poll  # noqa: E402

FIXTURE = """
import sys
from pathlib import Path
from types import SimpleNamespace
sys.path.insert(0, HELPER_DIRECTORY)
import cli
from browser import NeedsAttention

class SimulatedDot:
    def __init__(self, state, host="local", **kwargs):
        self.state = state
        self.ready = False
        self.context = None
        self.page = SimpleNamespace(is_closed=lambda: False)
        self.items = [
            {"id": str(i), "role": item["k"], "text": item["text"]}
            for i, item in enumerate(state.read("transcript", []))
            if item["k"] in ("user", "assistant")
        ] or [{"id": "0", "role": "assistant", "text": "Dot bridge ready"}]

    def publish(self):
        self.state.publish([{"k": item["role"], "text": item["text"], "at": None} for item in self.items])

    async def open(self, **kwargs):
        self.ready = True
        self.publish()
        return await self.snapshot()

    async def snapshot(self):
        if (self.state.directory / "verification-needed").exists():
            raise NeedsAttention("Browser verification required. Disconnect with /quit, then use :dot login.")
        return {"messages": self.items, "composer": True, "working": False}

    async def send(self, text):
        self.items.extend([
            {"id": str(len(self.items)), "role": "user", "text": text},
            {"id": str(len(self.items) + 1), "role": "assistant", "text": "Dot received: " + text},
        ])
        self.publish()

    async def close(self):
        self.ready = False

cli.NativeBridge = SimulatedDot
cli.browser_path = lambda: None
raise SystemExit(cli.main())
"""


def home(device):
    device.hide_keyboard()
    device.tap(device.wait("chat menu", desc="Menu"))
    device.tap(device.wait("Home", text="Home"))
    device.wait("local heading", text="Local agents")


def check():
    assert_private_guest()
    m.guard()
    root = Path(tempfile.mkdtemp(prefix="dot-cloud-fixture-")).resolve()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    fake_state = root / "dot-fixture"
    fake_state.mkdir(mode=0o700)
    fixture = root / "dot_fixture.py"
    helper = m.ROOT / "plugins/dot"
    fixture.write_text(FIXTURE.replace("HELPER_DIRECTORY", repr(str(helper))))
    config = json.loads(deck.config.read_text())
    config["lua"] = False
    config["providers"] = [
        {
            "id": "dot",
            "label": "Dot",
            "cloud": True,
            "command": [sys.executable, str(fixture), "chat", "--state-dir", str(fake_state)],
            "chat_snapshot": str(fake_state / "transcript.json"),
            "broadcast": True,
            "working_patterns": ["Dot is working"],
            "prompt_patterns": [],
        }
    ]
    deck.config.write_text(json.dumps(config))
    summary = {}
    base_settle = m.settle

    def settle(current):
        base_settle(current)
        current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
        current.shell(
            "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
        )

    m.settle = settle
    m.chat_before_log = lambda *_: None
    os.environ["CEPHEUS_ONLY"] = "chat"
    try:
        deck.start()
        m.journey(device, deck, Path(os.environ["CEPHEUS_APK"]), summary)
        cloud = device.wait("cloud heading", text="Cloud agents")
        local = device.wait("local heading", text="Local agents")
        assert cloud["bounds"][1] < local["bounds"][1]
        assert len([n for n in device.nodes() if n["text"] == "Dot"]) == 1
        device.screenshot("03-cloud-off")
        device.tap(device.wait("Dot on", text="On"))
        device.wait("native Dot message", contains="Dot bridge ready")
        message = "Hello from the Cepheus app"
        device.type_into(device.fields()[-1], message)
        device.tap(m.wait_enabled(device, "Send"))
        device.wait("native Dot reply", contains="Dot received: " + message)
        assert len([n for n in device.nodes() if n["text"] == message]) == 1
        assert not device.find(desc="Attach")
        device.hide_keyboard()
        device.screenshot("04-native-dot-chat")
        (fake_state / "verification-needed").write_text("synthetic fault")
        device.wait("native browser attention notice", contains="Browser verification required")
        device.screenshot("05-browser-attention")
        (fake_state / "verification-needed").unlink()
        home(device)
        device.wait("cloud heading", text="Cloud agents")
        device.wait("local heading", text="Local agents")
        assert len([n for n in device.nodes() if n["text"] == "Dot"]) == 1
        device.screenshot("06-cloud-on")
        device.tap(device.wait("Dot off", text="Off"))
        assert poll(20, lambda: not any(s["alive"] and s["tool"] == "dot" for s in deck.sessions()))
        device.wait("off Dot remains discoverable", text="On")
        assert len([n for n in device.nodes() if n["text"] == "Dot"]) == 1
        device.screenshot("07-cloud-off-again")
        device.tap(device.wait("Dot on again", text="On"))
        device.wait("same native history", contains="Dot received: " + message)
        records = json.loads((fake_state / "transcript.json").read_text())
        assert sum(item["k"] == "user" and item["text"] == message for item in records) == 1
        summary.update(
            cloud_grouping=True,
            native_reply=True,
            no_duplicate_message=True,
            browser_notice=True,
            on_off_reopen=True,
        )
        (shots / "summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
    finally:
        deck.stop()


if __name__ == "__main__":
    check()
