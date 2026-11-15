#!/usr/bin/env python3
"""Native Jobs journey against private TLS services and inert scripted commands.

Use an explicitly selected disposable emulator and its private ADB guest.
CEPHEUS_BIN, CEPHEUS_APK and CEPHEUS_SHOTS identify exact candidate
artifacts. No installed service, real provider, owner device or reserved port.
"""

from __future__ import annotations

import base64
import contextlib
import hashlib
import json
import os
import shutil
import signal
import ssl
import sys
import tempfile
import threading
from pathlib import Path

import chat_ui_check as ui
from connections_check import connections
from connections_check import pair
from deck_tools_check import Journey
from scheduler_check import configure
from scheduler_check import connect
from scheduler_check import fleet
from scheduler_check import memory
from scheduler_check import require as unscaled_require

from test_support import scaled
from test_support import is_managed_path
from test_support import short_path
from test_support import storage_root

m = ui.m


class Device(ui.Device):
    def wait(self, what: str, timeout: float = 30, **query) -> dict:
        return super().wait(what, timeout=scaled(timeout), **query)


def require(what: str, check, seconds: float = 60):
    return unscaled_require(what, check, scaled(seconds))


LITERAL = "two words; $(no-command) *"


def peer_ready(deck: m.Deck) -> bool:
    """The configured peer may still be completing its first TLS connection."""
    try:
        return bool(fleet(deck, host="worker").get("headroom"))
    except m.CheckFailed as error:
        if str(error).endswith("Fixture peer is offline: connecting"):
            return False
        raise


def jobs(deck: m.Deck) -> list[dict]:
    return fleet(deck, "job-status")["jobs"]


def job(deck: m.Deck, identifier: int) -> dict:
    return next(value for value in jobs(deck) if value["id"] == identifier)


def start(deck: m.Deck, title: str, argv: list[str], provider: str | None = None) -> dict:
    args = {
        "key": title.lower().replace(" ", "-"),
        "title": title,
        "argv": argv,
        "cwd": str(deck.root / "work"),
        "placement": "local",
    }
    if provider:
        args["provider"] = provider
    return fleet(deck, "job-start", **args)["job"]


def seed(deck: m.Deck) -> Path:
    runner = deck.root / "work/fixture-job"
    runner.write_text(
        "#!" + sys.executable + "\n"
        "import json, pathlib, sys\n"
        "pathlib.Path(__file__).with_suffix('.argv.json').write_text(json.dumps(sys.argv[1:]))\n"
        "records = [\n"
        " {'type':'item.completed','item':{'id':'msg','type':'agent_message','text':'Native structured job answer'}},\n"
        " {'type':'item.completed','item':{'id':'cmd','type':'command_execution','command':'fixture verify','aggregated_output':'Fixture verification complete','exit_code':0,'status':'completed'}},\n"
        " {'type':'item.completed','item':{'id':'end','type':'agent_message','text':'Native job finished safely'}}]\n"
        "for record in records: print(json.dumps(record), flush=True)\n"
    )
    runner.chmod(0o700)
    return runner


def identity_alive(identity: dict) -> bool:
    """Validate the private supervisor receipt against its current Linux birth."""
    try:
        proc = Path("/proc") / str(identity["pid"])
        fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
        return (
            identity["pid"] > 1
            and identity["uid"] == os.getuid() == proc.stat().st_uid
            and identity["boot"] == Path("/proc/sys/kernel/random/boot_id").read_text().strip()
            and identity["start"] == int(fields[19])
            and fields[0] != "Z"
        )
    except (FileNotFoundError, ProcessLookupError):
        return False


