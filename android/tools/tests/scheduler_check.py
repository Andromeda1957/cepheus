#!/usr/bin/env python3
"""Native Scheduler journey against two private, federated services and fake agents.

Use a disposable emulator with CEPHEUS_BIN, CEPHEUS_APK and managed
CEPHEUS_SHOTS. No owner service, account, device or reserved port is used.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import chat_ui_check as ui
from connections_check import pair
from deck_tools_check import Journey
from device_waits import at_rest
from device_waits import poll

from test_support import is_managed_path
from test_support import short_path
from test_support import storage_root

m = ui.m
RERUN_COMMAND = "cargo test"
NON_OWNER_OPEN = '<cross-session-message source="cepheus" encoding="html">'
NON_OWNER_CLOSE = "</cross-session-message>"


def fleet(deck: m.Deck, operation: str = "status", **args) -> dict:
    return json.loads(deck.ctl("fleet", operation, json.dumps(args)))


def own_ids(deck: m.Deck) -> set[int]:
    return {session["id"] for session in deck.sessions() if not session.get("host")}


def require(what: str, check, seconds: float = 60):
    result = poll(seconds, check)
    if not result:
        raise m.CheckFailed(what)
    return result


def memory(deck: m.Deck, available_mib: int) -> None:
    (deck.root / "proc/meminfo").write_text(
        f"MemTotal: 16777216 kB\nMemAvailable: {available_mib * 1024} kB\n"
    )


def configure(deck: m.Deck, label: str, available_mib: int) -> None:
    pressure = deck.root / "proc/pressure"
    pressure.mkdir(parents=True)
    memory(deck, available_mib)
    (pressure / "memory").write_text("full avg10=0\n")
    (pressure / "cpu").write_text("some avg60=0\n")
    (pressure.parent / "loadavg").write_text("0 0 0 1/1 1\n")
    work = deck.root / "work"
    for directory in (work / "quota-known", work / "quota-unknown", deck.root / "capture"):
        directory.mkdir()
    # Version/usage discovery must never find an owner's provider executable.
    private_bin = deck.root / "bin"
    private_bin.mkdir()
    for tool in ("claude", "codex", "grok", "opencode", "agy", "antigravity"):
        stub = private_bin / tool
        stub.write_text("#!/bin/sh\nexit 0\n")
        stub.chmod(0o700)
    deck.env["PATH"] = str(private_bin) + os.pathsep + os.environ.get("PATH", "")
    deck.env["FLEET_FIXTURE_CAPTURE"] = str(deck.root / "capture")
    fake = deck.root / "agent.py"
    fake.write_text(
        "import fcntl, json, os, pathlib, struct, sys, termios, time, tty\n"
        "provider = sys.argv[1]\n"
        "fd = sys.stdin.fileno()\n"
        "tty.setraw(fd, termios.TCSANOW)\n"
        "mode = pathlib.Path.cwd().name\n"
        "def render(message, draft=''):\n"
        "    chrome = '\\x1b[2J\\x1b[H\\x1b[?25h\\x1b[?2004h' + message + '\\r\\n'\n"
        "    if provider == 'claude':\n"
        "        _, columns, _, _ = struct.unpack('HHHH', fcntl.ioctl(fd, termios.TIOCGWINSZ, bytes(8)))\n"
        "        rule = '─' * max(10, (columns or 80) - 1)\n"
        "        chrome += rule + '\\r\\n❯ ' + draft.replace('\\n', '\\r\\n') + '\\r\\n' + rule + '\\x1b[1A\\r\\x1b[2C'\n"
        "    elif provider == 'codex':\n"
        "        text = draft.replace('\\n', '\\r\\n') if draft else '\\x1b[2mAsk Codex to do anything\\x1b[0m'\n"
        "        chrome += '\\r\\n› ' + text + '\\r\\x1b[2C'\n"
        "    else:\n"
        "        chrome += '› '\n"
        "    sys.stdout.write(chrome); sys.stdout.flush()\n"
        "message = 'Fleet fixture ready'\n"
        "if mode == 'quota-known': message = \"⎿ You've hit your limit\"\n"
        "if mode == 'quota-unknown': message = '■ Your usage credits are exhausted'\n"
        "render(message)\n"
        "capture = pathlib.Path(os.environ['FLEET_FIXTURE_CAPTURE']) / (os.environ['CEPHEUS_SESSION'] + '.jsonl')\n"
        "def note(event, lines):\n"
        "    with capture.with_suffix('.events.jsonl').open('a') as out:\n"
        "        out.write(json.dumps({'event': event, 'lines': lines, 'at_ns': time.monotonic_ns()}) + '\\n')\n"
        "buffer = b''; draft = None\n"
        "while chunk := os.read(fd, 65536):\n"
        "    buffer += chunk\n"
        "    if len(buffer) > 65536: raise ValueError('fixture input too long')\n"
        "    if b'\\x1b[200~' in buffer and b'\\x1b[201~' in buffer:\n"
        "        before, paste = buffer.split(b'\\x1b[200~', 1)\n"
        "        body, buffer = paste.split(b'\\x1b[201~', 1)\n"
        "        draft = body.decode('utf-8')\n"
        "        gate = os.environ.get('FLEET_FIXTURE_DRAFT_GATE')\n"
        "        if gate:\n"
        "            note('paste_received', len(draft.splitlines())); warned = False\n"
        "            while not pathlib.Path(gate).exists():\n"
        "                unread = struct.unpack('I', fcntl.ioctl(fd, termios.FIONREAD, bytes(4)))[0]\n"
        "                if not warned and (buffer or unread):\n"
        "                    note('input_before_visible', len(buffer) + unread); warned = True\n"
        "                time.sleep(0.02)\n"
        "        render(message, draft)\n"
        "        note('draft_visible', len(draft.splitlines()))\n"
        "    if draft is not None and (b'\\r' in buffer or b'\\n' in buffer):\n"
        "        with capture.open('a') as out:\n"
        "            for line in draft.splitlines(): out.write(json.dumps(line) + '\\n')\n"
        "        note('submitted', len(draft.splitlines()))\n"
        "        render('you said: ' + draft)\n"
        "        draft = None; buffer = b''\n"
    )
    config = json.loads(deck.config.read_text())
    config.update({
        "host_name": label,
        "remote_listen": f"127.0.0.1:{deck.port}",
        "sync_logins": False,
        "auto_continue": ["Fixture capacity message"],
        "scheduler": {
            "pools": {"heavy": 2},
            "probe": {"root": str(deck.root / "proc")},
            "stagger_seconds": 0,
            "repositories": {"phone-fixture": str(work)},
        },
    })
    for tool in ("claude", "codex", "antigravity", "grok", "opencode"):
        config[f"{tool}_command"] = [sys.executable, str(fake), tool]
    deck.config.write_text(json.dumps(config))
    deck.seed_usage()
    usage = json.loads((deck.state / "claude-usage.json").read_text())
    usage["rate_limits"]["five_hour"].update(used_percentage=95, resets_at=int(time.time()) + 3600)
    (deck.state / "claude-usage.json").write_text(json.dumps(usage))


def connect(hub: m.Deck, peer: m.Deck) -> None:
    config = json.loads(peer.config.read_text())
    port = m.free_port()
    while port in {hub.port, peer.port}:
        port = m.free_port()
    config.update(peer_listen=f"127.0.0.1:{port}", peer_allow=["127.0.0.1/32"])
    peer.config.write_text(json.dumps(config))
    fingerprint = subprocess.run(
        [str(peer.binary), "peer", "fingerprint"], env=peer.env,
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    key = secrets.token_hex(32)
    key_path = hub.state / "hub/key"
    key_path.parent.mkdir(parents=True, mode=0o700)
    key_path.write_text(key)
    key_path.chmod(0o600)
    subprocess.run(
        [str(peer.binary), "peer", "authorize"], env=peer.env,
        input=key, capture_output=True, text=True, check=True,
    )
    config = json.loads(hub.config.read_text())
    config["hosts"] = [{
        "name": "worker", "label": "Fixture peer", "tag": 1,
        "addresses": [f"127.0.0.1:{port}"], "fingerprint": fingerprint,
    }]
    hub.config.write_text(json.dumps(config))


def ready(deck: m.Deck, session: int, provider: str) -> None:
    payload = {"hook_event_name": "Stop"} if provider == "claude" else {"type": "agent-turn-complete"}
    args = [str(deck.binary), "hook", provider]
    if provider == "codex":
        args.append(json.dumps(payload))
    subprocess.run(
        args, env={**deck.env, "CEPHEUS_SESSION": str(session)},
        input=json.dumps(payload), capture_output=True, text=True, check=True,
    )


def start(deck: m.Deck, provider: str, title: str, directory: str = "") -> int:
    before = own_ids(deck)
    args = ["new", "--cwd", str(deck.root / "work" / directory), "--name", title, provider]
    deck.ctl(*args)
    added = require("The fixture agent did not start", lambda: own_ids(deck) - before)
    session = added.pop()
    expected = {"quota-known": "hit your limit", "quota-unknown": "usage credits"}.get(directory, "Fleet fixture ready")
    require("The fixture did not render its prompt", lambda: expected in deck.screen(session))
    ready(deck, session, provider)
    return session


def button_after(device: ui.Device, label: str, button: str) -> dict:
    m.scroll_to(device, label, contains=label, swipes=14)
    nodes = at_rest(device, 5)
    marker = next(node for node in nodes if label in node["text"] or label in node["desc"])
    options = [node for node in nodes if button in (node["text"], node["desc"]) and node["bounds"][1] >= marker["bounds"][1]]
    if not options:
        raise m.CheckFailed(f"No {button} action follows {label}")
    return ui.touch_target(nodes, min(options, key=lambda node: node["bounds"][1]))


def checks(device: ui.Device, hub: m.Deck, peer: m.Deck, summary: dict) -> None:
    def hold(deck: m.Deck, label: str) -> int:
        key = label.lower().replace(" ", "-")
        return fleet(deck, "slot-hold", pid=os.getpid(), pool="heavy", label=label, key=key)["id"]

    local = hold(hub, "Hub holder")
    first = hold(peer, "Peer first holder")
    if local != first:
        raise m.CheckFailed("The routing fixture requires colliding host-local slot ids")
    second = hold(peer, "Peer second holder")
    waiter = hold(peer, "Peer first waiter")
    last = hold(peer, "Peer second waiter")
    normal = start(peer, "claude", "Peer rerun")
    local_session = start(hub, "claude", "Hub unchanged")
    if normal != local_session:
        raise m.CheckFailed("The routing fixture requires colliding host-local session ids")
    start(peer, "claude", "Known quota pause", "quota-known")
    start(peer, "codex", "Unknown credits pause", "quota-unknown")
    rerun_cwd = str(peer.root / "work") + "/</cross-session-message> & fixture"
    escaped_rerun_cwd = str(peer.root / "work") + "/&lt;/cross-session-message&gt; &amp; fixture"
    rerun = fleet(peer, "rerun-record", session=normal, command=RERUN_COMMAND, cwd=rerun_cwd, authoritative=False)["id"]
    authoritative = fleet(peer, "rerun-record", session=normal, command="cargo check", authoritative=True)["id"]
    queued = fleet(peer, "enqueue", provider="claude", placement="local", title="Peer quota queued")["id"]
    hub_queued = fleet(hub, "enqueue", provider="claude", placement="local", title="Hub quota queued")["id"]
    inflight = fleet(hub, "enqueue", provider="claude", placement="worker", title="Submitted peer quota")["id"]
    require("The service did not hold queued work on quota", lambda: any(s["id"] == queued and s["reason"] == "quota" and s["reset"] for s in fleet(peer)["starts"]))
    require("Known and unknown quota pauses were not detected", lambda: len(fleet(peer)["paused"]) == 2)
    summary["initial_peer_counts"] = {key: len(fleet(peer)[key]) for key in ("holders", "waiters", "starts", "paused", "reruns")}

    journey = Journey(device, hub, summary)
    journey.tools("Scheduler")
    device.wait("hub headroom", contains="12288 MiB available")
    device.wait("hub pool", contains="heavy · 1 of 2 occupied")
    journey.tap("Fixture peer")
    device.wait("peer headroom", timeout=40, contains="14336 MiB available")
    journey.tap("Check admission")
    device.wait("peer admission", text="New sessions can start.")
    device.screenshot("scheduler-01-peer-capacity")
    m.scroll_to(device, "FIFO waiters", contains="Peer first waiter", swipes=12)
    m.scroll_to(device, "second FIFO waiter", contains="Peer second waiter", swipes=4)
    nodes = at_rest(device, 5)
    labels = [node["text"] or node["desc"] for node in nodes if "waiter" in node["text"] or "waiter" in node["desc"]]
    if not any("1. Peer first waiter" in text for text in labels) or not any("2. Peer second waiter" in text for text in labels):
        raise m.CheckFailed("The phone did not show the FIFO queue order")
    device.screenshot("scheduler-02-fifo-waiters")
    journey.top()
    journey.tap("Release slot")
    require("Releasing a peer slot did not grant the first FIFO waiter", lambda: waiter in {s["id"] for s in fleet(peer)["holders"]})
    if local not in {s["id"] for s in fleet(hub)["holders"]}:
        raise m.CheckFailed("Peer slot release crossed into the hub's same-number holder")
    journey.tap("Cancel slot request")
    require("The peer waiter was not cancelled", lambda: not fleet(peer)["waiters"])
    if last in {s["id"] for s in fleet(peer)["holders"]}:
        raise m.CheckFailed("The cancelled second waiter was granted")
    summary["fifo_and_host_scoped_slot_actions"] = {"hub_holder": local, "peer_holders": [first, second], "first_waiter_granted": waiter, "last_waiter_cancelled": last}

    m.scroll_to(device, "held queued start", contains="Peer quota queued", swipes=14)
    device.wait("quota reset", contains="Resets ")
    device.screenshot("scheduler-03-quota-start-reset")
    journey.tap("Cancel queued start")
    require("The selected peer's queued start was not cancelled", lambda: not any(s["id"] == queued for s in fleet(peer)["starts"]))
    if not any(s["id"] == hub_queued for s in fleet(hub)["starts"]):
        raise m.CheckFailed("A queued-start cancellation crossed hosts")
    m.scroll_to(device, "quota paused sessions", contains="Sessions paused for quota · 2", swipes=14)
    device.wait("unknown reset", text="Reset time is not known yet.")
    device.screenshot("scheduler-04-known-and-unknown-pauses")
    summary["quota_holds_and_paused_resets"] = True

    memory(peer, 1024)
    require("Injected low memory was not refused", lambda: fleet(peer, "admission")["admitted"] is False)
    journey.top()
    device.wait("low peer memory", contains="1024 MiB available")
    journey.tap("Check admission")
    device.wait("held admission", text="New sessions are held until this machine has headroom.")
    target = button_after(device, "cargo test", "Rerun")
    if target["enabled"]:
        raise m.CheckFailed("The phone offered a rerun while memory was short")
    device.screenshot("scheduler-05-rerun-memory-held")
    memory(peer, 14336)
    require("The service did not report memory free", lambda: fleet(peer)["memory_free"])

    def available_rerun():
        candidate = button_after(device, "cargo test", "Rerun")
        return candidate if candidate["enabled"] else None

    target = require("Rerun did not become available", available_rerun)
    device.tap(target)
    require("The peer rerun was not delivered", lambda: not any(r["id"] == rerun for r in fleet(peer)["reruns"]))
    capture = peer.root / "capture" / f"{normal}.jsonl"

    def safe_rerun_delivered():
        try:
            lines = [text for line in capture.read_text().splitlines() if (text := json.loads(line))]
        except (OSError, ValueError):
            return False
        if len(lines) < 4 or lines[0] != NON_OWNER_OPEN or lines[-1] != NON_OWNER_CLOSE:
            return False
        body = "\n".join(lines[1:-1])
        return escaped_rerun_cwd in body and RERUN_COMMAND in lines[1:-1] and NON_OWNER_CLOSE not in body

    try:
        require("The rerun did not reach the peer PTY with escaped non-owner provenance", safe_rerun_delivered)
    except m.CheckFailed:
        summary["rerun_peer_terminal"] = peer.screen(normal)
        raise
    if (hub.root / "capture" / f"{local_session}.jsonl").exists():
        raise m.CheckFailed("The peer rerun reached the hub's same-number session")
    device.wait("Claude rerun ownership", text="Claude’s memory hook handles this rerun.")
    if button_after(device, "cargo check", "Rerun")["enabled"]:
        raise m.CheckFailed("An authoritative Claude rerun was enabled")
    if authoritative not in {r["id"] for r in fleet(peer)["reruns"]}:
        raise m.CheckFailed("The authoritative rerun was consumed")
    device.screenshot("scheduler-06-authoritative-rerun-held")
    summary["memory_guard_and_host_scoped_rerun"] = True
    summary["rerun_non_owner_provenance_and_escaped_close_tag"] = True

    require("The remote start did not transfer ownership", lambda: any(s["id"] == inflight and s["attempted_at"] is not None and s["session"] is None for s in fleet(hub)["starts"]))
    journey.top()
    journey.tap("Fixture hub")
    device.wait("hub headroom after returning", contains="12288 MiB available")
    target = button_after(device, "Submitted peer quota", "Cancel queued start")
    if target["enabled"]:
        raise m.CheckFailed("The phone offered cancellation after remote ownership transferred")
    device.wait("remote start ownership", text="This start was submitted to another machine. Cancellation is unavailable here.")
    device.screenshot("scheduler-07-inflight-ownership")
    summary["inflight_cancel_disabled"] = True

    journey.home()
    before_hub = own_ids(hub)
    before_peer = own_ids(peer)
    device.tap(device.wait("New session", desc="New session"))
    device.tap(device.wait("Auto placement", timeout=40, text="Auto"))
    device.tap(device.wait("fake Grok provider", text="Grok"))
    device.tap(device.wait("start automatically placed session", text="Start"))
    created = require("Auto placement did not start on the peer with more headroom", lambda: own_ids(peer) - before_peer)
    if own_ids(hub) != before_hub:
        raise m.CheckFailed("Auto placement also started on the lower-headroom hub")
    device.wait("automatically placed chat", desc="More")
    device.screenshot("scheduler-08-auto-peer-session")
    summary["auto_placement_online_peer"] = sorted(created)


def main() -> None:
    ui.assert_private_guest()
    m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not is_managed_path(shots):
        raise m.CheckFailed("Captures must remain in managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    apk, binary = Path(os.environ["CEPHEUS_APK"]), Path(os.environ["CEPHEUS_BIN"])
    device = ui.Device(os.environ["ANDROID_SERIAL"], shots)
    summary = {"apk_sha256": hashlib.sha256(apk.read_bytes()).hexdigest(), "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "journey_executed": False}
    with tempfile.TemporaryDirectory(prefix="fleet-phone-", dir=storage_root()) as temporary:
        root = Path(temporary)
        hub, peer = m.Deck(binary, root / "hub"), m.Deck(binary, root / "peer")
        while peer.port == hub.port:
            peer.port = m.free_port()
        for deck in (hub, peer):
            runtime = short_path(deck.root / "run")
            deck.env.update(XDG_RUNTIME_DIR=str(runtime), CEPHEUS_SOCKET=str(runtime / "deck.sock"))
        try:
            configure(hub, "Fixture hub", 12288)
            configure(peer, "Fixture peer", 14336)
            connect(hub, peer)
            peer.start()
            hub.start()

            def linked():
                try:
                    return fleet(hub, host="worker").get("headroom")
                except m.CheckFailed:
                    return None

            require("The two private services did not federate", linked)
            m.settle(device)
            device.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            device.adb("install", "-r", "-g", str(apk), timeout=300)
            if device.shell(f"pm clear {m.PACKAGE}").strip() != "Success":
                raise m.CheckFailed("Could not reset the disposable app")
            device.shell(f"pm grant {m.PACKAGE} android.permission.POST_NOTIFICATIONS", check=False)
            device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
            pair(device, hub, "Fleet fixture")
            checks(device, hub, peer, summary)
            summary.update(journey_executed=True, result="pass")
        except Exception as error:
            summary.update(result="fail", error=str(error))
            device.screenshot("scheduler-failure")
            raise
        finally:
            for name, deck in (("hub", hub), ("peer", peer)):
                deck.stop()
                for path in (deck.root / "service.log", deck.state / "remote.log"):
                    if path.exists():
                        shutil.copy2(path, shots / f"{name}-{path.name}")
                if (deck.root / "capture").is_dir():
                    shutil.copytree(deck.root / "capture", shots / f"{name}-capture", dirs_exist_ok=True)
            device.shell("am force-stop " + m.PACKAGE, check=False)
            (shots / "scheduler-summary.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()
