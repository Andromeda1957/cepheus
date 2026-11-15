#!/usr/bin/env python3
"""Deck tools journey against a synthetic service and a private emulator pool guest.

Requires the combined gateway/Lua candidate, CEPHEUS_BIN, CEPHEUS_APK,
CEPHEUS_SHOTS and a disposable emulator's private guest environment. No owner
service, repository, account, agent configuration or development port is used.
All data and captured evidence live in managed scratch and are removed by its
lease. A successful build alone does not certify this journey.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shlex
import shutil
import socket
import struct
import subprocess
import tempfile
import time
from pathlib import Path

import chat_ui_check as ui
from device_waits import at_rest

from test_support import scaled
from test_support import short_path
from test_support import storage_root

m = ui.m


def seed(deck: m.Deck) -> tuple[Path, Path]:
    """Only this fixture owns these files, agents, worktrees and scripts."""
    deck.seed_usage()
    work = deck.root / "work"
    for args in (
        ["init", "-b", "main"],
        ["config", "user.email", "fixture@example.invalid"],
        ["config", "user.name", "Deck tools fixture"],
    ):
        subprocess.run(["git", "-C", str(work), *args], check=True, capture_output=True)
    (work / "README.md").write_text("Synthetic repository for the Deck tools device check.\n")
    subprocess.run(["git", "-C", str(work), "add", "README.md"], check=True)
    subprocess.run(
        ["git", "-C", str(work), "commit", "-m", "fixture"], check=True, capture_output=True
    )
    branch = deck.root / "feature-worktree"
    subprocess.run(
        ["git", "-C", str(work), "worktree", "add", "-b", "phone-feature", str(branch)],
        check=True,
        capture_output=True,
    )
    agent = deck.root / "fixture-agent"
    agent.write_text(
        "#!/usr/bin/env python3\n"
        "import os, pathlib, sys, time\n"
        "args = sys.argv[1:]\n"
        "if '--add-dir' in args:\n"
        "    report = pathlib.Path(args[args.index('--add-dir') + 1])\n"
        "    report.mkdir(parents=True, exist_ok=True)\n"
        "    (report / 'report.md').write_text('Synthetic scan completed: no findings.\\n')\n"
        "    print('Synthetic scan completed', flush=True)\n"
        f"    deadline = time.monotonic() + {scaled(180)!r}\n"
        "    while not (report / '.observed-by-phone').exists() and time.monotonic() < deadline:\n"
        "        time.sleep(0.1)\n"
        "    raise SystemExit(0)\n"
        "if '-p' in args or '--print' in args or not sys.stdin.isatty():\n"
        "    print('Fixture quick answer: forty two', flush=True)\n"
        "    raise SystemExit(0)\n"
        "CHECK_START_DIR = os.getcwd()\n"
        + m.STAND_IN.replace("time.sleep(8)", "time.sleep(0.1)").replace(
            "time.sleep(4)", "time.sleep(0.1)"
        )
    )
    agent.chmod(0o700)
    config = json.loads(deck.config.read_text())
    config["host_name"] = "Phone fixture"
    for tool in ("claude", "codex", "antigravity", "grok", "opencode"):
        config[f"{tool}_command"] = [str(agent)]
    config["dev_server_command"] = ["sleep", "60"]
    config["shell_command"] = ["/bin/sh"]
    config["ask"] = {
        "web": {"tool": "claude", "model": ""},
        "code": {"tool": "claude", "model": ""},
        "timeout": 20,
    }
    scans = deck.root / "saved-scans"
    config["scans_dir"] = str(scans)
    deck.config.write_text(json.dumps(config))
    saved = scans / "phone-saved-report"
    (saved / "report").mkdir(parents=True)
    (saved / "report/report.md").write_text("Fixture saved report: reviewed from the phone.\n")
    (saved / "scan.json").write_text(
        json.dumps(
            {
                "id": saved.name,
                "repo": str(work),
                "started_at": int(time.time()) - 120,
                "finished_at": int(time.time()) - 60,
                "state": "saved",
                "dir": str(saved),
            }
        )
    )
    extension = deck.root / "phone-extension"
    skill = extension / "skills/phone-fixture-skill"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text(
        "---\nname: phone-fixture-skill\ndescription: Synthetic phone skill\n---\nFixture only.\n"
    )
    for tool in ("claude", "codex"):
        (deck.root / f"home/.{tool}/skills").mkdir(parents=True, exist_ok=True)
    (deck.root / "init.lua").write_text("""local deck = require("deck")