def lose_owned_supervisor(deck: m.Deck, identifier: int) -> dict:
    """Kill only the exact supervisor created by this private fixture service."""
    directory = deck.state / "jobs" / f"{identifier:010x}"
    launch = json.loads((directory / "launch.json").read_text())
    intent = json.loads((directory / "intent.json").read_text())
    owner = launch["supervisor"]
    if launch["nonce"] != intent["nonce"] or not identity_alive(launch["child"]):
        raise m.CheckFailed("Private job launch custody could not be proved")
    descriptor = os.pidfd_open(owner["pid"])
    try:
        command = (Path("/proc") / str(owner["pid"]) / "cmdline").read_bytes().split(b"\0")
        if not identity_alive(owner) or command[1:3] != [
            b"__job-supervise",
            os.fsencode(directory),
        ]:
            raise m.CheckFailed("Private job supervisor identity could not be proved")
        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
    finally:
        os.close(descriptor)
    require(
        "Supervisor-loss fixture did not expose an unknown outcome",
        lambda: (
            job(deck, identifier)["status"] == "failed"
            and job(deck, identifier).get("exit_code") is None
        ),
    )
    if not identity_alive(launch["child"]):
        raise m.CheckFailed("The supervisor-loss fixture did not preserve its workload")
    return launch["child"]


def no_composer(device: ui.Device) -> None:
    if device.fields():
        raise m.CheckFailed("A headless job exposed an interactive composer")


def detail(device: ui.Device, journey: Journey, title: str) -> None:
    m.scroll_to(device, title, contains=title, swipes=14)
    journey.tap(title)
    device.wait("read-only job output", text="Read-only output")
    no_composer(device)


