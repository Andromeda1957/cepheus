#!/usr/bin/env python3
"""Render the installed-phone send failure on a disposable pool guest.

Uses the exact supplied APK/native binary, a private gateway, and an inert
direct Codex launcher. This baseline records the multicolor-footer rejection;
the recovery acceptance must use provider admission without terminal input.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import tempfile
import threading
import uuid
from pathlib import Path

import chat_ui_check as ui
from device_waits import poll
from first_message_check import link_codex

from test_support import is_managed_path
from test_support import short_path
from test_support import storage_root

m = ui.m

PROGRAM = r"""#!/usr/bin/python3
import json, os, sys, tty
from pathlib import Path
root = Path(__file__).parent
if "queue" in sys.argv:
    with (root / "queue-probes").open("a") as output:
        output.write("probe\n")
    if not (root / "native-supported").exists():
        print("Unavailable")
        sys.exit(0)
    if "--help" in sys.argv:
        print("Queue a message --thread THREAD --message TEXT")
        sys.exit(0)
    thread = sys.argv[sys.argv.index("--thread") + 1]
    if thread != (root / "thread-id").read_text():
        sys.exit(1)
    message = sys.argv[sys.argv.index("--message") + 1]
    with (root / "native-inbox").open("a") as output:
        output.write(json.dumps({"thread": thread, "message": message}) + "\n")
    print("Queued fixture owner message.")
    sys.exit(0)
if "--version" in sys.argv or "--help" in sys.argv:
    print("codex private fixture --remote")
    sys.exit(0)
with (root / "launch.jsonl").open("a") as output:
    output.write(json.dumps({"argv": sys.argv, "pid": os.getpid()}) + "\n")
tty.setraw(0)
footer = "\x1b[38;5;33m GPT-6.1-Sol\x1b[0m \x1b[32m test-branch\x1b[0m\r\n\x1b[1m context\x1b[0m"
if (root / "dim-footer").exists():
    footer = "\x1b[2m GPT-6.1-Sol test-branch\r\n context\x1b[0m"
os.write(1, ("\x1b[?2004hready\r\n\r\n› \x1b[2mAsk Codex to do anything\x1b[0m\r\n\r\n" + footer + "\x1b[3;3H").encode())
while True:
    data = os.read(0, 4096)
    if not data:
        break
    with (root / "typed").open("ab") as output:
        output.write(data)
    os.write(1, data)