deck.command("phone-fixture", function(args)
  deck.notify("Fixture custom command ran")
  deck.ui.pick({title = "Fixture choice", items = {"First fixture", "Second fixture"},
    on_choose = function(item) deck.notify("Fixture selected: " .. tostring(item)) end})
end, {desc = "Synthetic phone command", complete = {"one", "two"}})
""")
    # A shell started by the fixture must not load the owner's shell configuration.
    deck.env["SHELL"] = "/bin/sh"
    return branch, extension


def service_connection(deck: m.Deck) -> socket.socket:
    """Observe this fixture's control connection across the service's exec."""
    connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        connection.settimeout(scaled(10))
        connection.connect(deck.env["CEPHEUS_SOCKET"])
        body = json.dumps({"t": "hello", "version": 10, "role": "control"}).encode()
        connection.sendall(struct.pack(">I", len(body)) + body)
        with connection.makefile("rb") as incoming:
            header = incoming.read(4)
            if len(header) != 4:
                raise m.CheckFailed("The fixture service did not welcome the observer")
            size = struct.unpack(">I", header)[0]
            if size > 2 * 1024 * 1024:
                raise m.CheckFailed("The fixture service welcome exceeds its bound")
            welcome = json.loads(incoming.read(size))
        if (
            welcome.get("t") != "welcome"
            or deck.service is None
            or welcome.get("pid") != deck.service.pid
        ):
            raise m.CheckFailed("The observer did not connect to this fixture's service")
        connection.settimeout(0.1)
        return connection
    except BaseException:
        connection.close()
        raise


def connection_closed(connection: socket.socket) -> bool:
    try:
        return not connection.recv(65536)
    except TimeoutError:
        return False
    except ConnectionResetError:
        return True


class Journey:
    def __init__(self, device: ui.Device, deck: m.Deck, summary: dict) -> None:
        self.d, self.deck, self.summary = device, deck, summary

    def tap(self, text: str) -> None:
        nodes = self.d.nodes()
        for _ in range(12):
            exact = [node for node in nodes if text in (node["text"], node["desc"])]
            matches = exact or [
                node for node in nodes if text in node["text"] or text in node["desc"]
            ]
            for node in matches:
                try:
                    target = ui.touch_target(nodes, node)
                except ValueError:
                    continue  # A page title is not the button with the same label.
                if target["enabled"]:
                    x = (node["bounds"][0] + node["bounds"][2]) // 2
                    y = (node["bounds"][1] + node["bounds"][3]) // 2
                    viewports = [
                        candidate["bounds"]
                        for candidate in nodes
                        if candidate["class"] == "android.widget.ScrollView"
                        and candidate["bounds"][0] <= x <= candidate["bounds"][2]
                        and candidate["bounds"][1] <= y <= candidate["bounds"][3]
                    ]
                    if viewports:
                        left, top, right, bottom = min(
                            viewports,
                            key=lambda bounds: (bounds[2] - bounds[0]) * (bounds[3] - bounds[1]),
                        )
                        x1, y1, x2, y2 = target["bounds"]
                        if not (left <= x1 < x2 <= right and top <= y1 < y2 <= bottom):
                            continue  # Scroll the entire action above system navigation.
                    self.d.tap(target)
                    return
            self.d.shell("input swipe 360 1100 360 500 300")
            # The next look reads the page once its fling has stopped.
            nodes = at_rest(self.d, scaled(5))
        raise m.CheckFailed(f"No enabled action named {text!r}")

    def evaluate_lua(self) -> None:
        self.tap("Evaluate Lua")
        self.d.wait("Lua evaluation confirmation", text="Evaluate this Lua code?")
        self.d.tap(self.d.wait("confirm Lua evaluation", text="Evaluate"))

    def fill(self, label: str, value: str) -> None:
        label_node = m.scroll_to(self.d, label, contains=label, swipes=12)
        fields = self.d.fields()
        x = (label_node["bounds"][0] + label_node["bounds"][2]) // 2
        y1 = label_node["bounds"][1]
        matching = [
            field
            for field in fields
            if field["bounds"][0] <= x <= field["bounds"][2]
            and field["bounds"][1] <= y1 <= field["bounds"][3]
        ]
        if not matching:
            raise m.CheckFailed(f"No field contains label {label!r}")
        self.d.type_into(matching[0], value)
        self.d.hide_keyboard()

    def type_terminal(self, line: str) -> None:
        """Type a line on the open terminal's screen and press Enter, as a keyboard does."""
        self.d.tap(self.d.wait("terminal screen", desc="Terminal screen. Tap to type."))
        # Chunk before encoding: splitting "%s" between ADB calls types it literally.
        for start in range(0, len(line), 16):
            escaped = line[start : start + 16].replace(" ", "%s")
            self.d.shell(f"input text {shlex.quote(escaped)}")
        self.d.shell("input keyevent 66")

    def home(self) -> None:
        self.d.hide_keyboard()
        for _ in range(8):
            if self.d.find(desc="Menu"):
                self.d.tap(self.d.wait("Menu", desc="Menu"))
                self.d.tap(self.d.wait("Home", text="Home"))
                self.d.wait("Sessions", text="Local agents")
                return
            self.d.shell("input keyevent 4")
        raise m.CheckFailed("Could not return to Home")

    def top(self) -> None:
        self.d.hide_keyboard()
        for _ in range(6):
            self.d.shell("input swipe 360 450 360 1350 250")

    def tools(self, page: str | None = None) -> None:
        self.home()
        self.d.tap(self.d.wait("Home Deck tools entry", desc="Deck tools"))
        self.d.wait("Deck tools page", text="Deck tools")
        if page:
            self.tap(page)

    def checks(self, branch: Path, extension: Path) -> None:
        # Settings entry and Home entry both reach the same standalone screen.
        self.home()
        m.open_screen(self.d, "Settings")
        self.tap("Deck tools")
        self.d.wait("Deck tools title", text="Deck tools")
        self.d.screenshot("tools-light")
        self.d.shell("cmd uimode night yes")
        self.d.wait("Deck tools after theme change", text="Deck tools")
        self.d.screenshot("tools-dark")
        self.summary["settings_and_home_entry"] = True

        self.tools("Branches")
        self.d.wait("fixture branch", timeout=60, text="phone-feature")
        self.d.screenshot("branches-dark")
        self.tap("New session in phone-feature")
        # Name and folder wait behind Advanced; the worktree arrives prefilled there.
        self.tap("Advanced")
        self.fill("Session name", "Phone branch agent")
        cwd = next((field for field in self.d.fields() if field["text"] == str(branch)), None)
        if cwd is None:
            raise m.CheckFailed("Branch creation did not prefill its worktree")
        self.tap("Done")
        self.tap("Start")
        self.d.wait("custom branch session", timeout=60, contains="Phone branch agent")
        m.wait_for(
            "session on selected branch",
            30,
            lambda: any(s.get("cwd") == str(branch) for s in self.deck.sessions()),
        )
        first = next(s["id"] for s in self.deck.sessions() if s.get("cwd") == str(branch))
        self.d.tap(self.d.wait("session More", desc="More"))
        self.tap("Rename session")
        self.fill("Session name", "Renamed phone agent")
        self.tap("Save")
        self.d.wait("renamed session", contains="Renamed phone agent")
        m.wait_for(
            "service renamed session",
            30,
            lambda: any(
                s["id"] == first and s.get("title") == "Renamed phone agent"
                for s in self.deck.sessions()
            ),
        )
        self.summary["branch_custom_cwd_and_rename"] = True

        # Create a second live agent through the native New session sheet.
        self.home()
        self.d.tap(self.d.wait("New session", desc="New session"))
        self.tap("Start")
        m.wait_for("second fixture agent", 30, lambda: len(self.deck.sessions()) >= 2)
        agents = [s["id"] for s in self.deck.sessions() if s["tool"] == "claude"]
        if len(agents) != 2:
            raise m.CheckFailed("Expected two synthetic Claude agents")

        self.tools("Ask")
        self.fill("Question", "What is forty two?")
        self.tap("Ask")
        self.d.wait("quick answer", timeout=60, contains="Fixture quick answer: forty two")
        # Reopen so the visible question can only come from history, not the draft/result.
        self.tools("Ask")
        self.tap("Refresh history")
        self.d.wait("saved question", contains="What is forty two?")
        self.d.screenshot("ask-dark")
        self.summary["ask_answer_and_history"] = True

        self.tools("Broadcast")
        self.fill("Message", "phone broadcast checkpoint")
        self.tap("Send to 2 agents")
        for session in agents:
            m.wait_for(
                "broadcast on PTY",
                30,
                lambda session=session: "phone broadcast checkpoint" in self.deck.screen(session),
            )
        self.d.screenshot("broadcast-dark")
        self.summary["broadcast_two_ptys"] = True

        self.tools("Terminals")
        self.tap("Options")
        self.fill("Terminal name", "Phone terminal")
        self.tap("Done")
        self.d.screenshot("terminals-dark")
        self.tap("Open terminal")
        self.d.wait("terminal title", timeout=60, text="Phone terminal")
        # Typing goes straight to the shell: a tap on the screen, the keyboard, Enter.
        self.type_terminal('echo terminal-result-"complete"')
        self.d.wait("terminal Enter output", contains="terminal-result-complete")
        self.type_terminal("sleep 60")
        self.tap("Ctrl C")
        self.type_terminal('echo interrupt-"complete"')
        self.d.wait("Ctrl-C interrupted the running program", contains="interrupt-complete")
        # The PTY fits the phone's screen by itself: portrait has room for fewer
        # than the 80 columns a terminal starts with.
        self.type_terminal("stty size")

        def fitted() -> bool:
            sizes = re.findall(r"^(\d+) (\d+)\s*$", self.deck.screen(terminal), re.MULTILINE)
            return any(20 <= int(cols) < 80 and int(rows) >= 5 for rows, cols in sizes)

        terminal = next(s["id"] for s in self.deck.sessions() if s.get("title") == "Phone terminal")
        m.wait_for("terminal PTY fitted to the phone", scaled(30), fitted)
        self.d.screenshot("terminal-dark")
        self.summary["terminal_type_enter_interrupt_fit"] = True

        self.tools("Scans")
        self.d.wait("saved scan", timeout=60, contains="phone-saved-report")
        self.tap("Read report")
        self.d.wait("saved report", contains="Fixture saved report: reviewed from the phone.")
        self.d.screenshot("scans-dark")
        # The fixture deck has one machine, so there is no picker and the scan starts there.
        self.fill("Scope or path (optional)", "README.md")
        self.tap("Start security scan")
        self.d.wait("scan session", timeout=90, contains="Security scan")
        m.wait_for(
            "new scan report",
            60,
            lambda: any(
                "Synthetic scan completed" in p.read_text()
                for p in (self.deck.root / "saved-scans").glob("*/report/report.md")
            ),
        )
        report = next(
            p
            for p in (self.deck.root / "saved-scans").glob("*/report/report.md")
            if "Synthetic scan completed" in p.read_text()
        )
        (report.parent / ".observed-by-phone").touch()

        def scan_saved() -> bool:
            try:
                metadata = json.loads((report.parent.parent / "scan.json").read_text())
                return metadata.get("state") == "saved" and metadata.get("finished_at") is not None
            except (OSError, json.JSONDecodeError):
                return False

        m.wait_for("completed synthetic scan", scaled(60), scan_saved)
        self.summary["saved_scan_report_and_new_scan"] = True

        self.tools("Extensions")
        self.fill("Extension source", str(extension))
        self.tap("Preview installation")
        self.d.wait("extension preview", timeout=60, contains="phone-fixture-skill")
        self.tap("Apply installation")
        self.tap("Apply")
        installed = self.deck.root / "home/.claude/skills/phone-fixture-skill"
        m.wait_for("installed skill link", 60, installed.is_symlink)
        self.tap("Sync skills on all machines")
        self.d.wait("skills sync report", timeout=60, contains="linked")
        self.d.screenshot("extensions-dark")
        self.summary["extension_preview_apply_and_sync"] = True

        self.tools("Scripting")
        self.d.wait("custom command", timeout=60, contains="phone-fixture")
        # One tap runs a command; its result lands at the top of the page.
        self.tap("Run phone-fixture")
        self.top()
        self.d.wait("Lua picker", timeout=30, text="Fixture choice")
        self.tap("Second fixture")
        self.d.wait(
            "picker callback result", timeout=30, contains="Fixture selected: Second fixture"
        )
        self.d.screenshot("scripting-dark")
        # A completion runs the command with that word, also in one tap.
        self.top()
        self.tap("one")
        self.d.wait("picker from a completion", timeout=30, text="Fixture choice")
        self.tap("Cancel")
        self.summary["custom_command_tap_completion_and_picker"] = True
        self.tap("Advanced")
        self.fill("Lua code", "=6*7")
        self.evaluate_lua()
        self.top()
        self.d.wait("Lua result", timeout=30, text="42")
        self.tap("Reload scripts")
        self.summary["lua_eval_and_reload"] = True
        self.fill("Lua code", 'require("deck").term({name="Lua phone shell"})')
        self.evaluate_lua()
        self.d.wait("script-created terminal title", timeout=60, text="Lua phone shell")
        self.d.wait("script-created terminal keyboard", timeout=60, desc="Keyboard")
        self.summary["scripted_shell_navigation"] = True

        self.tools("Service")
        self.d.screenshot("service-dark")
        expected = {session["id"] for session in self.deck.sessions() if session.get("alive")}
        with service_connection(self.deck) as observer:
            self.tap("Restart deck services")
            self.tap("Confirm")
            self.d.wait("restart acknowledged", timeout=60, contains="Requested operation: restart")
            m.wait_for(
                "pre-restart service connection closed",
                scaled(60),
                lambda: connection_closed(observer),
            )

        def restarted() -> bool:
            try:
                alive = {session["id"] for session in self.deck.sessions() if session.get("alive")}
                return len(expected) >= 2 and expected <= alive
            except m.CheckFailed:
                return False

        m.wait_for("fixture service after restart", 60, restarted)
        self.home()
        m.open_screen(self.d, "Settings")
        self.tap("Reconnect now")
        self.d.wait("reconnected phone", timeout=60, text="Connected")
        self.tools("Service")
        if json.loads(self.deck.ctl("status")).get("dev_server", {}).get("pid") is not None:
            raise m.CheckFailed("The fixture development process already runs before Start")
        self.tap("Start development server")
        self.tap("Confirm")
        self.d.wait("development server start acknowledged", contains="Requested operation: on")
        m.wait_for(
            "fixture development process started",
            30,
            lambda: (
                json.loads(self.deck.ctl("status")).get("dev_server", {}).get("pid") is not None
            ),
        )
        dev_pid = json.loads(self.deck.ctl("status"))["dev_server"]["pid"]
        if not isinstance(dev_pid, int) or dev_pid < 2:
            raise m.CheckFailed("The fixture development process has no valid pid")
        self.tap("Stop development server")
        self.tap("Confirm")
        self.d.wait("development server stop acknowledged", contains="Requested operation: off")
        m.wait_for(
            "fixture development process stopped",
            30,
            lambda: json.loads(self.deck.ctl("status")).get("dev_server", {}).get("pid") is None,
        )

        def dev_exited() -> bool:
            try:
                os.kill(dev_pid, 0)
                return False
            except ProcessLookupError:
                return True

        m.wait_for("fixture development process exited", scaled(30), dev_exited)
        self.summary["service_restart_and_dev_controls"] = True
        self.tap("Stop the empty service")
        self.tap("Confirm")
        self.d.wait("live Stop refused", contains="Cannot stop the service while sessions are live")
        if self.deck.service is None or self.deck.service.poll() is not None:
            raise m.CheckFailed("Refused Stop ended the fixture service")
        for session in self.deck.sessions():
            self.deck.ctl("close", str(session["id"]))
        m.wait_for(
            "fixture sessions explicitly closed", scaled(30), lambda: not self.deck.sessions()
        )
        self.summary["live_stop_refused"] = True
        self.tap("Stop the empty service")
        self.tap("Confirm")

        service = self.deck.service
        if service is None:
            raise m.CheckFailed("The fixture service was not started")
        m.wait_for("fixture service process exited", scaled(30), lambda: service.poll() is not None)
        if service.returncode != 0:
            raise m.CheckFailed(f"The fixture service failed during Stop: {service.returncode}")
        self.summary["empty_service_stop_last"] = True


