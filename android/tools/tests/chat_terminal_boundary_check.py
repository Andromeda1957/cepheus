#!/usr/bin/env python3
"""Verify agent chats reject PTY snapshots across live, peer and offline paths."""

from __future__ import annotations

import hashlib
import json
import os
import re
import secrets
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import chat_ui_check as ui
import deck_tools_check as tools
from first_message_check import link_codex

from test_support import is_managed_path
from test_support import storage_root

m = ui.m


class Device(ui.Device):
    def hide_keyboard(self) -> None:
        state = re.search(r"^\s*mInputShown=(true|false)\b", self.shell("dumpsys input_method"), re.M)
        if state and state[1] == "true":
            self.shell("input keyevent 4")


class Journey(tools.Journey):
    def home(self) -> None:
        self.d.hide_keyboard()
        for _ in range(10):
            if self.d.find(desc="Menu"):
                self.d.tap(self.d.wait("Menu", desc="Menu"))
                self.d.tap(self.d.wait("Home", text="Home"))
                self.d.wait("Sessions", text="Sessions")
                return
            back = self.d.find(desc="Back")
            if back:
                self.d.tap(back[0])
            else:
                self.d.shell("input keyevent 4")
        raise m.CheckFailed("Could not return Home through visible navigation")


def no_terminal(device: Device, *, missing: bool = False) -> None:
    nodes = device.nodes()
    if not nodes:
        raise m.CheckFailed("Could not inspect the visible conversation")
    markers = ("stand-in agent ready", "you said:", "LEGACY_PTY_LEAK")
    if any(marker in node["text"] for node in nodes for marker in markers):
        raise m.CheckFailed("Raw terminal content appeared in the conversation")
    if any(node["text"] == "Terminal" for node in nodes):
        raise m.CheckFailed("The conversation rendered a terminal block")
    if missing and any(node["desc"] == "Dictate" for node in nodes):
        raise m.CheckFailed("A removed session retained its reply composer")


def open_chat(device: Device, session: int, title: str) -> None:
    # The same entry point used by a notification, including tagged peer ids.
    device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY} --el session {session}")
    device.wait("selected conversation", timeout=60, text=title)


def unavailable(device: Device) -> None:
    device.wait("native transcript unavailable state", timeout=60, contains="can't be read yet")
    no_terminal(device)


def cache_path(device: Device, session: int) -> str | None:
    paths = device.shell(
        f"run-as {m.PACKAGE} find no_backup -type f -name chat-{session}.json",
        check=False,
    ).splitlines()
    for path in paths:
        if path.startswith("no_backup/deck-cache") and Path(path).name == f"chat-{session}.json":
            return path
    return None


def cached(device: Device, session: int) -> dict | None:
    path = cache_path(device, session)
    if path is None:
        return None
    try:
        return json.loads(device.shell(f"run-as {m.PACKAGE} cat {shlex.quote(path)}"))
    except json.JSONDecodeError:
        return None


def peers(hub: m.Deck, peer: m.Deck) -> None:
    key = secrets.token_hex(32)
    keyfile = hub.state / "hub/key"
    keyfile.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    keyfile.write_text(key)
    keyfile.chmod(0o600)
    port = m.free_port()
    config = json.loads(peer.config.read_text())
    config.update(
        host_name="Fedora",
        peer_listen=f"127.0.0.1:{port}",
        peer_allow=["127.0.0.1"],
        sync_logins=False,
    )
    peer.config.write_text(json.dumps(config))
    subprocess.run(
        [str(peer.binary), "peer", "authorize"],
        input=key, text=True, capture_output=True, env=peer.env, check=True, timeout=30,
    )
    fingerprint = subprocess.check_output(
        [str(peer.binary), "peer", "fingerprint"], text=True, env=peer.env, timeout=30,
    ).strip()
    hub.config.with_name("hosts.json").write_text(json.dumps({"hosts": [{
        "name": "fedora", "label": "Fedora", "tag": 1,
        "addresses": [f"127.0.0.1:{port}"], "fingerprint": fingerprint, "ssh": None,
    }]}))