def checks(device: ui.Device, hub: m.Deck, peer: m.Deck, runner: Path, summary: dict) -> None:
    # Identical host-local IDs make a routing mistake observable.
    survivor = start(hub, "Hub survivor", [sys.executable, "-c", "import time; time.sleep(1800)"])
    stopping = start(
        peer, "Peer stop target", [sys.executable, "-c", "import time; time.sleep(1800)"]
    )
    require("Private hub job did not work", lambda: job(hub, survivor["id"])["status"] == "working")
    require(
        "Private peer job did not work", lambda: job(peer, stopping["id"])["status"] == "working"
    )
    failing = start(hub, "Exact nonzero exit", [sys.executable, "-c", "raise SystemExit(23)"])
    require(
        "Nonzero fixture job did not finish", lambda: job(hub, failing["id"])["status"] == "failed"
    )
    paged = start(
        hub,
        "Paged private log",
        [sys.executable, "-c", "print('PAGE_BEGIN'); print('α' * 20000); print('PAGE_END')"],
    )
    require(
        "Paged fixture job did not finish", lambda: job(hub, paged["id"])["status"] == "finished"
    )

    unknown = start(
        hub, "Unknown surviving workload", [sys.executable, "-c", "import time; time.sleep(1800)"]
    )
    require(
        "Unknown-outcome fixture did not work",
        lambda: job(hub, unknown["id"])["status"] == "working",
    )
    unknown_child = lose_owned_supervisor(hub, unknown["id"])

    journey = Journey(device, hub, summary)
    journey.tools("Jobs")
    device.wait("job list", contains="Headless jobs")
    device.wait("hub retained job", text="Hub survivor")
    device.screenshot("jobs-01-list")
    no_composer(device)
    summary["jobs_list"] = True

    detail(device, journey, "Unknown surviving workload")
    device.wait("unknown supervisor outcome", contains="Exit status unknown")
    journey.tap("Stop job")
    device.wait("unknown workload stop confirmation", text="Stop this job?")
    device.tap(device.wait("confirm unknown workload stop", text="Stop job"))
    require(
        "Phone Stop left the private unknown workload alive",
        lambda: not identity_alive(unknown_child),
    )
    device.wait("unknown outcome retained after Stop", contains="Exit status unknown")
    no_composer(device)
    device.screenshot("jobs-02-unknown-workload-stopped")
    summary["unknown_surviving_workload_stopped"] = True
    journey.tap("All jobs")

    # The user's launch is admitted as a durable held record before any process runs.
    memory(hub, 1024)
    require("Memory fixture was not refused", lambda: fleet(hub, "admission")["admitted"] is False)
    journey.tap("Start job")
    journey.fill("Job title", "Phone launched job")
    journey.fill("Program", str(runner))
    journey.fill("Arguments (one per line)", LITERAL)
    journey.fill("Working directory", str(hub.root / "work"))
    journey.tap("This machine")
    journey.tap("Codex")
    journey.tap("Launch job")
    device.wait("held job title", text="Phone launched job")
    device.wait("durable held job state", contains="Starting")
    pending = require(
        "Phone launch did not create one durable held job",
        lambda: next(
            (value for value in jobs(hub) if value["title"] == "Phone launched job"), None
        ),
    )
    if pending["status"] != "starting" or not pending.get("reason"):
        raise m.CheckFailed("Admission did not preserve the held job and its reason")
    no_composer(device)
    device.screenshot("jobs-02-held-start")
    summary["durable_start_held"] = {"status": pending["status"], "reason": pending["reason"]}

    memory(hub, 12288)
    require(
        "Phone job did not complete after admission",
        lambda: job(hub, pending["id"])["status"] == "finished",
    )
    device.wait("exact successful exit", timeout=45, contains="Exit 0")
    device.wait("native structured message", timeout=45, contains="Native structured job answer")
    no_composer(device)
    device.screenshot("jobs-03-structured-chat")
    captured = json.loads(runner.with_suffix(".argv.json").read_text())
    if captured != [LITERAL]:
        raise m.CheckFailed("The phone command changed literal argument boundaries")
    summary["literal_argv"] = captured
    summary["structured_chat_and_exit_zero"] = True

    journey.tap("Show raw log")
    device.wait("private raw output", contains="item.completed")
    m.scroll_to(device, "log page completion", contains="End of log", swipes=12)
    device.screenshot("jobs-04-private-raw-log")
    summary["raw_log_and_eof"] = True
    journey.top()
    journey.tap("All jobs")
    detail(device, journey, "Exact nonzero exit")
    device.wait("exact failed exit", contains="Exit 23")
    no_composer(device)
    device.screenshot("jobs-05-nonzero-exit")
    summary["nonzero_exit_exact"] = 23
    journey.tap("All jobs")

    detail(device, journey, "Paged private log")
    journey.tap("Show raw log")
    m.scroll_to(device, "next private byte page", contains="Load next page", swipes=14)
    journey.tap("Load next page")
    # The large Text semantics contains PAGE_END even while its bottom is off-screen.
    m.scroll_to(device, "terminal cursor reached log end", contains="End of log", swipes=14)
    device.wait("last private log marker", contains="PAGE_END")
    device.screenshot("jobs-06-paged-private-log")
    summary["private_log_next_page"] = True
    device.tap(device.wait("back to jobs from paged log", desc="Back"))
    device.wait("job list after header back", contains="Headless jobs")

    detail(device, journey, "Peer stop target")
    before_stop = {
        "peer": job(peer, stopping["id"])["status"],
        "hub": job(hub, survivor["id"])["status"],
    }
    if set(before_stop.values()) != {"working"}:
        raise m.CheckFailed("The private routing targets must still work before native Stop")
    summary["peer_stop_before"] = before_stop
    journey.tap("Stop job")
    device.wait("stop confirmation", text="Stop this job?")
    device.tap(device.wait("confirm stop", text="Stop job"))
    require(
        "Peer job did not stop",
        lambda: job(peer, stopping["id"])["status"] in {"finished", "failed"},
    )
    if job(hub, survivor["id"])["status"] != "working":
        raise m.CheckFailed("Stopping a peer job affected the hub job")
    ended = job(peer, stopping["id"])
    if ended.get("signal") is not None:
        device.wait("exact termination signal", contains=f"Signal {ended['signal']}")
    elif ended.get("exit_code") is not None:
        device.wait("exact stop exit", contains=f"Exit {ended['exit_code']}")
    else:
        device.wait("unknown stop exit", contains="Exit status unknown")
    no_composer(device)
    device.screenshot("jobs-07-peer-stopped")
    summary["peer_stop_isolation"] = {
        "peer": ended,
        "hub_status": job(hub, survivor["id"])["status"],
    }
    journey.tap("Dismiss job")
    device.wait("dismiss confirmation", text="Dismiss this job?")
    device.tap(device.wait("confirm dismiss", text="Dismiss job"))
    require(
        "Dismissed peer job remained listed",
        lambda: not any(value["id"] == stopping["id"] for value in jobs(peer)),
    )
    if job(hub, survivor["id"])["status"] != "working":
        raise m.CheckFailed("Dismissing a peer job affected the hub job")
    device.wait("job list after dismissal", contains="Headless jobs")
    device.screenshot("jobs-08-dismissed")
    summary["peer_dismiss_isolation"] = True

    # A cold reopen reads the retained completed job rather than an interactive chat.
    device.shell(f"am force-stop {m.PACKAGE}")
    device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
    journey.tools("Jobs")
    detail(device, journey, "Phone launched job")
    device.wait("retained completion after app restart", contains="Exit 0")
    device.wait("retained structured answer", contains="Native structured job answer")
    no_composer(device)
    device.screenshot("jobs-09-retained-after-reopen")
    summary["retained_jobs_survive_phone_restart"] = True