"""


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class NativeControl:
    """Private native start-or-steer endpoint, independent of terminal input."""

    def __init__(self, root: Path):
        from websockets.sync.server import unix_serve

        self.root = root.resolve()
        self.name = None
        self.receipt = None
        self.methods = []
        self.started_threads = []
        self.start_requests = []
        self.clients = set()
        self.server = unix_serve(self.handle, path=str(short_path(root / "codex.sock")))
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def handle(self, client):
        self.clients.add(client)
        try:
            for raw in client:
                value = json.loads(raw)
                if "id" not in value:
                    continue
                method = value.get("method")
                self.methods.append(method)
                if method == "initialize":
                    result = {}
                elif method == "thread/start":
                    params = value.get("params") or {}
                    cwd = str(self.root / "work")
                    if Path(params.get("cwd", "")).resolve() != Path(cwd).resolve():
                        client.send(
                            json.dumps(
                                {
                                    "id": value["id"],
                                    "error": {"code": -32602, "message": "Wrong private cwd"},
                                }
                            )
                        )
                        continue
                    thread_id = str(uuid.uuid4())
                    (self.root / "thread-id").write_text(thread_id)
                    self.start_requests.append(params)
                    result = {
                        "thread": {"id": thread_id, "cwd": cwd, "status": {"type": "idle"}},
                        "cwd": cwd,
                        "model": params.get("model") or "gpt-6.1-sol",
                        "reasoningEffort": (params.get("config") or {}).get(
                            "model_reasoning_effort"
                        ),
                        "approvalPolicy": params.get("approvalPolicy", "on-request"),
                        "approvalsReviewer": "user",
                        "sandbox": {},
                    }
                    self.started_threads.append(result)
                elif method == "thread/name/set":
                    params = value["params"]
                    thread_id = (self.root / "thread-id").read_text()
                    if params["threadId"] != thread_id:
                        raise m.CheckFailed("Native name addressed another thread")
                    self.name = params["name"]
                    self.receipt = self.root / "home/.codex/sessions" / f"{thread_id}.jsonl"
                    self.receipt.parent.mkdir(parents=True, exist_ok=True)
                    self.receipt.write_text(
                        json.dumps(
                            {
                                "type": "session_meta",
                                "payload": {"id": thread_id, "cwd": str(self.root / "work")},
                            }
                        )
                        + "\n"
                    )
                    result = {}
                elif method == "thread/settings/update":
                    params = value["params"]
                    if params["threadId"] != (self.root / "thread-id").read_text():
                        raise m.CheckFailed("Native settings addressed another thread")
                    with self.receipt.open("a") as output:
                        output.write(
                            json.dumps(
                                {
                                    "type": "event_msg",
                                    "payload": {
                                        "type": "thread_settings_applied",
                                        "thread_id": params["threadId"],
                                        "thread_settings": {
                                            "cwd": str(self.root / "work"),
                                            "model": params["model"],
                                            "reasoning_effort": params.get("effort"),
                                        },
                                    },
                                }
                            )
                            + "\n"
                        )
                    result = {}
                elif method == "thread/read":
                    started = self.started_threads[-1]
                    result = {
                        "thread": {
                            "id": (self.root / "thread-id").read_text(),
                            "cwd": str(self.root / "work"),
                            "name": self.name,
                            "path": str(self.receipt) if self.receipt else None,
                            "model": started["model"],
                            "reasoningEffort": started["reasoningEffort"],
                            "status": {
                                "type": "active" if (self.root / "active").exists() else "idle"
                            },
                        }
                    }
                elif method == "thread/turns/list":
                    result = {"data": [{"id": "fixture-turn", "status": "inProgress"}]}
                elif method == "turn/start":
                    params = value["params"]
                    if params["threadId"] != (self.root / "thread-id").read_text():
                        raise m.CheckFailed("Native input addressed another thread")
                    text = "\n".join(
                        part["text"] for part in params["input"] if part["type"] == "text"
                    )
                    with (self.root / "native-inbox").open("a") as output:
                        output.write(
                            json.dumps(
                                {
                                    "thread": params["threadId"],
                                    "message": text,
                                    "client_id": params["clientUserMessageId"],
                                }
                            )
                            + "\n"
                        )
                    result = {"turn": {"id": "fixture-turn", "status": "inProgress"}}
                elif method == "thread/queue/start":
                    result = {"turn": {"id": "fixture-turn", "status": "inProgress"}}
                else:
                    client.send(
                        json.dumps(
                            {
                                "id": value["id"],
                                "error": {
                                    "code": -32601,
                                    "message": "Unsupported fixture operation",
                                },
                            }
                        )
                    )
                    continue
                client.send(json.dumps({"id": value["id"], "result": result}))
        finally:
            self.clients.discard(client)

    def close(self):
        for client in list(self.clients):
            client.close()
        self.server.shutdown()
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise m.CheckFailed("Private provider server did not stop")


def inbox(root: Path) -> list[dict]:
    path = root / "native-inbox"
    return [json.loads(row) for row in path.read_text().splitlines()] if path.exists() else []


def send_native(device, deck, root, session, text):
    device.type_into(device.fields()[-1], text)
    button = poll(
        45, lambda: next((node for node in device.find(desc="Send") if node["enabled"]), None)
    )
    if not button:
        raise m.CheckFailed("Fresh phone composer did not become ready")
    device.tap(button)
    if not poll(45, lambda: any(row["message"] == text for row in inbox(root))):
        raise m.CheckFailed("Rendered phone send did not reach the provider inbox")
    if sum(row["message"] == text for row in inbox(root)) != 1 or (root / "typed").exists():
        raise m.CheckFailed("Phone delivery duplicated input or wrote into the terminal")
    if any("Sent to no session" in node["text"] for node in device.nodes()):
        raise m.CheckFailed("Phone displayed a false delivery error after native admission")


def main() -> None:
    arguments = argparse.ArgumentParser(description=__doc__)
    arguments.add_argument("--baseline", action="store_true")
    arguments.add_argument("--expected-native-sha")
    options = arguments.parse_args()
    ui.assert_private_guest()
    m.PACKAGE = os.environ.get("CEPHEUS_PACKAGE", "io.github.cepheus.app")
    artifacts = Path(os.environ.get("CEPHEUS_ANDROID_ARTIFACTS", os.environ["TMPDIR"]))
    binary = Path(os.environ.get("CEPHEUS_BIN", Path.home() / ".local/bin/cepheus"))
    apk = Path(os.environ.get("CEPHEUS_APK", artifacts.parent / "candidate.apk"))
    shots = Path(os.environ.get("CEPHEUS_SHOTS", artifacts / "phone-send"))
    if not is_managed_path(shots):
        raise m.CheckFailed("Keep phone-send evidence in managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    summary = {"apk_sha256": digest(apk), "native_sha256": digest(binary), "cases": []}
    if options.expected_native_sha and summary["native_sha256"] != options.expected_native_sha:
        raise m.CheckFailed("Remote fixture native binary does not match the frozen candidate")
    baseline = options.baseline or os.environ.get("CEPHEUS_EXPECT_REJECTION") == "1"
    with tempfile.TemporaryDirectory(prefix="phone-send-", dir=storage_root()) as temporary:
        root = Path(temporary)
        deck = m.Deck(binary, root)
        program = root / "codex"
        program.write_text(PROGRAM)
        program.chmod(0o700)
        config = json.loads(deck.config.read_text())
        label = "Private phone fixture"
        pressure = root / "proc/pressure"
        pressure.mkdir(parents=True)
        (root / "proc/meminfo").write_text("MemTotal: 16777216 kB\nMemAvailable: 12582912 kB\n")
        (pressure / "memory").write_text("full avg10=0\n")
        (pressure / "cpu").write_text("some avg60=0\n")
        (root / "proc/loadavg").write_text("0 0 0 1/1 1\n")
        subprocess.run(["git", "init", "-q", str(root / "work")], env=deck.env, check=True)
        config.update(
            host_name=label,
            hosts=[],
            sync_logins=False,
            scheduler={
                "pools": {"heavy": 2},
                "probe": {"root": str(root / "proc")},
                "stagger_seconds": 0,
                "repositories": {"phone-send-fixture": str(root / "work")},
            },
        )
        config["codex_command"] = [
            str(program.resolve()),
            "--remote",
            f"unix://{short_path(root / 'codex.sock')}",
        ]
        deck.config.write_text(json.dumps(config))
        native = None
        initial = "private native New initial delivery probe"
        if not baseline:
            (root / "native-supported").touch()
            native = NativeControl(root)
        device = ui.Device(os.environ["ANDROID_SERIAL"], shots)
        base_settle = m.settle

        def settle(current):
            base_settle(current)
            current.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            current.shell(
                "pm disable-user --user 0 com.google.android.googlequicksearchbox", check=False
            )

        m.settle = settle
        try:
            deck.start()
            snapshot = json.loads(deck.ctl("status"))
            capacity = json.loads(deck.ctl("fleet", "status", "{}"))
            if (
                snapshot.get("hosts")
                or snapshot.get("sessions")
                or snapshot.get("native_host", {}).get("label") != label
            ):
                raise m.CheckFailed("The fixture is not an empty isolated native host")
            if (
                capacity.get("repositories", {}).get("phone-send-fixture") != str(root / "work")
                or not capacity.get("admitted")
                or capacity.get("headroom", {}).get("available_mib") != 12288
            ):
                raise m.CheckFailed(
                    "Private source workspace or fixture capacity is not registered"
                )
            private = []
            marker = f"CEPHEUS_STATE_DIR={deck.state}".encode()
            for process in Path("/proc").iterdir():
                if not process.name.isdigit():
                    continue
                try:
                    environment = (process / "environ").read_bytes().split(b"\0")
                    if marker in environment and (process / "exe").resolve() == binary.resolve():
                        if (
                            f"HOME={deck.env['HOME']}".encode() not in environment
                            or f"CEPHEUS_CONFIG={deck.config}".encode() not in environment
                        ):
                            raise m.CheckFailed(
                                "Private native process inherited another HOME/config"
                            )
                        private.append(int(process.name))
                except (OSError, ProcessLookupError):
                    continue
            if deck.service.pid not in private or len(private) < 2:
                raise m.CheckFailed("Private service and gateway process custody was not proved")
            summary["private_placement"] = {
                "label": label,
                "registered_workspace": True,
                "fixture_available_mib": 12288,
                "service_pid": deck.service.pid,
                "owned_native_pids": private,
            }
            os.environ["CEPHEUS_ONLY"] = "chat"
            m.chat_before_log = lambda *_: None
            m.journey(device, deck, apk, summary)
            before = {session["id"] for session in deck.sessions()}
            device.tap(device.wait("New session", text="New session"))
            # The only native host is already selected when no peers exist;
            # the current form shows machine choices only for multiple hosts.
            if targets := device.find(text=label):
                device.tap(targets[0])
            device.tap(device.wait("Codex", text="Codex"))
            if not baseline:
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
                    raise m.CheckFailed("Advanced options did not expose the first message")
                device.type_into(field, initial)
                device.hide_keyboard()
                device.tap(device.wait("Done with Advanced options", text="Done"))

            device.tap(device.wait("Start", text="Start"))
            created = poll(45, lambda: {s["id"] for s in deck.sessions()} - before)
            if not created:
                raise m.CheckFailed("The phone did not create its private Codex session")
            session = max(created)
            m.wait_for(
                "private Codex composer",
                20,
                lambda: "Ask Codex to do anything" in deck.screen(session),
            )
            if baseline:
                link_codex(deck, session, "private previous turn")
            m.wait_for(
                "exact private conversation",
                30,
                lambda: next(s for s in deck.sessions() if s["id"] == session).get("conversation"),
            )
            original = next(s for s in deck.sessions() if s["id"] == session)
            if baseline:
                (root / "thread-id").write_text(original["conversation"])
            else:
                if len(native.started_threads) != 1:
                    raise m.CheckFailed("New did not create exactly one native provider thread")
                thread_id = native.started_threads[0]["thread"]["id"]
                launches = [
                    json.loads(line) for line in (root / "launch.jsonl").read_text().splitlines()
                ]
                if len(launches) != 1:
                    raise m.CheckFailed("New launched more than one private Codex client")
                launch = launches[0]
                argv = launch["argv"]
                if (
                    "resume" not in argv
                    or argv.index("resume") + 1 >= len(argv)
                    or argv[argv.index("resume") + 1] != thread_id
                    or "--cd" not in argv
                    or argv.index("--cd") + 1 >= len(argv)
                    or Path(argv[argv.index("--cd") + 1]).resolve() != (root / "work").resolve()
                    or launch["pid"] != original["pid"]
                    or initial in json.dumps(argv)
                    or initial in json.dumps(native.start_requests)
                ):
                    raise m.CheckFailed(
                        "First Codex launch did not resume the exact native thread/cwd without input"
                    )
                summary["new_factory"] = {
                    "start_request": native.start_requests[0],
                    "thread": native.started_threads[0],
                    "first_launch": launch,
                    "conversation_from_api": original["conversation"] == thread_id,
                    "hook_or_binder": False,
                }
            device.wait("phone chat", timeout=60, desc="More")
            if not baseline:
                if original["conversation"] != thread_id or not poll(
                    45, lambda: any(row["message"] == initial for row in inbox(root))
                ):
                    raise m.CheckFailed("New initial text did not reach its exact provider thread")
                if (
                    sum(row["message"] == initial for row in inbox(root)) != 1
                    or (root / "typed").exists()
                ):
                    raise m.CheckFailed("New initial text duplicated input or wrote terminal bytes")
                summary["cases"].append(
                    {
                        "kind": "New initial text",
                        "id": session,
                        "provider_inbox": initial,
                        "pty_bytes": 0,
                    }
                )
                device.screenshot("native-new-initial-sent")
                visible_expected = [initial]
                for case in ["idle", "working", "working-second", "working-third", "adopted"]:
                    if case == "working":
                        (root / "active").touch()
                    elif case == "adopted":
                        subprocess.run(
                            [str(binary), "restart"],
                            env=deck.env,
                            cwd=root / "work",
                            capture_output=True,
                            check=True,
                            timeout=30,
                        )
                        current = next(s for s in deck.sessions() if s["id"] == session)
                        if (current["pid"], current["conversation"]) != (
                            original["pid"],
                            original["conversation"],
                        ):
                            raise m.CheckFailed("Private handover changed the message target")
                    text = f"private native {case} delivery probe"
                    send_native(device, deck, root, session, text)
                    visible_expected.append(text)
                    visible = [node for node in device.nodes() if node["text"] in visible_expected]
                    positions = {node["text"]: node["bounds"][1] for node in visible}
                    if len(visible) != len(positions):
                        raise m.CheckFailed("Rendered phone messages contain duplicate bubbles")
                    if len(positions) < 2 or sorted(positions, key=positions.get) != [
                        message for message in visible_expected if message in positions
                    ]:
                        raise m.CheckFailed("Rendered phone messages do not preserve send order")
                    summary["cases"].append(
                        {"kind": case, "id": session, "provider_inbox": text, "pty_bytes": 0}
                    )
                    device.screenshot(f"native-{case}-sent")
                delivered = inbox(root)
                expected = [
                    initial,
                    *[
                        f"private native {case} delivery probe"
                        for case in [
                            "idle",
                            "working",
                            "working-second",
                            "working-third",
                            "adopted",
                        ]
                    ],
                ]
                if [row["message"] for row in delivered] != expected:
                    raise m.CheckFailed("Repeated owner sends were reordered or duplicated")
                if len({row["client_id"] for row in delivered}) != len(delivered):
                    raise m.CheckFailed("Repeated sends reused a native identity")
                if (root / "queue-probes").exists():
                    raise m.CheckFailed("Owner input used the idle queue instead of start-or-steer")
                summary["native_methods"] = native.methods
                summary["provider_messages"] = inbox(root)
                summary["rendered_order"] = True
                summary["ok"] = True
                return
            message = "private phone delivery probe"
            device.type_into(device.fields()[-1], message)
            device.tap(device.wait("Send", desc="Send"))
            failure = poll(
                50,
                lambda: next(
                    (
                        node["text"]
                        for node in device.nodes()
                        if "Sent to no session" in node["text"]
                    ),
                    None,
                ),
            )
            if not failure or "finish or clear the desktop draft before sending" not in failure:
                raise m.CheckFailed(f"Unexpected send failure: {failure}")
            if not (root / "queue-probes").is_file() or (root / "typed").exists():
                raise m.CheckFailed(
                    "Baseline did not establish unsupported native delivery and zero PTY input"
                )
            device.screenshot("01-published-send-rejected")
            summary["cases"].append(
                {
                    "kind": "new local Codex; multicolor footer",
                    "id": session,
                    "failure": failure,
                    "native_queue": "unsupported",
                    "pty_bytes": 0,
                }
            )
            summary["reproduced"] = True
        except Exception as error:
            summary["error"] = str(error)
            if native:
                summary["native_methods"] = native.methods
                summary["provider_messages"] = inbox(root)
            for relative in ("service.log", "state/remote.log"):
                source = root / relative
                if source.is_file():
                    (shots / source.name).write_bytes(source.read_bytes())
            device.screenshot("phone-send-failure")
            raise
        finally:
            deck.stop()
            if native:
                native.close()
            (shots / "summary.json").write_text(json.dumps(summary, indent=2))
            print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