def check(device: Device, hub: m.Deck, peer: m.Deck, summary: dict) -> None:
    sid = int(hub.ctl("new", "--name", "Codex transcript fixture", "codex"))
    m.wait_for("unlinked Codex PTY", 30, lambda: "stand-in agent ready" in hub.screen(sid))
    open_chat(device, sid, "Codex transcript fixture")
    unavailable(device)
    device.screenshot("chat-unlinked-codex")
    summary["unlinked_codex_pty_hidden"] = True

    rollouts = Path(hub.env["CODEX_HOME"]) / "sessions"
    before = set(rollouts.rglob("rollout-test-*.jsonl"))
    link_codex(hub, sid, "native-linked")
    device.wait("native linked reply", timeout=60, contains="Native answer: native-linked")
    no_terminal(device)
    summary["native_transcript_replaces_unavailable_state"] = True
    created = set(rollouts.rglob("rollout-test-*.jsonl")) - before
    if len(created) != 1:
        raise m.CheckFailed("The fixture did not create exactly one Codex rollout")
    rollout = created.pop()
    rollout.rename(rollout.with_suffix(".unavailable"))
    unavailable(device)
    m.wait_for(
        "real screen fallback saved by the gateway",
        30,
        lambda: any(item.get("k") == "screen" for item in (cached(device, sid) or {}).get("items", [])),
    )
    device.screenshot("chat-lost-transcript")
    device.shell(f"am force-stop {m.PACKAGE}")
    open_chat(device, sid, "Codex transcript fixture")
    unavailable(device)
    summary["lost_log_reset_cache_and_reconnect_hidden"] = True

    grok = int(hub.ctl("new", "--name", "Grok transcript fixture", "grok"))
    open_chat(device, grok, "Grok transcript fixture")
    unavailable(device)
    message = "please work long"
    m.send_from_app(device, hub, grok, message)
    m.wait_for("Grok message on its actual PTY", 30, lambda: message in hub.screen(grok))
    m.wait_for("Grok native outgoing bubble", 30, lambda: device.chat_messages(message))
    no_terminal(device)
    device.screenshot("chat-grok-native-outgoing")
    summary["screen_provider_outgoing_bubble"] = True

    dynamic = int(hub.ctl("new", "--name", "Dynamic transcript fixture", "fixture-agent"))
    open_chat(device, dynamic, "Dynamic transcript fixture")
    unavailable(device)
    summary["dynamic_provider_pty_hidden"] = True

    remote = int(hub.ctl("new", "--name", "Peer close fixture", "codex", "fedora"))
    if remote < 1 << 40:
        raise m.CheckFailed("The close fixture is not a tagged peer session")
    open_chat(device, remote, "Peer close fixture")
    unavailable(device)
    device.tap(device.wait("session menu", desc="More"))
    device.tap(device.wait("close action", text="Close session"))
    device.tap(device.wait("confirmed close", text="Close"))
    m.wait_for("peer session removed", 30, lambda: all(s["id"] != remote for s in hub.sessions()))
    device.wait("Home after close", timeout=60, text="Sessions")
    no_terminal(device, missing=True)
    device.screenshot("chat-closed-peer-home")
    summary["close_peer_returns_home"] = True

    external = int(hub.ctl("new", "--name", "External close fixture", "codex", "fedora"))
    open_chat(device, external, "External close fixture")
    unavailable(device)
    peer.ctl("close", str(external & ((1 << 40) - 1)))
    m.wait_for("external peer removal", 30, lambda: all(s["id"] != external for s in hub.sessions()))
    device.wait("removed session controls", timeout=60, text="Session unavailable")
    no_terminal(device, missing=True)
    device.screenshot("chat-removed-session")
    summary["external_removal_has_no_pty_or_composer"] = True

    ended = int(hub.ctl("new", "--name", "Ended transcript fixture", "codex"))
    link_codex(hub, ended, "ended-native")
    open_chat(device, ended, "Ended transcript fixture")
    device.wait("native conversation before exit", timeout=60, contains="Native answer: ended-native")
    session = next(s for s in hub.sessions() if s["id"] == ended)
    pid = int(session["pid"])
    marker = f"CEPHEUS_STATE_DIR={hub.state}".encode()
    if marker not in Path(f"/proc/{pid}/environ").read_bytes().split(b"\0"):
        raise m.CheckFailed("Refusing to end a process outside the fixture")
    os.kill(pid, signal.SIGTERM)
    device.wait("retained ended conversation", timeout=60, text="Reopen")
    device.wait("native reply after exit", contains="Native answer: ended-native")
    no_terminal(device)
    summary["naturally_ended_native_conversation_retained"] = True

    journey = Journey(device, hub, summary)
    journey.tools("Terminals")
    journey.tap("Open terminal")
    device.wait("dedicated terminal", timeout=60, desc="Terminal screen. Tap to type.")
    journey.type_terminal('echo dedicated-\"terminal-preserved\"')
    device.wait("dedicated terminal output", timeout=30, contains="dedicated-terminal-preserved")
    device.screenshot("dedicated-terminal-preserved")
    summary["dedicated_terminal_still_receives_raw_screen_and_input"] = True

    # Replay a legacy mixed cache with a misleading source label while offline.
    m.wait_for("Grok cache file", 30, lambda: cache_path(device, grok))
    path = cache_path(device, grok)
    assert path is not None
    payload = json.dumps({
        "id": grok, "start": 20, "total": 23, "generation": 7, "source": "log",
        "items": [
            {"k": "screen", "text": "LEGACY_PTY_LEAK\n› terminal prompt"},
            {"k": "user", "text": "Cached native question"},
            {"k": "assistant", "text": "Cached native answer"},
        ],
    })
    device.shell(f"am force-stop {m.PACKAGE}")
    command = f"cat > {shlex.quote(path)}"
    subprocess.run(
        [device.adb_path, "-s", device.serial, "shell",
         f"run-as {m.PACKAGE} sh -c {shlex.quote(command)}"],
        input=payload, text=True, capture_output=True, check=True, timeout=30,
    )
    hub.stop()
    open_chat(device, grok, "Grok transcript fixture")
    device.wait("cached native reply", timeout=60, contains="Cached native answer")
    device.wait("cached native question", contains="Cached native question")
    no_terminal(device)
    device.screenshot("chat-legacy-offline-cache")
    summary["legacy_mixed_offline_cache_hides_pty_and_preserves_messages"] = True