class LegacyCapabilities:
    """TLS relay changing only the private gateway's capability reply to protocol 12.

    This exercises the delivered APK's unsupported UI, not an old executable.
    Requires the task-local websockets package; all keys come from our fixture.
    """

    def __init__(self, deck: m.Deck):
        self.deck = deck
        self.port = m.free_port()
        while self.port == deck.port:
            self.port = m.free_port()
        self.rewritten = 0
        self.job_requests = 0
        self.connections: set = set()

    def fingerprint(self):
        return self.deck.fingerprint()

    def open_pairing(self):
        return self.deck.open_pairing()

    @contextlib.contextmanager
    def running(self):
        from websockets.sync.client import connect as ws_connect
        from websockets.sync.server import serve

        cert = self.deck.state / "remote/cert.der"
        key = self.deck.state / "remote/key.der"
        pem = self.deck.root / "legacy-tls.pem"
        pem.write_text(
            ssl.DER_cert_to_PEM_cert(cert.read_bytes())
            + "-----BEGIN PRIVATE KEY-----\n"
            + base64.encodebytes(key.read_bytes()).decode()
            + "-----END PRIVATE KEY-----\n"
        )
        pem.chmod(0o600)
        server_tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        server_tls.load_cert_chain(pem)
        upstream_tls = ssl.create_default_context(
            cadata=ssl.DER_cert_to_PEM_cert(cert.read_bytes())
        )
        upstream_tls.check_hostname = False

        def handler(client):
            with ws_connect(
                f"wss://127.0.0.1:{self.deck.port}/v1",
                ssl=upstream_tls,
                proxy=None,
                open_timeout=scaled(15),
                max_size=8 * 1024 * 1024,
            ) as upstream:
                self.connections.update((client, upstream))

                def from_gateway():
                    try:
                        for raw in upstream:
                            value = json.loads(raw)
                            data = value.get("data", {})
                            if (
                                value.get("t") == "control"
                                and value.get("action") == "fleet"
                                and "facilities" in data
                            ):
                                data["protocol"] = 12
                                data["facilities"] = [
                                    name for name in data["facilities"] if name != "jobs"
                                ]
                                self.rewritten += 1
                                raw = json.dumps(value)
                            client.send(raw)
                    finally:
                        client.close()

                worker = threading.Thread(target=from_gateway)
                worker.start()
                try:
                    for raw in client:
                        value = json.loads(raw)
                        if value.get("operation", "").startswith("job-"):
                            self.job_requests += 1
                            client.send(
                                json.dumps(
                                    {
                                        "t": "control",
                                        "rid": value.get("rid"),
                                        "action": "fleet",
                                        "data": {"error": "Update needed"},
                                    }
                                )
                            )
                        else:
                            upstream.send(raw)
                finally:
                    upstream.close()
                    worker.join(timeout=scaled(10))
                    self.connections.discard(client)
                    self.connections.discard(upstream)

        with serve(
            handler,
            "127.0.0.1",
            self.port,
            ssl=server_tls,
            open_timeout=scaled(15),
            max_size=8 * 1024 * 1024,
        ) as server:
            worker = threading.Thread(target=server.serve_forever)
            worker.start()
            try:
                yield self
            finally:
                for connection in tuple(self.connections):
                    connection.close()
                server.shutdown()
                worker.join(timeout=scaled(10))


