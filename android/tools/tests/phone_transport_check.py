#!/usr/bin/env python3
"""Client-only submission/history proof. This does not run the Claude owner API.

The disposable gateway simulates a positive owner-API submission reply, delays
canonical history, and later publishes its ordinary User echo. The real native
Codex journey is phone_send_check.py; keep its provider-inbox/zero-PTY evidence
separate from this client-state fixture.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import ssl
import subprocess
import tempfile
import threading
from pathlib import Path

import chat_ui_check as ui
from device_waits import poll

from test_support import is_managed_path
from test_support import storage_root

m = ui.m
SESSION = (3 << 40) | 7
BODY = "Private submitted owner message"


class Gateway:
    def __init__(self, root):
        self.port = m.free_port()
        self.protocol = 19
        self.title = "Private submitted transport"
        self.clients = set()
        self.messages = []
        self.items = []
        self.auths = 0
        self.paired = False
        cert, key = root / "cert.pem", root / "key.pem"
        subprocess.run(
            [
                "openssl",
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                str(key),
                "-out",
                str(cert),
                "-days",
                "1",
                "-subj",
                "/CN=private-phone-transport",
            ],
            capture_output=True,
            check=True,
            timeout=30,
        )
        self.sha = hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.read_text())).hexdigest()
        self.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.tls.load_cert_chain(cert, key)

    def fingerprint(self):
        return self.sha

    def open_pairing(self):
        return "ABCDEFGH"

    def devices(self):
        return ["private-guest"] if self.paired else []

    def state(self):
        return {
            "t": "state",
            "protocol": self.protocol,
            "ready": True,
            "sessions": [
                {
                    "id": SESSION,
                    "tool": "claude",
                    "title": self.title,
                    "status": "idle",
                    "alive": True,
                    "created_at": 10,
                    "pid": 123,
                    "conversation": "private-thread",
                    "spawned_ms": 100,
                    "host": "private-host",
                    "host_label": "Private host",
                }
            ],
            "usage": {},
            "hosts": [{"name": "private-host", "label": "Private host", "online": True}],
        }

    def publish(self):
        for client in tuple(self.clients):
            client.send(json.dumps(self.state()))

    def reconnect(self):
        for client in tuple(self.clients):
            client.close()

    def chat(self, rid=None):
        return {
            "t": "chat",
            **({"rid": rid} if rid is not None else {}),
            "id": SESSION,
            "start": 0,
            "total": len(self.items),
            "generation": 1,
            "source": "log",
            "items": self.items,
        }

    def publish_history(self):
        for client in tuple(self.clients):
            client.send(json.dumps(self.chat()))

    def handle(self, client):
        try:
            for raw in client:
                request = json.loads(raw)
                kind, rid = request.get("t"), request.get("rid")
                if kind == "pair":
                    self.paired = True
                    reply = {
                        "t": "paired",
                        "device": "private-guest",
                        "key": "private-fixture-only",
                    }
                elif kind == "auth":
                    self.clients.add(client)
                    self.auths += 1
                    client.send(
                        json.dumps(
                            {"t": "hello", "epoch": "private-transport", "version": "fixture"}
                        )
                    )
                    client.send(json.dumps(self.state()))
                    continue
                elif kind in ("open", "chat"):
                    reply = self.chat(rid)
                elif kind == "send":
                    self.messages.append(request)
                    reply = {"t": "ok", "rid": rid}
                elif kind == "models":
                    reply = {"t": "models", "rid": rid, "tools": []}
                elif kind == "control":
                    reply = {
                        "t": "control",
                        "rid": rid,
                        "action": request.get("action"),
                        "data": {"protocol": self.protocol, "capabilities": []},
                    }
                else:
                    reply = {"t": "ok", "rid": rid}
                client.send(json.dumps(reply))
        finally:
            self.clients.discard(client)

    @contextlib.contextmanager
    def running(self):
        from websockets.sync.server import serve

        with serve(self.handle, "127.0.0.1", self.port, ssl=self.tls) as server:
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                yield self
            finally:
                self.reconnect()
                server.shutdown()
                worker.join(timeout=10)
                if worker.is_alive():
                    raise m.CheckFailed("Private transport gateway did not stop")


def main():
    ui.assert_private_guest()
    m.PACKAGE = "io.github.cepheus.app"
    shots = Path(os.environ["CEPHEUS_ANDROID_ARTIFACTS"]) / "phone-transport"
    if not is_managed_path(shots):
        raise m.CheckFailed("Transport evidence must stay in managed storage")
    shots.mkdir(parents=True, exist_ok=True)
    # The supported offloader stores its one verified APK beside evidence.
    apk = (
        Path(os.environ["CEPHEUS_APK"])
        if os.environ.get("CEPHEUS_APK")
        else shots.parent.parent / "candidate.apk"
    )
    with apk.open("rb") as stream:
        apk_sha = hashlib.file_digest(stream, "sha256").hexdigest()
    summary = {
        "scope": "client submission/history only; no actual Claude owner API claim",
        "session": SESSION,
        "apk_sha256": apk_sha,
    }
    device = ui.Device(os.environ["ANDROID_SERIAL"], shots)
    with tempfile.TemporaryDirectory(prefix="phone-transport-", dir=storage_root()) as temporary:
        with Gateway(Path(temporary)).running() as gateway:
            try:
                os.environ["CEPHEUS_ONLY"] = "chat"
                m.chat_before_log = lambda *_: None
                m.journey(device, gateway, apk, summary)
                device.tap(
                    device.wait("private transport session", contains="Private submitted transport")
                )
                device.wait("private chat composer", timeout=45, desc="More")
                device.type_into(device.fields()[-1], BODY)
                device.tap(device.wait("Send", desc="Send"))
                device.wait("submission awaiting canonical history", contains="Submitted")
                if len(gateway.messages) != 1 or gateway.messages[0].get("id") != SESSION:
                    raise m.CheckFailed("Transport did not receive one exact full-ID request")
                print(
                    "Submitted transport observed once; waiting through the former quiet expiry",
                    flush=True,
                )
                if poll(
                    22,
                    lambda: (
                        not device.find(contains="Submitted")
                        or device.find(contains="Queued")
                        or device.find(contains="Send now")
                    ),
                ):
                    raise m.CheckFailed("Submitted message vanished or offered a duplicate send")
                before = gateway.auths
                gateway.reconnect()
                if not poll(45, lambda: gateway.auths > before):
                    raise m.CheckFailed("Private transport did not reconnect")
                device.wait("submitted row after reconnect", contains="Submitted")
                device.shell(f"am force-stop {m.PACKAGE}")
                device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
                device.tap(
                    device.wait("saved private session", contains="Private submitted transport")
                )
                device.wait("submitted recovery after app restart", contains="Submitted")
                if len(gateway.messages) != 1:
                    raise m.CheckFailed("Reconnect/restart repeated the original request")
                device.screenshot("submitted-preserved")
                gateway.items = [{"k": "user", "text": BODY}]
                gateway.publish_history()
                if not poll(
                    45,
                    lambda: (
                        not device.find(contains="Submitted") and len(device.find(text=BODY)) == 1
                    ),
                ):
                    raise m.CheckFailed("Canonical history did not replace one submitted bubble")
                if len(gateway.messages) != 1:
                    raise m.CheckFailed("Canonical reconciliation repeated the original request")
                device.screenshot("canonical-echo-reconciled")
                gateway.protocol = 18
                gateway.title = "Private gateway updated"
                gateway.publish()
                device.wait(
                    "fresh old-protocol State publication", contains="Private gateway updated"
                )
                device.type_into(device.fields()[-1], "Must never reach an old gateway")
                device.tap(device.wait("Send", desc="Send"))
                device.wait("actionable protocol refusal", contains="Update")
                if len(gateway.messages) != 1:
                    raise m.CheckFailed("Old gateway received a native owner message")
                device.screenshot("protocol18-refused")
                summary.update(
                    ok=True,
                    send_requests=1,
                    quiet_retained=True,
                    reconnect_retained=True,
                    restart_retained=True,
                    canonical_echo_reconciled=True,
                    old_gateway_writes=0,
                )
            except Exception as error:
                summary["error"] = str(error)
                device.screenshot("transport-failure")
                raise
            finally:
                (shots / "summary.json").write_text(json.dumps(summary, indent=2))
                print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    main()