def main() -> None:
    ui.assert_private_guest()
    m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not is_managed_path(shots):
        raise m.CheckFailed("Captures must remain in managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="chat-terminal-boundary-", dir=storage_root()))
    apk = Path(os.environ["CEPHEUS_APK"])
    binary = Path(os.environ["CEPHEUS_BIN"])
    hub, peer = m.Deck(binary, root / "hub"), m.Deck(binary, root / "peer")
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    summary = {
        "apk_sha256": hashlib.sha256(apk.read_bytes()).hexdigest(),
        "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
    }
    try:
        config = json.loads(hub.config.read_text())
        config.update(
            host_name="Phone fixture", sync_logins=False,
            shell_command=["/bin/sh"],
            providers=[{"id": "fixture-agent", "label": "Fixture agent",
                        "command": [sys.executable, str(hub.root / "stand_in.py")]}],
        )
        hub.config.write_text(json.dumps(config))
        hub.env["SHELL"] = "/bin/sh"
        peers(hub, peer)
        peer.start()
        hub.seed_usage()
        hub.start()
        m.journey(device, hub, apk, summary, stats_only=True)
        check(device, hub, peer, summary)
        summary["result"] = "pass"
    except Exception as error:
        summary.update(result="fail", error=str(error))
        device.screenshot("chat-terminal-failure")
        raise
    finally:
        hub.stop()
        peer.stop()
        (shots / "terminal-chat-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