def legacy_check(device: ui.Device, hub: m.Deck, summary: dict) -> None:
    journey = Journey(device, hub, summary)
    journey.home()
    with LegacyCapabilities(hub).running() as legacy:
        connections(device)
        device.tap(device.wait("add private legacy capability connection", text="Add connection"))
        pair(device, legacy, "Protocol 12 fixture")
        journey.tools("Jobs")
        device.wait(
            "explicit older gateway message", contains="Update the desktop deck, then reconnect."
        )
        if device.find(text="Start job") or legacy.job_requests:
            raise m.CheckFailed("The older-capability connection exposed or submitted job controls")
        if not legacy.rewritten:
            raise m.CheckFailed("The legacy-capability fixture was not exercised")
        device.screenshot("jobs-10-update-needed")
        summary["older_service_ui"] = {
            "fixture": "private TLS relay; protocol12/no-jobs capabilities",
            "rewritten_replies": legacy.rewritten,
            "job_requests": legacy.job_requests,
        }
        device.shell("am force-stop " + m.PACKAGE, check=False)


def stop_owned_jobs(deck: m.Deck) -> None:
    """The fixture owns every job in these private service roots."""
    if deck.service is None or deck.service.poll() is not None:
        return
    for value in jobs(deck):
        if value["status"] in {"starting", "working"} or (
            value["status"] == "failed" and value.get("exit_code") is None
        ):
            fleet(deck, "job-stop", job=value["id"])
    children = [
        json.loads(path.read_text())["child"]
        for path in (deck.state / "jobs").glob("*/launch.json")
    ]
    require(
        "A private fixture job remained active during cleanup",
        lambda: (
            all(value["status"] not in {"starting", "working"} for value in jobs(deck))
            and not any(identity_alive(child) for child in children)
        ),
    )


def main() -> None:
    ui.assert_private_guest()
    m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not is_managed_path(shots):
        raise m.CheckFailed("Captures must remain in managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    apk, binary = Path(os.environ["CEPHEUS_APK"]), Path(os.environ["CEPHEUS_BIN"])
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    summary = {
        "apk_sha256": hashlib.sha256(apk.read_bytes()).hexdigest(),
        "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "journey_executed": False,
    }
    with tempfile.TemporaryDirectory(prefix="jobs-phone-", dir=storage_root()) as temporary:
        root = Path(temporary)
        hub, peer = m.Deck(binary, root / "hub"), m.Deck(binary, root / "peer")
        while peer.port == hub.port:
            peer.port = m.free_port()
        for deck in (hub, peer):
            runtime = short_path(deck.root / "run")
            deck.env.update(
                XDG_RUNTIME_DIR=str(runtime), CEPHEUS_SOCKET=str(runtime / "deck.sock")
            )
        try:
            configure(hub, "This machine", 12288)
            configure(peer, "Fixture peer", 14336)
            runner = seed(hub)
            connect(hub, peer)
            peer.start()
            hub.start()
            require("Jobs services did not federate", lambda: peer_ready(hub))
            capability = fleet(hub, "capabilities")
            if capability.get("protocol", 0) < 13 or "jobs" not in capability.get("facilities", []):
                raise m.CheckFailed("This candidate does not implement protocol 13 jobs")
            m.settle(device)
            device.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            device.adb("install", "-r", "-g", str(apk), timeout=300)
            if device.shell(f"pm clear {m.PACKAGE}").strip() != "Success":
                raise m.CheckFailed("Could not reset the disposable app")
            device.shell(f"pm grant {m.PACKAGE} android.permission.POST_NOTIFICATIONS", check=False)
            device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
            pair(device, hub, "Jobs fixture")
            checks(device, hub, peer, runner, summary)
            legacy_check(device, hub, summary)
            summary.update(journey_executed=True, result="pass")
        except Exception as error:
            summary.update(result="fail", error=str(error))
            device.screenshot("jobs-failure")
            raise
        finally:
            cleanup_errors = []
            for name, deck in (("hub", hub), ("peer", peer)):
                try:
                    stop_owned_jobs(deck)
                except Exception as error:
                    cleanup_errors.append(f"{name}: {error}")
                finally:
                    deck.stop()
                for path in (deck.root / "service.log", deck.state / "remote.log"):
                    if path.exists():
                        shutil.copy2(path, shots / f"{name}-{path.name}")
            device.shell("am force-stop " + m.PACKAGE, check=False)
            if cleanup_errors:
                summary.update(result="fail", cleanup_errors=cleanup_errors)
            (shots / "jobs-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
            if cleanup_errors:
                raise m.CheckFailed("Private fixture cleanup failed: " + "; ".join(cleanup_errors))


if __name__ == "__main__":
    main()