def main() -> None:
    ui.assert_private_guest()
    m.guard()
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    if not ui.is_managed_path(shots):
        raise m.CheckFailed("Captures must remain in managed scratch")
    shots.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="deck-tools-", dir=storage_root()))
    if not ui.is_managed_path(root):
        raise m.CheckFailed("Fixture must remain in managed scratch")
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    # Git uses physical paths; only Unix sockets need the lease's short alias.
    runtime = short_path(root / "run")
    deck.env["XDG_RUNTIME_DIR"] = str(runtime)
    deck.env["CEPHEUS_SOCKET"] = str(runtime / "deck.sock")
    device = ui.Device(os.environ["ANDROID_SERIAL"], shots)
    apk = Path(os.environ["CEPHEUS_APK"])
    summary = {
        "apk_sha256": hashlib.sha256(apk.read_bytes()).hexdigest(),
        "gateway_sha256": hashlib.sha256(deck.binary.read_bytes()).hexdigest(),
        "journey_executed": False,
    }
    try:
        branch, extension = seed(deck)
        device.shell("pm disable-user --user 0 com.google.android.tts", check=False)
        device.shell("settings put secure show_ime_with_hard_keyboard 0")
        device.shell("cmd uimode night no")
        deck.start()
        m.journey(device, deck, apk, summary, stats_only=True)
        Journey(device, deck, summary).checks(branch, extension)
        summary["journey_executed"] = True
        summary["result"] = "pass"
    except Exception as error:
        summary["result"] = "fail"
        summary["error"] = str(error)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            detail = error.stderr
            if isinstance(detail, bytes):
                detail = detail.decode(errors="replace")
            summary["stderr"] = detail
            print(detail, flush=True)
        device.screenshot("tools-failure")
        raise
    finally:
        (shots / "tools-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        deck.stop()
        device.shell("am force-stop " + m.PACKAGE, check=False)
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
