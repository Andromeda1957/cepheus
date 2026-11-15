#!/usr/bin/env python3
"""Native Queue acceptance on one private service and a task-owned headless guest.

CEPHEUS_BIN/APK/SHOTS identify frozen artifacts. No real models, installed
service, owner device, visible windows or reserved port are used. --preflight
checks the same private service fixtures without starting any Android guest.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import hashlib
import json
import os
import shutil
import ssl
import sys
import tempfile
import threading
from pathlib import Path

import chat_ui_check as ui
from connections_check import connections
from connections_check import pair
from deck_tools_check import Journey
from jobs_check import Device
from jobs_check import require
from jobs_check import stop_owned_jobs
from scheduler_check import configure
from scheduler_check import fleet
from scheduler_check import memory

from test_support import scaled
from test_support import is_managed_path
from test_support import short_path
from test_support import storage_root

m = ui.m
LITERAL = "two words; $(no-command) *"


def configure_queue(deck: m.Deck) -> None:
    configure(deck, "This machine", 12288)
    # New sessions receive their prompt as a literal startup argument. Keep the
    # shared Scheduler fake unchanged; this Queue wrapper records that boundary.
    wrapper = deck.root / "queue-agent.py"
    wrapper.write_text(
        "import json,os,pathlib,runpy,sys\n"
        "root=pathlib.Path(os.environ['FLEET_FIXTURE_CAPTURE'])\n"
        "target=root/(os.environ['CEPHEUS_SESSION']+'.launch.json')\n"
        "target.write_text(json.dumps({'argv':sys.argv[1:]}))\n"
        "runpy.run_path(str(pathlib.Path(__file__).with_name('agent.py')),run_name='__main__')\n"
    )
    config = json.loads(deck.config.read_text())
    config["codex_command"] = [sys.executable, str(wrapper), "codex"]
    deck.config.write_text(json.dumps(config))


def listing(deck: m.Deck) -> dict:
    return fleet(deck, "queue-list")


def item(deck: m.Deck, title: str) -> dict | None:
    return next((entry for entry in listing(deck)["items"] if entry["title"] == title), None)


def spec(deck: m.Deck, key: str, title: str, mode: str) -> dict:
    return {
        "key": key,
        "title": title,
        "kind": "job",
        "argv": [sys.executable, str(deck.root / "work/queue-job.py"), mode],
        "cwd": str(deck.root / "work"),
        "placement": "local",
    }


def seed(deck: m.Deck) -> Path:
    runner = deck.root / "work/queue-job.py"
    runner.write_text(
        "import json,pathlib,sys,time\n"
        "root=pathlib.Path(__file__).parent\n"
        "mode=sys.argv[1]\n"
        "(root/('entry-'+mode+'.json')).write_text(json.dumps(sys.argv[1:]))\n"
        "if mode=='gate-a':\n"
        " deadline=time.monotonic()+1800\n"
        " while not (root/'release-a').exists():\n"
        "  if time.monotonic()>deadline: raise SystemExit(97)\n"
        "  time.sleep(0.05)\n"
        "(root/('record-'+mode+'.json')).write_text(json.dumps(sys.argv[1:]))\n"
        "print('Queue fixture '+mode+' complete',flush=True)\n"
    )
    # This command-backed source reads only a private Markdown checklist and
    # publishes metadata; it never derives or executes a prompt from the title.
    (deck.root / "work/TASKS.md").write_text("- [ ] Native source task\n")
    provider = deck.root / "work/source.py"
    provider.write_text(
        "import json,pathlib\n"
        "root=pathlib.Path(__file__).parent\n"
        "title=(root/'TASKS.md').read_text().strip()[6:]\n"
        "print(json.dumps({'tasks':[{'source_id':'native-markdown','id':'source-one','title':title,'state':'ready',"
        "'worktree':str(root),'branch':'fixture-source','next_step':'Reference text only; explicit work required'}]}))\n"
    )
    # The publisher credential returned by registration stays in private memory.
    fleet(
        deck,
        "source-register",
        id="native-markdown",
        owner="owner",
        repository="phone-fixture",
        mode="command",
        cwd=str(deck.root / "work"),
        argv=[sys.executable, str(provider)],
        interval_seconds=86400,
        enabled=True,
    )
    require(
        "Private task source did not publish",
        lambda: any(source["fresh"] for source in fleet(deck, "source-list")["sources"]),
    )
    for key, title, mode in (
        ("gate-a", "Dependency gate A", "gate-a"),
        ("cancel-one", "Cancel candidate", "cancelled"),
        ("reorder-one", "Reorder candidate", "reordered"),
    ):
        fleet(deck, "queue-add", **spec(deck, key, title, mode))
    return runner


def enabled_action(device: Device, text: str) -> dict | None:
    nodes = device.nodes()
    for node in nodes:
        if text not in (node["text"], node["desc"]):
            continue
        try:
            target = ui.touch_target(nodes, node)
        except ValueError:
            continue
        if target["enabled"]:
            return target
    return None


def confirm(device: Device, journey: Journey, title: str) -> None:
    device.wait(title, text=title)
    journey.tap("Confirm")


def details(device: Device, journey: Journey, title: str) -> None:
    # Back collapses an expanded card, or returns the unexpanded list to the
    # tool index. Reset that public selection before opening the exact target.
    device.hide_keyboard()
    device.shell("input keyevent 4")
    journey.top()
    if device.find(text="Deck tools"):
        journey.tap("Queue")
    device.wait("queue list after selection reset", contains="Queue dispatch ")
    journey.top()
    journey.tap("Work")
    journey.tap(title)
    m.scroll_to(device, "expanded queue actions", contains="Queue after this", swipes=12)
    device.wait("queue item detail", text="Queue after this")


def fill_job(
    journey: Journey, deck: m.Deck, title: str, mode: str, extra: str | None = None
) -> None:
    journey.fill("Work title", title)
    journey.fill("Program", sys.executable)
    arguments = [str(deck.root / "work/queue-job.py"), mode]
    if extra is not None:
        arguments.append(extra)
    journey.fill("Arguments (one per line)", "\n".join(arguments))
    journey.fill("Working directory", str(deck.root / "work"))
    journey.tap("This machine")


def chore(deck: m.Deck) -> dict | None:
    return next(
        (value for value in fleet(deck, "chore-list")["chores"] if value["id"] == "native-daily"),
        None,
    )


def preflight(deck: m.Deck) -> None:
    capability = fleet(deck, "capabilities")
    if capability.get("protocol", 0) < 14 or not {"work-queue", "task-sources", "chores"}.issubset(
        capability.get("facilities", [])
    ):
        raise m.CheckFailed("This candidate does not implement protocol14 Queue facilities")
    if listing(deck)["enabled"]:
        raise m.CheckFailed("A new queue was enabled by default")
    first = spec(deck, "preflight-one", "Preflight one", "preflight")
    added = fleet(deck, "queue-add", **first)["item"]
    repeated = fleet(deck, "queue-add", **first)["item"]
    if added["id"] != repeated["id"] or not added["cancelable"]:
        raise m.CheckFailed("Queue add did not preserve pending idempotency")
    ids = [entry["id"] for entry in listing(deck)["items"] if entry["cancelable"]]
    fleet(deck, "queue-reorder", ids=list(reversed(ids)))
    fleet(deck, "queue-cancel", id=added["id"])
    registration = {
        "id": "preflight-disabled",
        "owner": "owner",
        "repository": "phone-fixture",
        "title": "Preflight disabled chore",
        "target": "job",
        "schedule": {"cron": "0 0 1 1 *", "timezone": "UTC"},
        "spec": spec(deck, "preflight-chore", "Preflight disabled chore", "preflight-chore"),
        "enabled": False,
    }
    result = fleet(deck, "chore-register", **registration)["chore"]
    if result["enabled"] or result["effective_enabled"] or "token" in json.dumps(result):
        raise m.CheckFailed("Chore public registration is unsafe")
    fleet(deck, "chore-remove", id="preflight-disabled")


class QueueLegacyCapabilities:
    """TLS relay changing only the private gateway's capability reply to protocol 13 without Queue facilities.

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
        self.queue_requests = 0
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
                                data["protocol"] = 13
                                data["facilities"] = [
                                    name
                                    for name in data["facilities"]
                                    if name not in {"work-queue", "task-sources", "chores"}
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
                        if value.get("operation", "").startswith(("queue-", "source-", "chore-")):
                            self.queue_requests += 1
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
                            if value.get("operation", "").startswith("job-"):
                                self.job_requests += 1
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


def qualify_ordering(path: Path, apk_sha: str, backend_sha: str) -> dict:
    if not is_managed_path(path) or path.stat().st_size > 1024 * 1024:
        raise m.CheckFailed("Prior UI proof must be a bounded managed receipt")
    contents = path.read_bytes()
    previous = json.loads(contents)
    coverage = previous.get("fixture_summary", {})
    if (
        previous.get("apk_sha256") != apk_sha
        or previous.get("backend_sha256") != backend_sha
        or coverage.get("reorder_cancel_exact_targets") is not True
        or not coverage.get("explicit_disabled_add")
        or coverage.get("explicit_source_spec")
        != {"source_id": "native-markdown", "item_id": "source-one"}
    ):
        raise m.CheckFailed("Prior receipt does not qualify unchanged ordering/dependency UI")
    return {
        "receipt_sha256": hashlib.sha256(contents).hexdigest(),
        "prior_ui": ["complete-set reorder", "exact cancellation", "finished-dependency creation"],
        "current_ui_required": [
            "literal submission and body",
            "source fullspec submission and body",
            "all remaining checks",
        ],
    }


def seed_ordering(deck: m.Deck) -> tuple[dict, dict]:
    """Replay only metadata already verified through the unchanged native UI."""
    names = ("Cancel candidate", "Dependency gate A", "Reorder candidate", "Phone literal work")
    ids = [item(deck, title)["id"] for title in names]
    if listing(deck)["enabled"] or set(ids) != {
        entry["id"] for entry in listing(deck)["items"] if entry["cancelable"]
    }:
        raise m.CheckFailed("Continuation does not have the exact paused pending set")
    fleet(deck, "queue-reorder", ids=ids)
    fleet(deck, "queue-cancel", id=ids[0])
    gate_a = item(deck, "Dependency gate A")
    dependent = spec(deck, "continued-dependent", "Phone dependent B", "gate-b")
    dependent["dependencies"] = [{"queue": gate_a["id"], "condition": "finished"}]
    gate_b = fleet(deck, "queue-add", **dependent)["item"]
    if (
        gate_b["dependencies"] != dependent["dependencies"]
        or item(deck, "Cancel candidate")["reason"] != "cancelled"
    ):
        raise m.CheckFailed("Continuation metadata did not preserve exact identities")
    return gate_a, gate_b


def admission_receipt(deck: m.Deck, held: dict, summary: dict, name: str) -> None:
    proof = summary[name] = {
        "queue": held,
        "admission": fleet(deck, "admission"),
        "executor_headroom": fleet(deck)["headroom"],
        "entry_markers": [path.name for path in (deck.root / "work").glob("entry-*.json")],
        "completion_markers": [path.name for path in (deck.root / "work").glob("record-*.json")],
    }
    if (
        held["state"] != "held"
        or not held["reason"]
        or held["placement"] != "local"
        or proof["admission"]["admitted"] is not False
        or proof["admission"]["headroom"]["available_mib"] != 1024
        or proof["executor_headroom"]["available_mib"] != 1024
        or json.loads(deck.config.read_text()).get("hosts", [])
        or proof["entry_markers"]
        or proof["completion_markers"]
    ):
        raise m.CheckFailed("Held work lost local admission or entered a command body")
    dispatch = held.get("dispatch")
    if dispatch is not None:
        if dispatch["kind"] != "job" or held["cancelable"] is not False:
            raise m.CheckFailed("Accepted held work has an invalid typed custody handle")

        def starting_job():
            job = fleet(deck, "job-status", job=dispatch["id"])["jobs"][0]
            proof["job"] = job
            if (
                job["status"] != "starting"
                or job.get("started_at") is not None
                or job.get("host") not in (None, "local")
            ):
                raise m.CheckFailed("An accepted held intent started its body")
            return job if "headroom" in job["reason"] else None

        require("Accepted intent did not report its headroom hold", starting_job)


def qualify_interactive(path: Path, apk_sha: str, backend_sha: str) -> dict:
    if not is_managed_path(path) or path.stat().st_size > 1024 * 1024:
        raise m.CheckFailed("Prior native receipt must be bounded managed evidence")
    contents = path.read_bytes()
    previous = json.loads(contents)
    coverage = previous.get("fixture_summary", {})
    bodies = coverage.get("ui_submitted_job_bodies", {})
    chore_result = coverage.get("registered_chore_enable_run_schedule", {})
    held = coverage.get("admission_hold_after_ui", {})
    if (
        previous.get("apk_sha256") != apk_sha
        or previous.get("backend_sha256") != backend_sha
        or bodies.get("literal")
        != {"entry": ["literal", LITERAL], "completed": ["literal", LITERAL]}
        or bodies.get("source") != {"entry": ["source"], "completed": ["source"]}
        or coverage.get("literal_argv_and_finished_dependency", {}).get("job_navigation")
        is not True
        or coverage.get("atomic_batch") is not True
        or chore_result.get("outcome") != "done"
        or chore_result.get("schedule") != {"cron": "0 1 1 1 *", "timezone": "UTC"}
        or not chore_result.get("claim")
        or held.get("admission", {}).get("admitted") is not False
        or held.get("executor_headroom", {}).get("available_mib") != 1024
        or held.get("entry_markers") != []
        or held.get("completion_markers") != []
        or coverage.get("qualified_prior_ui", {}).get("prior_ui")
        != ["complete-set reorder", "exact cancellation", "finished-dependency creation"]
    ):
        raise m.CheckFailed("Prior receipt does not qualify the completed native prefix")
    return {
        "receipt_sha256": hashlib.sha256(contents).hexdigest(),
        "prior_ui": [
            "ordering/cancel/dependency forms",
            "literal/source UI and exact bodies",
            "admission and dependency holds",
            "job navigation",
            "chore run/schedule",
            "batch",
        ],
        "current_ui_required": [
            "interactive submission and literal startup argv",
            "session navigation",
            "cold reopen",
            "protocol13 Queue and Jobs update gates",
        ],
    }


def qualify_legacy(path: Path, apk_sha: str, backend_sha: str) -> dict:
    if not is_managed_path(path) or path.stat().st_size > 1024 * 1024:
        raise m.CheckFailed("Prior native receipt must be bounded managed evidence")
    contents = path.read_bytes()
    previous = json.loads(contents)
    coverage = previous.get("fixture_summary", {})
    expected = (
        '<cross-session-message source="cepheus" encoding="html">\n'
        "Queue phone interactive verification\n</cross-session-message>"
    )
    if (
        previous.get("apk_sha256") != apk_sha
        or previous.get("backend_sha256") != backend_sha
        or coverage.get("interactive_launch_argv", {}).get("argv") != ["codex", "--", expected]
        or coverage.get("interactive_dispatch_navigation")
        != {"kind": "session", "non_owner_envelope": True, "delivery": "literal startup argument"}
        or coverage.get("cold_reopen_retained_state") is not True
        or coverage.get("qualified_prior_ui", {}).get("prior_ui")
        != [
            "ordering/cancel/dependency forms",
            "literal/source UI and exact bodies",
            "admission and dependency holds",
            "job navigation",
            "chore run/schedule",
            "batch",
        ]
    ):
        raise m.CheckFailed("Prior receipt does not qualify the complete native prefix")
    return {
        "receipt_sha256": hashlib.sha256(contents).hexdigest(),
        "earlier_prefix": coverage["qualified_prior_ui"],
        "prior_ui": [
            "all earlier qualified native checks",
            "interactive UI and literal argv",
            "session navigation",
            "cold reopen and retained claim",
        ],
        "current_ui_required": ["protocol13 Queue and Jobs update gates"],
    }


def seed_interactive_prefix(deck: m.Deck) -> dict:
    # Only the already-verified prefix is replayed via API. The interactive spec
    # is still submitted by the actual native form below, never seeded here.
    for value in listing(deck)["items"]:
        if value["cancelable"]:
            fleet(deck, "queue-cancel", id=value["id"])
    fleet(
        deck,
        "chore-register",
        id="native-daily",
        owner="owner",
        repository="phone-fixture",
        title="Phone scheduled work",
        target="job",
        enabled=False,
        schedule={"cron": "0 1 1 1 *", "timezone": "UTC"},
        spec=spec(deck, "continued-chore", "Phone scheduled work", "chore"),
    )
    fleet(deck, "chore-enable", id="native-daily", enabled=True)
    fleet(deck, "queue-enable", enabled=True)
    fleet(deck, "chore-run", id="native-daily", key="continued-chore-run")
    completed = require(
        "Continued chore receipt did not complete",
        lambda: value if (value := chore(deck)).get("outcome") == "done" else None,
    )
    return completed["claim"]


def failure_context(deck: m.Deck, shots: Path) -> None:
    """Retain bounded synthetic-only evidence before the private fixture exits."""
    result = {
        "synthetic_fixture_only": True,
        "queue": listing(deck),
        "sessions": [],
        "captures": {},
        "specifications": [],
    }
    for session in deck.sessions()[:8]:
        result["sessions"].append({"state": session, "screen": deck.screen(session["id"])[:8192]})
    for path in sorted((deck.root / "capture").glob("*"))[:8]:
        if path.is_file():
            result["captures"][path.name] = path.read_bytes()[:16384].decode(errors="replace")
    result["queue"]["items"] = result["queue"]["items"][:100]
    fields = (
        "title",
        "kind",
        "argv",
        "prompt",
        "provider",
        "model",
        "cwd",
        "placement",
        "dependencies",
    )
    for path in sorted((deck.state / "work-queue").glob("batch-*/*.json"))[:32]:
        raw = json.loads(path.read_text()).get("spec", {})
        result["specifications"].append({key: raw[key] for key in fields if key in raw})
    encoded = json.dumps(result, indent=2, ensure_ascii=False)
    if len(encoded.encode()) > 1024 * 1024:
        result["specifications"] = [{"title": "omitted: bounded diagnostics"}]
        encoded = json.dumps(result, indent=2, ensure_ascii=False)
    (shots / "queue-failure-context.json").write_text(encoded + "\n")


def checks(device: Device, deck: m.Deck, summary: dict, continuation: bool = False) -> None:
    journey = Journey(device, deck, summary)
    journey.tools("Queue")
    device.wait("queue disabled by default", text="Queue dispatch paused")
    if not listing(deck).get("pause_effective") or listing(deck).get("pause_pending"):
        raise m.CheckFailed("The initial empty queue pause was not acknowledged")
    device.screenshot("queue-01-disabled")
    journey.tap("Add work")
    journey.fill("Work title", "Phone literal work")
    journey.top()
    journey.tap("Back to queue")
    # Leave the tool and MainShell page as well: the draft must keep both its
    # contents and stable request identity when the Queue screen is disposed.
    device.shell("input keyevent 4")
    device.wait("tool index with Queue draft retained", text="Deck tools")
    journey.tools("Queue")
    journey.tap("Resume draft")
    device.wait("Back preserved the unsubmitted title", contains="Phone literal work")
    fill_job(journey, deck, "Phone literal work", "literal", LITERAL)
    journey.tap("Queue work")
    phone = require("Phone did not queue explicit job", lambda: item(deck, "Phone literal work"))
    if listing(deck)["enabled"] or phone.get("dispatch"):
        raise m.CheckFailed("Adding work implicitly enabled dispatch")
    summary["explicit_disabled_add"] = {"id": phone["id"], "state": phone["state"]}

    if continuation:
        gate_a, gate_b = seed_ordering(deck)
        summary["ordering_seeded_via_api"] = {"dependency": gate_a["id"], "dependent": gate_b["id"]}
    else:
        before = [value for value in listing(deck)["items"] if value["cancelable"]]
        moving = before[1]
        details(device, journey, moving["title"])
        journey.tap("Move up")
        confirm(device, journey, "Move item up?")
        expected = [value["id"] for value in before]
        expected[0], expected[1] = expected[1], expected[0]
        require(
            "Native reorder lost the complete item set",
            lambda: (
                [value["id"] for value in listing(deck)["items"] if value["cancelable"]] == expected
            ),
        )
        details(device, journey, "Cancel candidate")
        journey.tap("Cancel item")
        confirm(device, journey, "Cancel queued work?")
        require(
            "Native cancellation did not target its pending item",
            lambda: item(deck, "Cancel candidate")["reason"] == "cancelled",
        )
        if (deck.root / "work/record-cancelled.json").exists() or not item(
            deck, "Phone literal work"
        )["cancelable"]:
            raise m.CheckFailed("Cancellation executed work or changed another item")
        summary["reorder_cancel_exact_targets"] = True

        details(device, journey, "Dependency gate A")
        gate_a = item(deck, "Dependency gate A")
        journey.tap("Queue after this")
        fill_job(journey, deck, "Phone dependent B", "gate-b")
        journey.tap("Queue work")
        gate_b = require(
            "Native after action did not add dependent work",
            lambda: item(deck, "Phone dependent B"),
        )
        if gate_b["dependencies"] != [{"queue": gate_a["id"], "condition": "finished"}]:
            raise m.CheckFailed("The native dependency did not retain its named milestone")

    journey.top()
    journey.tap("Sources")
    device.wait("registered source metadata", text="native-markdown")
    device.wait("source branch metadata", contains="fixture-source")
    device.screenshot("queue-02-source")
    journey.tap("Queue source task")
    fill_job(journey, deck, "Phone source work", "source")
    journey.tap("Queue work")
    sourced = require("Native source task was not queued", lambda: item(deck, "Phone source work"))
    if sourced["source"] != {"source_id": "native-markdown", "item_id": "source-one"}:
        raise m.CheckFailed("Source queueing lost composite identity")
    summary["explicit_source_spec"] = sourced["source"]

    journey.top()
    journey.tap("Work")
    memory(deck, 1024)
    require(
        "Private headroom probe was not refused",
        lambda: fleet(deck, "admission")["admitted"] is False,
    )
    journey.tap("Enable queue")
    confirm(device, journey, "Enable queue?")
    held = require(
        "Queue admission did not hold work",
        lambda: value if (value := item(deck, "Phone literal work"))["state"] == "held" else None,
    )
    admission_receipt(deck, held, summary, "admission_hold")
    details(device, journey, "Phone literal work")
    device.wait("held queue reason", contains=held["reason"])
    device.screenshot("queue-03-held-admission")
    admission_receipt(deck, item(deck, "Phone literal work"), summary, "admission_hold_after_ui")

    memory(deck, 12288)
    require("Gate A did not dispatch", lambda: item(deck, "Dependency gate A").get("dispatch"))
    require(
        "Literal job did not complete", lambda: item(deck, "Phone literal work")["state"] == "done"
    )
    require(
        "Source job did not complete", lambda: item(deck, "Phone source work")["state"] == "done"
    )
    bodies = summary["ui_submitted_job_bodies"] = {}
    for mode, expected in (("literal", ["literal", LITERAL]), ("source", ["source"])):
        entry = json.loads((deck.root / f"work/entry-{mode}.json").read_text())
        completed = json.loads((deck.root / f"work/record-{mode}.json").read_text())
        bodies[mode] = {"entry": entry, "completed": completed}
        if entry != expected or completed != expected:
            raise m.CheckFailed("UI-submitted arguments were shell-parsed or changed")
    require(
        "Gate A never entered its bounded wait",
        lambda: (deck.root / "work/entry-gate-a.json").exists(),
    )
    gate_b = item(deck, "Phone dependent B")
    if (
        gate_b.get("dispatch")
        or (deck.root / "work/entry-gate-b.json").exists()
        or (deck.root / "work/record-gate-b.json").exists()
    ):
        raise m.CheckFailed("Dependent work started before A finished")
    details(device, journey, "Phone dependent B")
    device.wait("dependency condition", contains=f"Queue #{gate_a['id']} · finished")
    device.screenshot("queue-04-dependency-held")
    (deck.root / "work/release-a").touch()
    require(
        "Dependent B did not start after A completed",
        lambda: item(deck, "Phone dependent B")["state"] == "done",
    )
    if (
        item(deck, "Dependency gate A")["state"] != "done"
        or not (deck.root / "work/entry-gate-b.json").exists()
        or not (deck.root / "work/record-gate-b.json").exists()
    ):
        raise m.CheckFailed("The dependency result was incomplete")
    journey.top()
    details(device, journey, "Phone dependent B")
    device.wait("completed dependent card", contains=f"#{gate_b['id']} · job · done")
    journey.tap("Open job")
    device.wait("separate job navigation", text="Read-only output")
    device.wait("dispatched job's own log", contains="Queue fixture gate-b complete")
    device.screenshot("queue-05-dispatched-job")
    summary["literal_argv_and_finished_dependency"] = {
        "literal_argv": True,
        "dependency": gate_a["id"],
        "dependent": gate_b["id"],
        "job_navigation": True,
    }

    journey.tools("Queue")
    journey.tap("Chores")
    journey.tap("Register scheduled work")
    fill_job(journey, deck, "Phone scheduled work", "chore")
    journey.fill("Chore ID", "native-daily")
    journey.fill("Registered plugin owner", "owner")
    journey.fill("Repository identity", "phone-fixture")
    journey.fill("Cron schedule (minute hour day month weekday)", "0 0 1 1 *")
    journey.tap("UTC")
    journey.tap("Register disabled chore")
    registered = require("Native chore was not registered", lambda: chore(deck))
    if registered["enabled"] or registered.get("claim"):
        raise m.CheckFailed("Chore registration implicitly enabled or ran work")
    journey.top()
    journey.tap("Chores")
    journey.tap("Enable chore")
    confirm(device, journey, "Enable chore?")
    require("Native chore enable failed", lambda: chore(deck)["effective_enabled"])
    require("Run action did not enable", lambda: enabled_action(device, "Run now"))
    journey.tap("Run now")
    confirm(device, journey, "Run chore now?")
    completed = require(
        "Native chore did not complete",
        lambda: value if (value := chore(deck)).get("outcome") == "done" else None,
    )
    device.wait("public claim outcome", contains="Outcome: done")
    device.screenshot("queue-06-chore-outcome")
    old_claim = completed["claim"]
    if any(key in json.dumps(old_claim) for key in ('"token"', '"key"', '"occurrence"')):
        raise m.CheckFailed("The public chore claim exposed private custody fields")
    journey.tap("Edit schedule")
    device.wait("schedule dialog", text="Edit future schedule")
    journey.fill("Cron schedule", "0 1 1 1 *")
    journey.tap("Save schedule")
    changed = require(
        "Native future schedule update failed",
        lambda: value if (value := chore(deck))["schedule"]["cron"] == "0 1 1 1 *" else None,
    )
    if changed["claim"] != old_claim:
        raise m.CheckFailed("Updating a future schedule changed an existing claim")
    summary["registered_chore_enable_run_schedule"] = {
        "claim": old_claim,
        "outcome": completed["outcome"],
        "schedule": changed["schedule"],
    }

    journey.top()
    journey.tap("Work")
    journey.tap("Batch")
    batch = [
        spec(deck, "phone-batch-one", "Phone batch one", "batch-one"),
        spec(deck, "phone-batch-two", "Phone batch two", "batch-two"),
    ]
    journey.fill("Batch specifications (JSON array)", json.dumps(batch))
    journey.tap("Queue batch")
    require(
        "Native atomic batch did not complete",
        lambda: all(
            item(deck, title) and item(deck, title)["state"] == "done"
            for title in ("Phone batch one", "Phone batch two")
        ),
    )
    summary["atomic_batch"] = True

    interactive_checks(device, deck, summary, old_claim)


def launch_receipt(deck: m.Deck, session: int) -> dict:
    capture = deck.root / "capture" / f"{session}.launch.json"

    def complete_capture():
        try:
            return json.loads(capture.read_text())
        except (OSError, ValueError):
            return None

    launched = require("Queued prompt did not reach the literal startup argv", complete_capture)
    argv = launched["argv"]
    expected = (
        '<cross-session-message source="cepheus" encoding="html">\n'
        "Queue phone interactive verification\n</cross-session-message>"
    )
    if argv.count("--") != 1 or argv[argv.index("--") + 1 :] != [expected]:
        raise m.CheckFailed(
            "Queued prompt lost its single literal argument or non-owner provenance"
        )
    return launched


def interactive_checks(device: Device, deck: m.Deck, summary: dict, old_claim: dict) -> None:
    journey = Journey(device, deck, summary)
    journey.tools("Queue")
    journey.top()
    journey.tap("Add work")
    journey.fill("Work title", "Phone interactive work")
    journey.tap("Interactive session")
    journey.fill("Session prompt", "Queue phone interactive verification")
    journey.tap("Codex")
    journey.fill("Working directory", str(deck.root / "work"))
    journey.tap("This machine")
    journey.tap("Queue work")
    interactive = require(
        "Native interactive work did not dispatch",
        lambda: (
            value
            if (value := item(deck, "Phone interactive work")) and value.get("dispatch")
            else None
        ),
    )
    if interactive["dispatch"]["kind"] != "session":
        raise m.CheckFailed("Interactive work was addressed as a job")
    summary["interactive_launch_argv"] = launch_receipt(deck, interactive["dispatch"]["id"])
    details(device, journey, "Phone interactive work")
    journey.tap("Open session")
    device.wait("separate session navigation", contains="Phone interactive work")
    device.screenshot("queue-07-dispatched-session")
    summary["interactive_dispatch_navigation"] = {
        "kind": "session",
        "non_owner_envelope": True,
        "delivery": "literal startup argument",
    }

    device.shell("am force-stop " + m.PACKAGE)
    device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
    journey.tools("Queue")
    device.wait("retained queue enabled state", text="Queue dispatch enabled")
    journey.tap("Chores")
    device.wait("retained chore outcome", contains="Outcome: done")
    if chore(deck)["claim"] != old_claim:
        raise m.CheckFailed("Reopen changed the chore claim")
    device.screenshot("queue-08-reopened")
    summary["cold_reopen_retained_state"] = True


def legacy_check(device: Device, deck: m.Deck, summary: dict) -> None:
    journey = Journey(device, deck, summary)
    journey.home()
    with QueueLegacyCapabilities(deck).running() as legacy:
        connections(device)
        device.tap(device.wait("add protocol13 fixture", text="Add connection"))
        pair(device, legacy, "Protocol 13 queue fixture")
        journey.tools("Queue")
        device.wait(
            "explicit older queue gate", contains="Update the desktop deck, then reconnect."
        )
        if device.find(text="Add work") or legacy.queue_requests:
            raise m.CheckFailed("Older capabilities exposed or sent queue controls")
        device.screenshot("queue-09-update-needed")
        journey.tools("Jobs")
        device.wait("explicit older Jobs gate", contains="Update the desktop deck, then reconnect.")
        if device.find(contains="Headless jobs") or legacy.job_requests or legacy.queue_requests or not legacy.rewritten:
            raise m.CheckFailed("Protocol13 must expose neither Jobs nor Queue controls")
        device.screenshot("queue-10-older-jobs-update-needed")
        summary["older_service_ui"] = {
            "fixture": "private TLS relay protocol13/no Queue facilities; actual older binary not used",
            "rewritten_replies": legacy.rewritten,
            "queue_requests": legacy.queue_requests,
            "job_requests": legacy.job_requests,
        }
        device.shell("am force-stop " + m.PACKAGE, check=False)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument(
        "--continue-ordering-from",
        type=Path,
        help="Qualify prior unchanged ordering UI; current literal/source forms and bodies still run",
    )
    parser.add_argument(
        "--continue-interactive-from",
        type=Path,
        help="Qualify prior native prefix; current interactive form and remaining UI still run",
    )
    parser.add_argument(
        "--continue-legacy-from",
        type=Path,
        help="Qualify the completed native prefix; run actual protocol13 Queue/Jobs UI",
    )
    options = parser.parse_args()
    if (
        sum(
            value is not None
            for value in (
                options.continue_ordering_from,
                options.continue_interactive_from,
                options.continue_legacy_from,
            )
        )
        > 1
    ):
        parser.error("Choose one qualified continuation")
    preflight_only = options.preflight
    if not preflight_only:
        ui.assert_private_guest()
        m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not is_managed_path(shots):
        raise m.CheckFailed("Queue evidence must remain in managed storage")
    shots.mkdir(parents=True, exist_ok=True)
    binary = Path(os.environ["CEPHEUS_BIN"])
    apk = Path(os.environ["CEPHEUS_APK"])
    summary = {
        "apk_sha256": hashlib.sha256(apk.read_bytes()).hexdigest(),
        "gateway_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "journey_executed": False,
        "preflight_only": preflight_only,
    }
    continuation = options.continue_ordering_from is not None
    interactive_continuation = options.continue_interactive_from is not None
    legacy_continuation = options.continue_legacy_from is not None
    summary["journey_scope"] = (
        "continuation_at_legacy"
        if legacy_continuation
        else "continuation_at_interactive"
        if interactive_continuation
        else "continuation_after_ordering"
        if continuation
        else "full"
    )
    if legacy_continuation:
        summary["qualified_prior_ui"] = qualify_legacy(
            options.continue_legacy_from, summary["apk_sha256"], summary["gateway_sha256"]
        )
    if interactive_continuation:
        summary["qualified_prior_ui"] = qualify_interactive(
            options.continue_interactive_from, summary["apk_sha256"], summary["gateway_sha256"]
        )
    if continuation:
        summary["qualified_prior_ui"] = qualify_ordering(
            options.continue_ordering_from, summary["apk_sha256"], summary["gateway_sha256"]
        )
    device = None if preflight_only else Device(os.environ["ANDROID_SERIAL"], shots)
    with tempfile.TemporaryDirectory(prefix="queue-phone-", dir=storage_root()) as temporary:
        deck = m.Deck(binary, Path(temporary) / "hub")
        runtime = short_path(deck.root / "run")
        deck.env.update(XDG_RUNTIME_DIR=str(runtime), CEPHEUS_SOCKET=str(runtime / "deck.sock"))
        try:
            configure_queue(deck)
            deck.start()
            seed(deck)
            if preflight_only:
                preflight(deck)
                if continuation:
                    literal = spec(deck, "preflight-literal", "Phone literal work", "literal")
                    literal["argv"].append(LITERAL)
                    fleet(deck, "queue-add", **literal)
                    gate_a, gate_b = seed_ordering(deck)
                    summary["continuation_setup_preflight"] = {
                        "dependency": gate_a["id"],
                        "dependent": gate_b["id"],
                    }
                if interactive_continuation:
                    claim = seed_interactive_prefix(deck)
                    summary["continued_claim_preflight"] = claim
                    interactive_spec = {
                        "key": "preflight-interactive",
                        "title": "Preflight interactive work",
                        "kind": "session",
                        "provider": "codex",
                        "placement": "local",
                        "cwd": str(deck.root / "work"),
                        "prompt": "Queue phone interactive verification",
                    }
                    fleet(deck, "queue-add", **interactive_spec)
                    accepted = require(
                        "Preflight interactive intent did not dispatch",
                        lambda: (
                            value
                            if (value := item(deck, "Preflight interactive work")).get("dispatch")
                            else None
                        ),
                    )
                    summary["interactive_launch_argv"] = launch_receipt(
                        deck, accepted["dispatch"]["id"]
                    )
                    failure_context(deck, shots)
                summary.update(result="pass", preflight_completed=True)
                return
            m.settle(device)
            device.shell("pm disable-user --user 0 com.google.android.tts", check=False)
            device.adb("install", "-r", "-g", str(apk), timeout=300)
            if device.shell(f"pm clear {m.PACKAGE}").strip() != "Success":
                raise m.CheckFailed("Could not reset the disposable queue app")
            device.shell(f"pm grant {m.PACKAGE} android.permission.POST_NOTIFICATIONS", check=False)
            device.shell(f"am start -n {m.PACKAGE}/{m.ACTIVITY}")
            pair(device, deck, "Queue fixture")
            if interactive_continuation:
                claim = seed_interactive_prefix(deck)
                summary["prefix_seeded_via_api"] = {
                    "claim": claim,
                    "interactive_spec_seeded": False,
                }
                interactive_checks(device, deck, summary, claim)
            elif not legacy_continuation:
                checks(device, deck, summary, continuation=continuation)
            legacy_check(device, deck, summary)
            summary.update(journey_executed=True, result="pass")
        except Exception as error:
            summary.update(result="fail", error=str(error))
            try:
                failure_context(deck, shots)
            except Exception as diagnostic_error:
                summary["failure_context_error"] = str(diagnostic_error)
            if device is not None:
                device.screenshot("queue-failure")
            raise
        finally:
            cleanup_errors = []
            try:
                fleet(deck, "queue-enable", enabled=False)
                for value in listing(deck)["items"]:
                    if value["cancelable"]:
                        fleet(deck, "queue-cancel", id=value["id"])
                stop_owned_jobs(deck)
            except Exception as error:
                cleanup_errors.append(str(error))
            finally:
                deck.stop()
            for path in (deck.root / "service.log", deck.state / "remote.log"):
                if path.exists():
                    shutil.copy2(path, shots / path.name)
            if device is not None:
                device.shell("am force-stop " + m.PACKAGE, check=False)
            if cleanup_errors:
                summary.update(result="fail", cleanup_errors=cleanup_errors)
            (shots / "queue-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
            if cleanup_errors:
                raise m.CheckFailed(
                    "Private queue fixture cleanup failed: " + "; ".join(cleanup_errors)
                )


if __name__ == "__main__":
    main()
