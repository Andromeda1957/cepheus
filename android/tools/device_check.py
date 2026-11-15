#!/usr/bin/env python3
"""The phone journey on an emulator, against an isolated deck.

Run it against a disposable Android Studio emulator with ANDROID_SERIAL and
CEPHEUS_TEST_EMULATOR_SERIAL set to the same emulator ID. It never touches the owner's deck: the service, its
gateway and the stand-in program it hosts live under a fresh directory in this
run's scratch, with their own socket, state, configuration and HOME, and the
gateway listens on a free loopback port that the emulator reaches as
10.0.2.2. Everything it started is stopped and removed at the end.

Environment:
  ANDROID_SERIAL     the explicitly selected disposable emulator
  CEPHEUS_BIN     cepheus built from this checkout
  CEPHEUS_APK     the app's debug APK
  CEPHEUS_SHOTS   where screenshots and the summary go (default: the run directory)
  CEPHEUS_COMMANDS_ONLY  set to 1 for the focused Claude/Codex picker journey
  CEPHEUS_ONLY=system-bars  check all sheets in explicit and system light/dark themes
  CEPHEUS_RELEASE_APK  optional: first check that the signed release build starts

Use --stats-only for the pairing and remaining-quota regression check alone.

The journey: pair by typing the details and see the session list; with the
background connection off, as it is by default, no service runs and the app
lets its connection go once closed; Stats shows the desktop; Settings turns
the background connection on, which then outlasts closing the app. Then start
a session, send a message and see it on the program's screen, get a
"finished" notification, attach a file from the phone's Downloads and see its
path on the program's screen, answer a question from its notification, force
Doze and confirm an alert still arrives and the app is connected after it,
drag a second session above the first and see the deck keep that order, and
last open the drawer and its Settings button, whose page Back leaves for the
session it opened over, then use Home to return to the session list and reopen.
last send a message to a session read from its log and see it and
"Thinking…" before the log holds it, then exactly once after.

CEPHEUS_ONLY=search-keyboard checks real-IME dismissal on drawer/home outside taps.
CEPHEUS_ONLY=tool-details checks expanding tool cards while a turn is working.
CEPHEUS_ONLY=chat pairs and runs only that last step.
CEPHEUS_ONLY=activity checks an unlinked Codex and idle sent-message state.
CEPHEUS_ONLY=navigation checks Menu -> Home, reopening the session,
Settings' return path, and Android Back against the isolated deck.
CEPHEUS_ONLY=resume pairs and resumes a synthetic earlier Grok conversation
from the home screen's Resume button, then again from New session's
"Resume earlier…", which shows the same session.
CEPHEUS_ONLY=search pairs, tells one of two Claude sessions about agy
agents, types "AGY" in the drawer's search box and opens the one session it
finds, then sees the home list hold the same search until Back clears it.
CEPHEUS_ONLY=send_now pairs, queues a message behind a long Claude turn,
taps its Send now and sees the turn take it at once.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import re
import secrets
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse
import xml.etree.ElementTree as ElementTree
from pathlib import Path

PACKAGE = os.environ.get("CEPHEUS_PACKAGE", "io.github.cepheus.app.debug")
ACTIVITY = os.environ.get("CEPHEUS_ACTIVITY", "io.github.cepheus.app.ui.MainActivity")
CODE_ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"
# Hosted sessions carry the deck's variables under this prefix too, their
# name before the rename to Cepheus.
LEGACY_ENV_PREFIX = "AGENT_DECK_"  # legacy-name
ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))

from test_support import short_path  # noqa: E402
from test_support import storage_root  # noqa: E402

# The file attached from the phone's Downloads.
ATTACHMENT = "deck-note.txt"
# Deck.kt PAUSE_AFTER_MS: how long a closed app keeps its connection while
# the background connection is off.
PAUSE_SECONDS = 30
# How late the stand-in hosted as Claude writes a message to its transcript.
# Claude Code logs a message only once its prompt hooks finish: a median of
# 1.2 s and up to 9 s in this repository's sessions (2026-10-30).
LOG_DELAY = 8

# The program the isolated deck hosts in place of a real agent. It turns on
# bracketed paste as Claude Code and Codex do (the deck types into a session
# only then), echoes each line it is sent, shows the line the deck reads as
# "working" for a few seconds, and on a message ending "ask me" or "doze ask"
# shows a permission question under the rule Claude Code draws above it, then
# reports the option the arrow keys chose. Without the rule, the question card
# took in the whole screen above it (2026-10-27). An attached file's path,
# which can hold "task", must not ask. Hosted as Claude, it is given
# `--session-id` and keeps a transcript as Claude Code does, writing each
# message there @LOG_DELAY@ seconds after it arrives.
# On "pick several" it asks a
# several-choice question as Claude Code 2.1.314 does: Enter ticks the box
# under the cursor, a paste on "Type something" fills and ticks it, Enter on
# Submit shows the review of the answers, and one more Enter sends them.
# On "work long" it works for up to 90 s and holds what is typed meanwhile,
# as Claude Code 2.1.317 queues it, until its send-now keys (ctrl+x ctrl+s)
# arrive; then it takes that message at once. Flow control is off then, as
# Claude Code's raw mode has it, or ctrl+s would stop the output.
STAND_IN = r"""
import json, os, sys, termios, time
fd = sys.stdin.fileno()
attrs = termios.tcgetattr(fd)
attrs[3] &= ~termios.ECHO
termios.tcsetattr(fd, termios.TCSANOW, attrs)
args = sys.argv[1:]
transcript = None
if "--session-id" in args:
    start = CHECK_START_DIR
    slug = "".join(c if c.isascii() and c.isalnum() else "-" for c in start)
    transcript = os.path.join(os.environ["CLAUDE_CONFIG_DIR"], "projects", slug, args[args.index("--session-id") + 1] + ".jsonl")
def record(kind, text):
    if transcript is None:
        return
    os.makedirs(os.path.dirname(transcript), exist_ok=True)
    content = text if kind == "user" or isinstance(text, list) else [{"type": "text", "text": text}]
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime())
    with open(transcript, "a") as f:
        f.write(json.dumps({"type": kind, "timestamp": stamp, "message": {"role": kind, "content": content}}) + "\n")
def out(text):
    os.write(1, text.replace("\n", "\r\n").encode())
def clean(raw):
    return raw.replace("\x1b[200~", "").replace("\x1b[201~", "").strip()
out("\x1b[?2004hstand-in agent ready\n> ")
for raw in sys.stdin:
    text = clean(raw)
    if not text:
        out("> ")
        continue
    out("you said: " + text + "\n")
    if text == "/help":
        # A native command can be accepted without entering the transcript.
        out("help: choose a command\n> ")
        continue
    if text.endswith(("ask me", "doze ask")):
        out("\n" + "─" * 60 + "\n Bash command\n\n   rm -rf build\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel\n")
        keys = sys.stdin.readline()
        choice = keys.count("\x1b[B") + keys.count("\x1bOB") - keys.count("\x1b[A") - keys.count("\x1bOA")
        out("\x1b[2J\x1b[Hanswered: " + ("Yes", "No")[max(0, min(1, choice))] + "\n> ")
        continue
    if text.endswith("pick several"):
        labels = ["Cheese", "Olives", "Type something"]
        ticked = [False, False, False]
        cursor = 0  # rows: the three boxes, Submit, Chat about this
        def several():
            rows = ["Pick toppings?", ""]
            for i, label in enumerate(labels):
                rows.append(("❯" if cursor == i else " ") + f" {i + 1}. [{'✔' if ticked[i] else ' '}] {label}")
            rows += [("❯" if cursor == 3 else " ") + "    Submit", "─" * 40]
            rows += [("❯" if cursor == 4 else " ") + " 4. Chat about this", ""]
            out("\x1b[2J\x1b[H" + "\n".join(rows) + "\nEnter to select · ↑/↓ to navigate · Esc to cancel\n")
        several()
        while True:
            keys, i = sys.stdin.readline(), 0
            while i < len(keys):
                if keys.startswith(("\x1b[B", "\x1bOB"), i):
                    cursor, i = min(4, cursor + 1), i + 3
                elif keys.startswith(("\x1b[A", "\x1bOA"), i):
                    cursor, i = max(0, cursor - 1), i + 3
                elif keys.startswith("\x1b[200~", i):
                    end = keys.index("\x1b[201~", i)
                    if cursor == 2:
                        labels[2], ticked[2] = keys[i + 6 : end], True
                    i = end + 6
                else:
                    i += 1
            if cursor > 2:
                break
            ticked[cursor] = not ticked[cursor]
            several()
        if cursor == 4:
            out("\x1b[2J\x1b[Hdeclined to answer\n> ")
            continue
        out("\x1b[2J\x1b[HReview your answers\n\nReady to submit your answers?\n\n❯ 1. Submit answers\n  2. Cancel\n")
        sys.stdin.readline()
        out("\x1b[2J\x1b[Hpicked: " + ", ".join(l for l, t in zip(labels, ticked) if t) + "\n> ")
        continue
    if text.endswith("work long"):
        record("user", text)
        out("thinking (esc to interrupt)")
        line = termios.tcgetattr(fd)
        held = termios.tcgetattr(fd)
        held[0] &= ~termios.IXON
        held[3] &= ~termios.ICANON
        held[6][termios.VMIN], held[6][termios.VTIME] = 0, 10
        termios.tcsetattr(fd, termios.TCSANOW, held)
        got, until = b"", time.monotonic() + 90
        while b"\x18\x13" not in got and time.monotonic() < until:
            got += os.read(fd, 1024)
        termios.tcsetattr(fd, termios.TCSANOW, line)
        if b"\x18\x13" not in got:
            out("\r\x1b[2Kturn over\n> ")
            continue
        queued = clean(got.split(b"\x18\x13")[0].decode(errors="replace"))
        out("\r\x1b[2Ksent now: " + queued + "\nthinking (esc to interrupt)")
        record("user", queued)
        time.sleep(3)
        record("assistant", "done: " + queued)
        out("\r\x1b[2Kdone: " + queued + "\n> ")
        continue
    out("thinking (esc to interrupt)")
    if text == "busy tool details" and transcript is not None:
        record("user", text)
        for index in range(8):
            record("assistant", f"Earlier progress {index}: " + ("The agent is investigating the command output. " * 15))
        record("assistant", [{"type": "tool_use", "id": "busy-bash", "name": "Bash", "input": {"command": "printf busy-detail-input"}}])
        record("user", [{"type": "tool_result", "tool_use_id": "busy-bash", "content": "busy-detail-result"}])
        time.sleep(240)
        continue
    if transcript is not None:
        time.sleep(@LOG_DELAY@)
        record("user", text)
    time.sleep(4)
    record("assistant", "done: " + text)
    out("\r\x1b[2Kdone: " + text + "\n> ")
""".replace("@LOG_DELAY@", str(LOG_DELAY))


class CheckFailed(Exception):
    pass


def log(message: str) -> None:
    print(f"[device-check] {message}", flush=True)


class Device:
    def __init__(self, serial: str, shots: Path) -> None:
        sdk = Path(
            os.environ.get("ANDROID_HOME")
            or os.environ.get("ANDROID_SDK_ROOT")
            or Path.home() / "Android/Sdk"
        )
        self.adb_path = str(sdk / "platform-tools/adb")
        self.serial = serial
        self.shots = shots

    def adb(self, *args: str, check: bool = True, timeout: int = 120) -> str:
        result = subprocess.run(
            [self.adb_path, "-s", self.serial, *args],
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
        if check and result.returncode:
            raise CheckFailed(
                f"adb {' '.join(args[:3])} failed: {result.stderr.strip() or result.stdout.strip()}"
            )
        return result.stdout

    def shell(self, command: str, check: bool = True) -> str:
        return self.adb("shell", command, check=check)

    def screenshot(self, name: str) -> Path:
        path = self.shots / f"{name}.png"
        data = subprocess.run(
            [self.adb_path, "-s", self.serial, "exec-out", "screencap", "-p"],
            capture_output=True,
            timeout=60,
            check=True,
        ).stdout
        path.write_bytes(data)
        log(f"screenshot {path.name}")
        return path

    def nodes(self) -> list[dict]:
        dump = "/sdcard/cepheus-ui.xml"
        for _ in range(3):
            # A failed dump must not leave the previous screen behind.
            said = self.shell(f"rm -f {dump}; uiautomator dump {dump} 2>&1", check=False)
            xml = self.shell(f"cat {dump} 2>/dev/null", check=False)
            if "<hierarchy" in xml:
                self.last_dump = xml
                break
            time.sleep(1)
        else:
            log(f"no UI dump: {said.strip()[:200]}")
            return []
        out = []
        # The dump comes from this run's own disposable emulator; ElementTree
        # resolves no external entities.
        for node in ElementTree.fromstring(xml[xml.index("<hierarchy") :]).iter("node"):
            bounds = [int(n) for n in re.findall(r"-?\d+", node.get("bounds", "[0,0][0,0]"))]
            x1, y1, x2, y2 = bounds
            if x1 < 0 or x2 <= x1 or y2 <= y1:
                continue  # off screen, such as a closed drawer
            out.append(
                {
                    "text": node.get("text", ""),
                    "desc": node.get("content-desc", ""),
                    "class": node.get("class", ""),
                    "package": node.get("package", ""),
                    "bounds": bounds,
                    "clickable": node.get("clickable") == "true",
                    "enabled": node.get("enabled") == "true",
                }
            )
        return out

    def find(
        self,
        *,
        text: str | None = None,
        contains: str | None = None,
        desc: str | None = None,
        cls: str | None = None,
    ) -> list[dict]:
        found = []
        nodes = self.nodes()
        if any("isn't responding" in node["text"] for node in nodes):
            # A cold emulator's System UI can stall; let it finish.
            waits = [node for node in nodes if node["text"] == "Wait"]
            if waits:
                log("dismissed a system 'isn't responding' dialog")
                self.tap(waits[0])
                nodes = self.nodes()
        for node in nodes:
            # A merged Compose button may carry its label as either.
            if text is not None and text not in (node["text"], node["desc"]):
                continue
            if (
                contains is not None
                and contains not in node["text"]
                and contains not in node["desc"]
            ):
                continue
            if desc is not None and node["desc"] != desc:
                continue
            if cls is not None and node["class"] != cls:
                continue
            found.append(node)
        return found

    def chat_messages(self, text: str) -> list[dict]:
        """Match message text between the chat toolbar and composer."""
        nodes = self.nodes()
        anchors = [node for node in nodes if node["desc"] in {"Menu", "More"}]
        fields = [node for node in nodes if node["class"] == "android.widget.EditText"]
        if {node["desc"] for node in anchors} != {"Menu", "More"} or not fields:
            return []
        toolbar_bottom = 0
        for anchor in anchors:
            x1, y1, x2, y2 = anchor["bounds"]
            targets = [
                node
                for node in nodes
                if node["clickable"]
                and node["bounds"][0] <= x1
                and node["bounds"][1] <= y1
                and node["bounds"][2] >= x2
                and node["bounds"][3] >= y2
            ]
            if not targets:
                return []
            target = min(
                targets,
                key=lambda node: (
                    (node["bounds"][2] - node["bounds"][0])
                    * (node["bounds"][3] - node["bounds"][1])
                ),
            )
            toolbar_bottom = max(toolbar_bottom, target["bounds"][3])
        composer_top = min(node["bounds"][1] for node in fields)
        return [
            node
            for node in nodes
            if text in (node["text"], node["desc"])
            and toolbar_bottom <= node["bounds"][1]
            and node["bounds"][3] <= composer_top
        ]

    def wait(self, what: str, timeout: float = 30, **query) -> dict:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            found = self.find(**query)
            if found:
                return found[0]
            time.sleep(1)
        # Keep what uiautomator saw, for the diagnosis.
        (self.shots / "failure-ui.xml").write_text(getattr(self, "last_dump", ""))
        raise CheckFailed(f"timed out waiting for {what}")

    def tap(self, node: dict) -> None:
        x1, y1, x2, y2 = node["bounds"]
        self.shell(f"input tap {(x1 + x2) // 2} {(y1 + y2) // 2}")
        time.sleep(0.6)

    def type_into(self, node: dict, text: str) -> None:
        self.tap(node)
        # Empty the field from its end, then type in short runs.
        if node["text"]:
            self.shell("input keyevent 123 " + " ".join(["67"] * (len(node["text"]) + 2)))
        # Chunk before encoding: splitting "%s" between ADB calls types it literally.
        for start in range(0, len(text), 16):
            escaped = text[start : start + 16].replace(" ", "%s")
            self.shell(f"input text {shlex.quote(escaped)}")
        time.sleep(0.4)

    def hide_keyboard(self) -> None:
        # Android can fall back to voice input after settle() disables Gboard.
        # Back dismisses an IME; only send it when an IME is actually showing.
        state = self.shell("dumpsys input_method")
        if re.search(r"\b(?:mInputShown|mIsInputViewShown|isInputViewShown)\s*=\s*true\b", state):
            self.shell("input keyevent 4")

    def fields(self) -> list[dict]:
        return sorted(self.find(cls="android.widget.EditText"), key=lambda n: n["bounds"][1])

    def notifications(self) -> str:
        return self.shell("dumpsys notification --noredact", check=False)


class Deck:
    """An isolated cepheus service and its gateway under `root`."""

    def __init__(self, binary: Path, root: Path) -> None:
        self.binary = binary
        self.root = root
        self.state = root / "state"
        self.port = free_port()
        home = root / "home"
        for path in (
            home,
            root / "run",
            root / "work",
            root / "xdg-state",
            root / "xdg-config",
            root / "xdg-data",
        ):
            path.mkdir(parents=True, exist_ok=True)
        # Synthetic catalog; never use the owner's skills as writable fixtures.
        if os.environ.get("CEPHEUS_COMMANDS_ONLY") == "1":
            for agent in ("claude", "codex"):
                skill_root = home / f".{agent}" / "skills"
                for index in range(90):
                    skill = skill_root / f"phone-skill-{index:02}"
                    skill.mkdir(parents=True)
                    (skill / "SKILL.md").write_text(
                        f"---\nname: phone-skill-{index:02}\ndescription: Phone catalog skill {index}\n---\nPRIVATE BODY\n"
                    )
        os.chmod(root / "run", 0o700)
        stand_in = root / "stand_in.py"
        stand_in.write_text(f"CHECK_START_DIR = {str(root / 'work')!r}\n" + STAND_IN)
        command = ["sh", "-c", f"exec {shlex.quote(sys.executable)} {shlex.quote(str(stand_in))}"]
        # Claude's command passes on the deck's `--session-id`.
        claude = [
            "sh",
            "-c",
            f'exec {shlex.quote(sys.executable)} {shlex.quote(str(stand_in))} "$@"',
            "stand-in",
        ]
        config = {
            "remote": True,
            "remote_listen": f"127.0.0.1:{self.port}",
            "remote_allow": ["127.0.0.1"],
            "notifications": False,
            "claude_hooks": False,
            "codex_notify": False,
            "claude_command": claude,
            "codex_command": command,
            "antigravity_command": command,
            "grok_command": command,
            "opencode_command": command,
            # No port, so the deck never looks for a dev server.
            "dev_server_command": ["false"],
            "auto_continue": [],
            "start_dir": str(root / "work"),
        }
        self.config = root / "config.json"
        self.config.write_text(json.dumps(config, indent=2))
        # Nothing of the owner's deck or agents leaks in.
        env = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith(("CEPHEUS_", LEGACY_ENV_PREFIX, "CLAUDE", "CODEX", "GROK", "XDG_"))
        }
        env.update(
            {
                "HOME": str(home),
                "XDG_CONFIG_HOME": str(root / "xdg-config"),
                "XDG_STATE_HOME": str(root / "xdg-state"),
                "XDG_DATA_HOME": str(root / "xdg-data"),
                "XDG_RUNTIME_DIR": str(root / "run"),
                "CEPHEUS_SOCKET": str(short_path(root / "run/deck.sock")),
                "CEPHEUS_STATE_DIR": str(self.state),
                "CEPHEUS_CONFIG": str(self.config),
                "CEPHEUS_START_DIR": str(root / "work"),
                "CLAUDE_CONFIG_DIR": str(home / ".claude"),
                "CODEX_HOME": str(home / ".codex"),
                "GROK_HOME": str(home / ".grok"),
            }
        )
        self.env = env
        self.claude_dir = home / ".claude"
        self.grok_id = ""
        self.service: subprocess.Popen | None = None

    def start(self) -> None:
        log_file = open(self.root / "service.log", "wb")  # noqa: SIM115 - closed with the process
        self.service = subprocess.Popen(
            [str(self.binary), "service"],
            env=self.env,
            cwd=self.root / "work",
            stdin=subprocess.DEVNULL,
            stdout=log_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        cert = self.state / "remote/cert.der"
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            if cert.exists() and port_open(self.port):
                log(f"isolated deck up; gateway on 127.0.0.1:{self.port}")
                return
            if self.service.poll() is not None:
                raise CheckFailed("the isolated service exited; see service.log")
            time.sleep(0.5)
        raise CheckFailed("the gateway did not start within 40 seconds; see state/remote.log")

    def seed_usage(self) -> None:
        """The screenshot's three windows, isolated from the owner's account."""
        now = int(time.time())
        self.state.mkdir(parents=True, exist_ok=True)
        (self.state / "claude-usage.json").write_text(
            json.dumps(
                {
                    "updated_at": now,
                    "rate_limits": {
                        "five_hour": {"used_percentage": 2, "resets_at": now + 3_600},
                        "seven_day": {"used_percentage": 37, "resets_at": now + 604_800},
                    },
                }
            )
        )
        day = datetime.datetime.now(datetime.timezone.utc).strftime("%Y/%m/%d")
        rollouts = self.root / "home/.codex/sessions" / day
        rollouts.mkdir(parents=True, exist_ok=True)
        (rollouts / "rollout-quota.jsonl").write_text(
            json.dumps(
                {
                    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "rate_limits": {
                            "secondary": {
                                "used_percent": 0,
                                "window_minutes": 10_080,
                                "resets_at": now + 604_800,
                            },
                        },
                    },
                }
            )
            + "\n"
        )

    def seed_conversations(self) -> str:
        """Earlier Claude and Grok conversations of the start directory; returns Grok's id."""
        work = str(self.root / "work")
        slug = "".join(c if c.isascii() and c.isalnum() else "-" for c in work)
        transcript = (
            self.claude_dir / "projects" / slug / "c0ffee00-0000-4000-8000-000000000001.jsonl"
        )
        transcript.parent.mkdir(parents=True, exist_ok=True)
        transcript.write_text(
            json.dumps({"type": "user", "message": {"content": "Tidy the parser"}}) + "\n"
        )
        grok_id = "01a16bf6-a28a-7ce3-b29d-e722992ee7d8"
        folder = self.root / "home/.grok/sessions" / urllib.parse.quote(work, safe="")
        folder.mkdir(parents=True, exist_ok=True)
        prompt = {
            "timestamp": "2026-10-24T03:31:30Z",
            "session_id": grok_id,
            "prompt": "Fix the build",
            "is_bash": False,
        }
        (folder / "prompt_history.jsonl").write_text(json.dumps(prompt) + "\n")
        return grok_id

    def fingerprint(self) -> str:
        return hashlib.sha256((self.state / "remote/cert.der").read_bytes()).hexdigest()

    def open_pairing(self) -> str:
        code = "".join(secrets.choice(CODE_ALPHABET) for _ in range(8))
        path = self.state / "remote/pairing.json"
        path.write_text(
            json.dumps(
                {
                    "code_sha256": hashlib.sha256(code.encode()).hexdigest(),
                    "expires": int(time.time()) + 300,
                }
            )
        )
        os.chmod(path, 0o600)
        return code

    def ctl(self, *args: str) -> str:
        result = subprocess.run(
            [str(self.binary), "ctl", *args],
            env=self.env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        if result.returncode:
            raise CheckFailed(f"cepheus ctl {args[0]} failed: {result.stderr.strip()}")
        return result.stdout

    def sessions(self) -> list[dict]:
        return json.loads(self.ctl("status")).get("sessions", [])

    def screen(self, session: int) -> str:
        return self.ctl("screen", str(session))

    def devices(self) -> list[dict]:
        path = self.state / "remote/devices.json"
        return json.loads(path.read_text()) if path.exists() else []

    def stop(self) -> None:
        if self.service and self.service.poll() is None:
            try:
                for session in self.sessions():
                    self.ctl("close", str(session["id"]))
                wait_for("fixture sessions explicitly closed", 10, lambda: not self.sessions())
            except (CheckFailed, OSError, ValueError, subprocess.SubprocessError):
                pass  # Existing fixture-only process fallback still handles a broken service.
            subprocess.run(
                [str(self.binary), "stop", "--yes"],
                env=self.env,
                capture_output=True,
                timeout=30,
                check=False,
            )
            try:
                self.service.wait(timeout=20)
            except subprocess.TimeoutExpired:
                os.killpg(self.service.pid, signal.SIGTERM)
                self.service.wait(timeout=10)
        # The gateway is detached from the service; find it by this run's state directory.
        marker = f"CEPHEUS_STATE_DIR={self.state}".encode()
        for proc in Path("/proc").iterdir():
            if not proc.name.isdigit():
                continue
            try:
                if marker in (proc / "environ").read_bytes().split(b"\0"):
                    os.kill(int(proc.name), signal.SIGTERM)
                    log(f"stopped this run's process {proc.name}")
            except (OSError, ValueError):
                continue


def free_port() -> int:
    while True:
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        if port != 8000:
            return port


def port_open(port: int) -> bool:
    with socket.socket() as s:
        s.settimeout(0.5)
        return s.connect_ex(("127.0.0.1", port)) == 0


def guard() -> None:
    from test_support import assert_private_emulator

    assert_private_emulator()


def has_notification(dump: str, tag: str) -> bool:
    return any(f"pkg={PACKAGE}" in line and f"tag={tag}" in line for line in dump.splitlines())


def wait_for(what: str, timeout: float, check) -> None:
    deadline = time.monotonic() + timeout
    while not check():
        if time.monotonic() > deadline:
            raise CheckFailed(f"timed out waiting for {what}")
        time.sleep(1)


def status_of(deck: Deck, session: int) -> str:
    return next((s["status"] for s in deck.sessions() if s["id"] == session), "gone")


def offline(device: Device) -> bool:
    return bool(
        device.find(contains="Reconnecting") or device.find(contains="Connecting to the deck")
    )


def send_from_app(device: Device, deck: Deck, session: int, text: str) -> None:
    wait_for(
        "the session to be ready",
        30,
        lambda: status_of(deck, session) not in ("starting", "working"),
    )
    device.type_into(device.fields()[-1], text)
    actual = device.fields()[-1]["text"]
    if actual != text:
        raise CheckFailed(f"the message field holds {actual!r}, expected {text!r}")
    device.tap(device.wait("Send", desc="Send"))


def gateway_connections(port: int) -> int:
    """Connections the isolated gateway has accepted and not yet closed.

    The gateway logs no ordinary leave (server.rs `dropped`), so the check
    counts its established sockets instead: the emulator reaches it through
    the host's loopback, where it is the only client.
    """
    count = 0
    for table in ("/proc/net/tcp", "/proc/net/tcp6"):
        try:
            rows = Path(table).read_text().splitlines()[1:]
        except OSError:
            continue
        for row in rows:
            fields = row.split()
            if (
                len(fields) > 3
                and fields[3] == "01"
                and int(fields[1].rsplit(":", 1)[1], 16) == port
            ):
                count += 1
    return count


def service_running(device: Device) -> bool:
    return "DeckService" in device.shell(f"dumpsys activity services {PACKAGE}", check=False)


def open_screen(device: Device, item: str) -> None:
    """Open Settings from the drawer's foot, and from there Stats or Errors."""
    device.tap(device.wait("the menu button", desc="Menu"))
    device.tap(device.wait("the drawer's Settings button", desc="Settings"))
    # Its first row; the drawer no longer has one.
    stats_row = device.wait("the Settings page's Stats row", timeout=10, text="Stats")
    if item == "Stats":
        device.tap(stats_row)
    elif item != "Settings":
        device.tap(device.wait(f"Settings' {item} row", text=item))


def scroll_to(device: Device, what: str, *, contains: str, swipes: int = 6) -> dict:
    """Scroll a page up until a row containing the text is on screen."""
    for _ in range(swipes):
        found = device.find(contains=contains)
        if found:
            return found[0]
        device.shell("input swipe 360 1100 360 500 300")
        time.sleep(0.8)
    return device.wait(what, timeout=5, contains=contains)


def wait_enabled(device: Device, desc: str, timeout: float = 30) -> dict:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = [node for node in device.find(desc=desc) if node["enabled"]]
        if found:
            return found[0]
        time.sleep(1)
    raise CheckFailed(f"the {desc} button stayed disabled")


def pick_file(device: Device, name: str) -> None:
    """Choose `name` in the system file picker: from Recent, else from Downloads."""
    device.wait("the file picker", timeout=20, desc="Show roots")
    deadline = time.monotonic() + 40
    roots_opened_at = time.monotonic() + 6
    while time.monotonic() < deadline:
        found = device.find(text=name)
        if found:
            device.tap(found[0])
            time.sleep(1)
            # A picker that took the tap as a selection waits for Select.
            select = device.find(text="Select") or device.find(desc="Select")
            if select:
                device.tap(select[0])
            return
        if roots_opened_at and time.monotonic() > roots_opened_at:
            device.tap(device.wait("the picker's roots", desc="Show roots"))
            device.tap(device.wait("the picker's Downloads", timeout=10, text="Downloads"))
            roots_opened_at = 0
            continue
        time.sleep(1)
    raise CheckFailed(f"the file picker never showed {name}")


def session_rows(device: Device) -> list[dict]:
    """The home list's rows under its Sessions heading, top first."""
    nodes = device.nodes()
    heading = next((node for node in nodes if node["text"] == "Sessions"), None)
    if heading is None:
        return []
    width = max(node["bounds"][2] for node in nodes)
    rows = [
        node
        for node in nodes
        if node["clickable"]
        and node["bounds"][1] >= heading["bounds"][3]
        and node["bounds"][2] - node["bounds"][0] > width * 0.8
    ]
    return sorted(rows, key=lambda node: node["bounds"][1])


def long_press_drag(
    device: Device, x: int, y_from: int, y_to: int, shot: str, steps: int = 8
) -> None:
    """Hold past the long-press timeout, then move in steps and let go.

    The first move stays under the touch slop, as a finger's jitter does. The
    2026-10-27 run's first move was 16.5 px, the slop itself: the list's own
    scrolling, which sees a move before the drag does, took it and the drag
    was cancelled before any row moved. A small move the drag takes first
    cancels the scrolling instead.
    """
    nudge = 3 if y_to > y_from else -3
    device.shell(
        f"input motionevent DOWN {x} {y_from}; sleep 1; input motionevent MOVE {x} {y_from + nudge}"
    )
    device.screenshot(shot)  # the lifted row, mid-drag
    moves = "; ".join(
        f"input motionevent MOVE {x} {y_from + (y_to - y_from) * step // steps}"
        for step in range(1, steps + 1)
    )
    device.shell(f"{moves}; sleep 0.5; input motionevent UP {x} {y_to}")
    time.sleep(1)


def order_file(deck: Deck) -> list:
    path = deck.state / "order.json"
    return json.loads(path.read_text()) if path.exists() else []


def search_keyboard(device: Device, summary: dict) -> None:
    """Outside taps dismiss search; editing and clearing keep the keyboard."""
    keyboard = next((ime for ime in device.keyboards if "latin" in ime.lower()), None)
    if keyboard is None:
        raise CheckFailed(f"no typing keyboard in the emulator: {device.keyboards}")
    device.shell(f"pm enable {keyboard.split('/')[0]}")
    wait_for(
        "the typing keyboard to register",
        20,
        lambda: keyboard in device.shell("ime list -a -s").split(),
    )
    device.shell(f"ime enable {keyboard}")
    device.shell(f"ime set {keyboard}")
    device.shell("settings put secure show_ime_with_hard_keyboard 1")

    def keyboard_shown() -> bool:
        # The IME service keeps mIsInputViewShown=true after its window hides.
        # InputMethodManager's current mInputShown tracks the client's keyboard.
        state = device.shell("dumpsys input_method")
        shown = re.search(r"^\s*mInputShown=(true|false)\b", state, re.MULTILINE)
        if shown is None:
            raise CheckFailed("Android did not report its current keyboard state")
        return shown.group(1) == "true"

    def focus_search() -> dict:
        field = device.wait("the search box", timeout=10, desc="Search sessions")
        device.tap(field)
        wait_for("the search keyboard to open", 10, keyboard_shown)
        return field

    device.tap(device.wait("the menu button", desc="Menu"))
    focus_search()
    device.tap(device.wait("the drawer heading", text="Cepheus"))
    wait_for(
        "outside the empty drawer search to hide the keyboard", 10, lambda: not keyboard_shown()
    )
    summary["search_empty_outside_tap"] = True

    device.type_into(focus_search(), "AGY")
    device.tap(device.wait("the search box", desc="Search sessions"))
    if not keyboard_shown():
        raise CheckFailed("tapping inside search dismissed its keyboard")
    device.tap(device.wait("the drawer heading", text="Cepheus"))
    wait_for(
        "outside the typed drawer search to hide the keyboard", 10, lambda: not keyboard_shown()
    )
    if not device.find(text="AGY"):
        raise CheckFailed("tapping outside search cleared the query")
    summary["search_query_preserved"] = True

    focus_search()
    # Search holds the trailing icon while its request waits up to 45 seconds.
    device.tap(device.wait("clear search", timeout=60, desc="Clear the search"))
    if not keyboard_shown():
        raise CheckFailed("clearing search dismissed the keyboard")
    if device.find(text="AGY"):
        raise CheckFailed("the clear button did not clear the query")
    summary["search_clear_keeps_keyboard"] = True

    device.type_into(device.wait("the drawer search", desc="Search sessions"), "AGY")
    device.tap(device.wait("the Home button", text="Home"))
    wait_for("the Home tap to dismiss the keyboard", 10, lambda: not keyboard_shown())
    if device.find(text="Home"):
        raise CheckFailed("the Home tap did not close the drawer")
    field = focus_search()
    x1, _, _, y2 = field["bounds"]
    device.shell(f"input tap {x1 + 2} {y2 + 5}")
    wait_for("home whitespace to hide the keyboard", 10, lambda: not keyboard_shown())
    if not device.find(text="AGY"):
        raise CheckFailed("tapping home whitespace cleared the query")
    summary["search_home_outside_tap"] = True
    device.screenshot("search-keyboard-dismissed")


def settle(device: Device) -> None:
    """Steady screens for uiautomator: no animations, and the boot's own work done."""
    for scale in (
        "window_animation_scale",
        "transition_animation_scale",
        "animator_duration_scale",
    ):
        device.shell(f"settings put global {scale} 0")
    # No soft keyboard to cover fields or open its own panels: `input text`
    # reaches the focused field as key events (the emulator is disposable).
    device.keyboards = device.shell("ime list -s", check=False).split()
    for ime in device.keyboards:
        device.shell(f"ime disable {ime}", check=False)
    device.shell("pm disable-user --user 0 com.google.android.inputmethod.latin", check=False)
    device.shell("input keyevent 224")
    device.shell("wm dismiss-keyguard", check=False)
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        focus = device.shell("dumpsys window | grep mCurrentFocus", check=False)
        if "Application Not Responding" not in focus:
            break
        device.find(text="Wait")  # dismisses a stalled System UI dialog
        time.sleep(2)


def release_starts(device: Device, apk: Path, summary: dict) -> None:
    """The minified, signed build opens to its pairing screen."""
    package = PACKAGE.removesuffix(".debug")
    device.adb("install", "-r", "-g", str(apk), timeout=300)
    device.shell(f"am start -n {package}/{ACTIVITY}")
    device.wait("the release build's pairing screen", timeout=60, text="Pair")
    device.screenshot("00-release-pairing")
    summary["release_starts"] = True
    device.adb("uninstall", package, check=False)


def background_off(device: Device, deck: Deck, summary: dict) -> None:
    """Off by default: no service, and a closed app lets its connection go.

    Owner, 2026-10-27: "make it so the service running in the background is
    off by default for the app, and you can enable it in the app settings".
    """
    wait_for("the open app's connection", 20, lambda: gateway_connections(deck.port) == 1)
    if service_running(device):
        raise CheckFailed("the background service runs, though it is off by default")
    summary["service_off_by_default"] = True
    device.shell("input keyevent 3")  # home
    wait_for(
        "the closed app to let its connection go",
        PAUSE_SECONDS + 45,
        lambda: gateway_connections(deck.port) == 0,
    )
    if service_running(device):
        raise CheckFailed("the background service started while the app was closed")
    summary["closed_app_disconnects"] = True
    device.shell(f"am start -n {PACKAGE}/{ACTIVITY}")
    wait_for("the reopened app's connection", 30, lambda: gateway_connections(deck.port) == 1)
    device.wait("the session list again", timeout=20, text="Local agents")
    summary["reopened_app_connects"] = True


def stats(device: Device, summary: dict) -> None:
    """Settings' Stats: agent usage, and the desktop's CPU, memory and SSD."""
    open_screen(device, "Stats")
    device.wait("the agent usage card", timeout=20, text="Agent usage")
    for expected in (
        "5h, 98% remaining, Resets in",
        "wk, 63% remaining, Resets in",
        "wk, 100% remaining, Resets in",
    ):
        device.wait(f"remaining quota: {expected}", timeout=20, contains=expected)
    summary["remaining_quota"] = {"claude_5h": 98, "claude_weekly": 63, "codex_weekly": 100}
    device.wait("the SSD's use", timeout=20, contains=" used of ")
    deadline = time.monotonic() + 20
    cpu = ""
    while not re.search(r"CPU, \d+%", cpu):
        if time.monotonic() > deadline:
            raise CheckFailed(f"Stats showed no CPU use ({cpu!r})")
        time.sleep(1)
        cpu = next((node["desc"] for node in device.find(contains="CPU, ")), "")
    summary["stats"] = cpu
    device.screenshot("02b-stats")
    device.shell("input keyevent 4")  # back to Settings, which Stats opened from
    device.wait("Settings after Stats", timeout=10, contains="the desktop's CPU, memory and SSD")
    device.shell("input keyevent 4")  # back to the list
    device.wait("the session list after Settings", timeout=10, text="Local agents")


def background_on(device: Device, deck: Deck, summary: dict) -> None:
    """Settings' switch starts the service, and the connection outlasts a closed app."""
    open_screen(device, "Settings")
    # Background follows the Deck and Connection sections, below a small screen's fold.
    scroll_to(device, "the switch's off summary", contains="Connects only while the app is open")
    device.tap(device.wait("the background switch", contains="Stay connected in the background"))
    device.wait(
        "the switch's on summary", timeout=10, contains="Alerts arrive while the app is closed"
    )
    # The journey exempts the app from battery optimization, as the switch asks.
    scroll_to(device, "the battery summary", contains="Unrestricted")
    wait_for("the background service", 20, lambda: service_running(device))
    device.screenshot("02c-settings-background-on")
    device.shell("input keyevent 3")  # home
    deadline = time.monotonic() + PAUSE_SECONDS + 15
    while time.monotonic() < deadline:
        if gateway_connections(deck.port) != 1:
            raise CheckFailed("the background connection went while the app was closed")
        time.sleep(2)
    summary["background_connection_outlasts_closing"] = True
    device.shell(f"am start -n {PACKAGE}/{ACTIVITY}")
    device.wait("the settings again", timeout=20, contains="Stay connected in the background")
    device.shell("input keyevent 4")  # back to the list Settings opened over
    device.wait("the session list", timeout=20, text="Local agents")


def attach(device: Device, deck: Deck, session: int, summary: dict) -> None:
    """+ then Files: a file from Downloads reaches the session as its path on the deck.

    Owner, 2026-10-27: "in the apk, i need a way to upload files like i can in
    claude etc".
    """
    note = f"a note from the device check {secrets.token_hex(8)}\n".encode()
    local = deck.root / ATTACHMENT
    local.write_bytes(note)
    device.adb("push", str(local), f"/sdcard/Download/{ATTACHMENT}")
    device.shell(
        "am broadcast -a android.intent.action.MEDIA_SCANNER_SCAN_FILE "
        f"-d file:///sdcard/Download/{ATTACHMENT}",
        check=False,
    )
    device.shell(
        "content call --uri content://media --method scan_volume --arg external_primary",
        check=False,
    )
    wait_for(
        "the session to be ready",
        30,
        lambda: status_of(deck, session) not in ("starting", "working"),
    )
    device.tap(device.wait("the Attach button", desc="Attach"))
    device.tap(device.wait("the menu's Files", text="Files"))
    pick_file(device, ATTACHMENT)
    device.wait("the attachment's chip", timeout=20, desc=f"Remove {ATTACHMENT}")
    device.screenshot("05b-attached")
    message = "here is a file"
    device.type_into(device.fields()[-1], message)
    device.tap(wait_enabled(device, "Send"))

    # The message, a blank line, then the path, as uploads.rs with_files types them.
    def flat() -> str:
        return deck.screen(session).replace("\n", "")

    wait_for("the message on the stand-in's screen", 30, lambda: f"you said: {message}" in flat())
    # Echoed, then reported done: the path's turn is over.
    wait_for("the file's path on the stand-in's screen", 40, lambda: flat().count(ATTACHMENT) >= 2)
    stored = deck.state / "uploads" / str(session) / ATTACHMENT
    if not stored.is_file() or stored.read_bytes() != note:
        raise CheckFailed(f"the deck did not keep the file whole at {stored}")
    if stored.stat().st_mode & 0o777 != 0o600:
        raise CheckFailed(f"the stored file is {oct(stored.stat().st_mode & 0o777)}, not 0600")
    if not any(f"you said: {path}" in flat() for path in (stored, stored.resolve())):
        raise CheckFailed("the stand-in was not sent the stored file's path")
    if device.find(desc=f"Remove {ATTACHMENT}"):
        raise CheckFailed("the sent file is still attached in the composer")
    summary["attachment"] = str(stored.relative_to(deck.root))
    device.screenshot("05c-attachment-sent")


def native_unavailable(device: Device, what: str) -> None:
    """A live agent without a transcript shows native controls, never its PTY."""
    device.wait(what, timeout=60, desc="More")
    device.wait("the native unavailable conversation", timeout=60, contains="can't be read yet")
    if device.find(text="Terminal") or device.find(contains="stand-in agent ready"):
        raise CheckFailed("the unavailable conversation displayed its terminal")


def reorder(device: Device, deck: Deck, first: int, summary: dict) -> None:
    """A long-press drag puts a second session above the first, and the deck keeps it.

    Owner, 2026-10-27: "the sessions grouping should reflect the deck, they
    dont, also allowing me to hold drag and reorder them".
    """
    device.shell("input keyevent 4")  # back to the list
    device.tap(device.wait("New session", text="New session"))
    device.tap(device.wait("the Grok chip", text="Grok"))
    device.tap(device.wait("Start", text="Start"))
    wait_for("a second session", 30, lambda: len(deck.sessions()) == 2)
    second = max(s["id"] for s in deck.sessions())
    native_unavailable(device, "the second session's native conversation")
    if order_file(deck)[:1] == [second]:
        raise CheckFailed("the new session was first before any drag")
    device.shell("input keyevent 4")  # back to the list
    deadline = time.monotonic() + 20
    while len(rows := session_rows(device)) < 2:
        if time.monotonic() > deadline:
            raise CheckFailed(f"the list shows {len(rows)} session rows, not two")
        time.sleep(1)
    device.screenshot("10-before-reorder")
    top, below = rows[0]["bounds"], rows[1]["bounds"]
    x = (below[0] + below[2]) // 2
    long_press_drag(device, x, (below[1] + below[3]) // 2, (top[1] + top[3]) // 2, "10b-dragging")
    wait_for(
        "the deck's order file to put the second session first",
        15,
        lambda: order_file(deck)[:2] == [second, first],
    )
    summary["order_file"] = order_file(deck)
    device.screenshot("11-after-reorder")
    # The phone's list follows: its top row is now the second session.
    device.tap(session_rows(device)[0])
    native_unavailable(device, "the second conversation from the top row")
    checkpoint = "reordered session checkpoint"
    send_from_app(device, deck, second, checkpoint)
    wait_for("the checkpoint on the second PTY", 30, lambda: checkpoint in deck.screen(second))
    if checkpoint in deck.screen(first):
        raise CheckFailed("the reordered top row sent to the first session")
    summary["reordered"] = True


def drawer(device: Device, summary: dict) -> None:
    """Home returns to the list; Settings returns to the session it opened over.

    Owner, 2026-10-28: "make it so the agent desk android app has stats,
    settings etc are all on a proper page instead of the side bar taking up
    space where the sessions should be". Opened over a session, Settings goes
    back to it.
    """
    device.tap(device.wait("the menu button", desc="Menu"))
    device.wait("the drawer's sessions", timeout=10, text="Local agents")
    device.wait("the drawer's Settings button", desc="Settings")
    for gone in ("Stats", "Errors"):
        if device.find(text=gone):
            raise CheckFailed(f"the drawer still lists {gone}")
    device.screenshot("12-drawer")
    device.tap(device.wait("the drawer's Settings button", desc="Settings"))
    device.wait("Settings' Stats row", timeout=10, text="Stats")
    device.wait("Settings' Errors row", text="Errors")
    device.screenshot("13-settings")
    device.shell("input keyevent 4")  # back to the session Settings opened over
    device.wait("the session again after Settings", timeout=10, desc="More")
    device.tap(device.wait("the session menu", desc="Menu"))
    device.tap(device.wait("the drawer's Home entry", timeout=10, text="Home"))
    device.wait("the home screen after leaving the drawer", timeout=10, text="Local agents")
    device.tap(session_rows(device)[0])
    device.wait("the session reopened from Home", timeout=10, desc="More")
    summary["drawer_holds_sessions"] = True


def home_navigation(device: Device, deck: Deck, summary: dict) -> None:
    """A session's drawer returns to Home without ending the session."""
    device.tap(device.wait("New session", text="New session"))
    device.tap(device.wait("the Grok chip", text="Grok"))
    device.tap(device.wait("Start", text="Start"))
    native_unavailable(device, "the native session")
    session = deck.sessions()[0]["id"]
    # Repeat the owner's list -> session -> Home journey, including reopening.
    for attempt in range(3):
        device.tap(device.wait("the session menu", desc="Menu"))
        device.tap(device.wait("the drawer's Home entry", timeout=10, text="Home"))
        device.wait("the home screen", timeout=10, text="Local agents")
        device.wait("the home screen's New session button", desc="New session")
        wait_for(
            "Home with the drawer and session screen closed",
            10,
            lambda: not device.find(desc="Settings") and not device.find(desc="More"),
        )
        if not any(s["id"] == session and s["alive"] for s in deck.sessions()):
            raise CheckFailed("returning Home ended the session")
        device.screenshot(f"home-navigation-{attempt + 1}")
        device.tap(session_rows(device)[0])
        native_unavailable(device, "the reopened native session")
    drawer(device, summary)
    device.shell("input keyevent 4")
    device.wait("Home after Android Back", timeout=10, text="Local agents")
    summary["home_navigation"] = {"drawer_round_trips": 3, "android_back": True}


def command_picker(device: Device, deck: Deck, summary: dict) -> None:
    for tool in ("Claude", "Codex"):
        device.tap(device.wait("New session", text="New session"))
        device.tap(device.wait(f"the {tool} chip", text=tool))
        device.tap(device.wait("Start", text="Start"))
        device.wait("the message box", timeout=60, cls="android.widget.EditText")
        session = next(s["id"] for s in deck.sessions() if s["tool"] == tool.lower())
        field = device.fields()[-1]
        device.type_into(field, "/")
        device.wait("all commands and skills", contains="Skills and commands ·")
        device.screenshot(f"commands-{tool.lower()}-all")
        device.type_into(device.fields()[-1], "/mod")
        device.wait("a built-in command", text="/model")
        # A query reaches an entry well beyond the first screen of the 90-skill catalog.
        device.type_into(device.fields()[-1], "/phone-skill-89")
        prefix = "/" if tool == "Claude" else "$"
        name = prefix + "phone-skill-89"
        device.tap(device.wait("the last skill", text=name))
        if device.fields()[-1]["text"] != name + " ":
            raise CheckFailed(f"{tool} did not insert its native skill invocation")
        device.screenshot(f"commands-{tool.lower()}-skill-selected")
        # Selection stays a draft. Add arguments, then press Send and inspect the real PTY.
        if "you said:" in deck.screen(session):
            raise CheckFailed("picking a skill submitted the draft")
        message = name + " please verify"
        send_from_app(device, deck, session, message)
        wait_for(
            "skill and arguments at the program",
            20,
            lambda message=message, session=session: message in deck.screen(session),
        )
        device.type_into(device.fields()[-1], "/mod")
        device.tap(device.wait("model command", text="/model"))
        if device.fields()[-1]["text"] != "/model ":
            raise CheckFailed("the slash command did not stay in the draft")
        device.screenshot(f"commands-{tool.lower()}-model-selected")
        device.type_into(device.fields()[-1], "/not-a-command")
        device.wait("no matches", contains="No matching skills or commands")
        device.screenshot(f"commands-{tool.lower()}-empty")
        device.type_into(device.fields()[-1], "")
        device.tap(device.wait("Menu", desc="Menu"))
        device.tap(device.wait("Sessions", text="Local agents"))
        summary[f"{tool.lower()}_picker"] = True


def tool_details_while_working(device: Device, deck: Deck, summary: dict) -> None:
    """The command chevron opens input and result while the agent stays busy."""
    session = int(deck.ctl("new", "claude").strip())

    def ready() -> bool:
        screen = deck.screen(session)
        if status_of(deck, session) == "ended":
            raise CheckFailed(f"the stand-in exited before the card check: {screen}")
        return "stand-in agent ready" in screen

    wait_for("the stand-in agent", 30, ready)
    deck.ctl("send", str(session), r"busy tool details\r")
    wait_for("the busy turn", 30, lambda: status_of(deck, session) == "working")
    device.tap(session_rows(device)[0])
    device.shell("settings put global animator_duration_scale 1")
    arrow = device.wait("the tool details arrow", timeout=45, desc="Show details")
    device.screenshot("tool-busy-collapsed")
    device.tap(arrow)
    device.wait("the expanded tool input", timeout=15, contains="printf busy-detail-input")
    device.wait("the expanded tool result", timeout=15, text="busy-detail-result")
    if status_of(deck, session) != "working":
        raise CheckFailed("the turn finished before its tool details opened")
    device.screenshot("tool-busy-expanded")
    device.tap(device.wait("the collapse arrow", desc="Hide details"))
    device.wait("the collapsed arrow", desc="Show details")
    if device.find(text="busy-detail-result"):
        raise CheckFailed("the tool result stayed visible after collapse")
    summary["tool_details_while_working"] = True

    # A busy chat can gain a row between finger-down and finger-up. Keep the
    # real gesture active across an update, then require both the new message
    # and the opened result. Three cards exercise repeated updates, not one
    # lucky tap between batches.
    transcript = next((deck.claude_dir / "projects").glob("*/*.jsonl"))

    def record(kind: str, content: list[dict]) -> None:
        with transcript.open("a") as output:
            output.write(
                json.dumps({"type": kind, "message": {"role": kind, "content": content}}) + "\n"
            )

    failures = []
    for index in range(3):
        call = f"incoming-tool-{index}"
        command = f"printf incoming-detail-input-{index}"
        result = f"incoming-detail-result-{index}"
        record(
            "assistant",
            [{"type": "tool_use", "id": call, "name": "Bash", "input": {"command": command}}],
        )
        record("user", [{"type": "tool_result", "tool_use_id": call, "content": result}])
        target = device.wait("the next command", contains=command)
        arrows = [
            node
            for node in device.find(desc="Show details")
            if abs(sum(node["bounds"][1::2]) - sum(target["bounds"][1::2])) < 30
        ]
        if not arrows:
            raise CheckFailed(f"the command {index} has no visible arrow")
        press = target if index == 1 else arrows[0]
        x1, y1, x2, y2 = press["bounds"]
        x, y = (x1 + x2) // 2, (y1 + y2) // 2
        device.shell(f"input motionevent DOWN {x} {y}")
        try:
            record(
                "assistant",
                [
                    {
                        "type": "text",
                        "text": f"Incoming update {index}. " + "More command output arrived. " * 6,
                    }
                ],
            )
            time.sleep(2)
            device.screenshot(f"tool-update-{index}-pressed")
            # A real finger has small moves; a perfectly still synthetic press
            # can survive after the target has moved out from underneath it.
            device.shell(f"input motionevent MOVE {x + 1} {y}")
        finally:
            device.shell(f"input motionevent UP {x} {y}")
        device.wait(
            "the incoming output after release", timeout=15, contains=f"Incoming update {index}."
        )
        try:
            device.wait("details opened across the update", timeout=8, text=result)
        except CheckFailed:
            failures.append(index)
        device.screenshot(f"tool-update-{index}-released")
        for opened in device.find(desc="Hide details"):
            device.tap(opened)
        if status_of(deck, session) != "working":
            raise CheckFailed("the turn finished during the incoming-output check")
    summary["tool_updates_during_press_failures"] = failures
    device.shell("input swipe 360 450 360 650 350")
    target = device.wait("the same command after scrolling", contains=command)
    arrows = [
        node
        for node in device.find(desc="Show details")
        if abs(sum(node["bounds"][1::2]) - sum(target["bounds"][1::2])) < 30
    ]
    if not arrows:
        raise CheckFailed("no collapsed command arrow after scrolling")
    device.screenshot("tool-jitter-scrolled-before")
    device.tap(arrows[0])
    device.wait("the same result after scrolling", timeout=10, text=result)
    device.screenshot("tool-jitter-scrolled-expanded")
    summary["same_card_opens_after_scrolling"] = True
    summary["tool_press_targets"] = ["chevron", "header", "chevron"]

    device.tap(device.wait("the reopened card's collapse arrow", desc="Hide details"))
    device.tap(device.wait("return to the bottom", desc="Latest messages"))
    for attempt in range(3):
        device.shell("input motionevent DOWN 360 450")
        try:
            record(
                "assistant",
                [
                    {
                        "type": "text",
                        "text": f"Swipe update {attempt}. "
                        + "Reading the streamed command output. " * 6,
                    }
                ],
            )
            # Start moving before the text-selection long-press timeout.
            time.sleep(0.12)
            for y in range(480, 1001, 40):
                device.shell(f"input motionevent MOVE 360 {y}")
        finally:
            device.shell("input motionevent UP 360 1000")
        latest = device.wait(
            "the first swipe to leave the bottom", timeout=8, desc="Latest messages"
        )
        device.screenshot(f"tool-swipe-{attempt}")
        device.tap(latest)
        device.wait(
            "the update after returning to the bottom",
            timeout=10,
            contains=f"Swipe update {attempt}.",
        )
        if status_of(deck, session) != "working":
            raise CheckFailed("the turn finished during the swipe check")
    summary["first_swipes_during_updates"] = 3
    if failures:
        raise CheckFailed(f"tool taps failed while output arrived: {failures}")


def chat_before_log(device: Device, deck: Deck, summary: dict) -> None:
    """A message sent from the app shows at once with "Thinking…", before the log holds it.

    Owner, 2026-10-30: "when i send messages in the android agent desk, there
    is always delay before my message shows and there is no thinking message
    as there is in the claude/codex apps". The phone's chat is read from the
    program's log, which the stand-in hosted as Claude writes LOG_DELAY
    seconds late, as Claude Code does after its prompt hooks.
    """
    if not device.find(text="New session"):
        device.shell("input keyevent 4")  # back to the list
    before = {s["id"] for s in deck.sessions()}
    device.tap(device.wait("New session", text="New session"))
    device.tap(device.wait("the Claude chip", text="Claude"))
    device.tap(device.wait("Start", text="Start"))
    wait_for("a Claude session", 30, lambda: {s["id"] for s in deck.sessions()} - before)
    session = max({s["id"] for s in deck.sessions()} - before)
    device.wait("the Claude session's empty chat", timeout=60, contains="can't be read yet")
    transcripts = deck.claude_dir / "projects"

    def logged(text: str) -> bool:
        return any(text in path.read_text() for path in transcripts.glob("*/*.jsonl"))

    message = "show me at once"
    send_from_app(device, deck, session, message)
    sent = time.monotonic()
    wait_for("the sent message in the chat", LOG_DELAY, lambda: device.chat_messages(message))
    shown = time.monotonic() - sent
    # A chat that shows only what the log holds finds the bubble once the
    # transcript has it, so this is checked first.
    if logged(message):
        raise CheckFailed("the transcript held the message before the chat showed it")
    if not device.find(text="Thinking…"):
        raise CheckFailed("the chat shows no Thinking… row after the message went")
    summary["sent_shown_before_log_seconds"] = round(shown, 1)
    device.screenshot("12-sent-before-log")

    wait_for("the message in the transcript", LOG_DELAY + 20, lambda: logged(message))
    device.wait("the answer in the chat", timeout=30, contains=f"done: {message}")
    copies = len(device.chat_messages(message))
    if copies != 1:
        raise CheckFailed(f"the chat shows the sent message {copies} times once the log has it")
    wait_for("the end of the turn", 30, lambda: status_of(deck, session) != "working")
    wait_for("Thinking… to leave the chat", 15, lambda: not device.find(text="Thinking…"))
    summary["sent_once_after_log"] = True
    summary["thinking_cleared"] = True
    device.screenshot("13-answered-from-log")


def send_now(device: Device, deck: Deck, summary: dict) -> None:
    """A message queued behind a Claude turn goes in at once from its Send now.

    Owner, 2026-11-03: "the app from the staging branch doesnt let me
    interrupt to send messages now like claude code works". The stand-in
    hosted as Claude works on "work long" until Claude Code's send-now keys
    reach it, then takes the message typed before them.
    """
    before = {s["id"] for s in deck.sessions()}
    device.tap(device.wait("New session", text="New session"))
    device.tap(device.wait("the Claude chip", text="Claude"))
    device.tap(device.wait("Start", text="Start"))
    wait_for("a Claude session", 30, lambda: {s["id"] for s in deck.sessions()} - before)
    session = max({s["id"] for s in deck.sessions()} - before)
    device.wait("the Claude session's empty chat", timeout=60, contains="can't be read yet")
    send_from_app(device, deck, session, "please work long")
    wait_for("the long turn", 20, lambda: "thinking (esc to interrupt)" in deck.screen(session))
    wait_for("the session to work", 20, lambda: status_of(deck, session) == "working")

    message = "steer it now"
    device.type_into(device.fields()[-1], message)
    device.tap(device.wait("Send", desc="Send"))
    wait_for("the queued message", 20, lambda: device.chat_messages(message))
    device.wait("its Queued caption", timeout=20, text="Queued")
    chip = device.wait("Send now on the queued message", timeout=20, text="Send now")
    device.screenshot("16-queued-send-now")
    if "sent now:" in deck.screen(session) or status_of(deck, session) != "working":
        raise CheckFailed("the turn took the message before Send now was tapped")
    tapped = time.monotonic()
    device.tap(chip)
    wait_for(
        "the turn to take the message now",
        20,
        lambda: f"sent now: {message}" in deck.screen(session),
    )
    summary["send_now_taken_seconds"] = round(time.monotonic() - tapped, 1)
    device.wait("the answer in the chat", timeout=30, contains=f"done: {message}")
    copies = len(device.chat_messages(message))
    if copies != 1:
        raise CheckFailed(f"the chat shows the message sent now {copies} times once the log has it")
    if device.find(text="Send now") or device.find(text="Queued"):
        raise CheckFailed("the message sent now still offers Send now or says Queued")
    summary["send_now"] = True
    device.screenshot("17-sent-now")


def activity_state(device: Device, deck: Deck, summary: dict) -> None:
    """Unlinked sessions stay native; a quiet unlogged command is not thinking."""
    before = {s["id"] for s in deck.sessions()}
    device.tap(device.wait("New session", text="New session"))
    device.tap(device.wait("the Codex chip", text="Codex"))
    device.tap(device.wait("Start", text="Start"))
    wait_for("an unlinked Codex", 30, lambda: {s["id"] for s in deck.sessions()} - before)
    session = max({s["id"] for s in deck.sessions()} - before)
    device.wait("the unlinked Codex native state", timeout=60, contains="can't be read yet")
    if next(s for s in deck.sessions() if s["id"] == session).get("conversation"):
        raise CheckFailed("the Codex fixture unexpectedly linked a conversation")
    message = "show live progress"
    send_from_app(device, deck, session, message)
    wait_for("the unlinked session's native sent bubble", 20, lambda: device.chat_messages(message))
    device.wait("the unlinked session's thinking row", timeout=20, contains="Thinking")
    if device.find(text="Terminal") or device.find(contains=f"you said: {message}"):
        raise CheckFailed(
            "the unlinked session displayed its terminal while the sent message awaits a transcript"
        )
    device.screenshot("14-unlinked-codex-working")
    # Expiring the unlogged outgoing bubble must never reveal a terminal.
    wait_for("the unlogged message to settle", 60, lambda: not device.chat_messages(message))
    device.wait("the native unavailable state", contains="can't be read yet")
    if device.find(text="Terminal") or device.find(contains=f"done: {message}"):
        raise CheckFailed("the terminal appeared after the outgoing bubble settled")
    summary["unlinked_codex_native_unavailable"] = True

    device.shell("input keyevent 4")
    chat_before_log(device, deck, summary)
    session = max(s["id"] for s in deck.sessions())
    send_from_app(device, deck, session, "/help")
    wait_for(
        "the accepted idle command", 10, lambda: "help: choose a command" in deck.screen(session)
    )
    wait_for(
        "the session to settle idle",
        10,
        lambda: status_of(deck, session) not in ("working", "starting"),
    )
    # The old row stayed up for the entire 20-second outbox settling period.
    wait_for(
        "no Thinking while quiet with an unlogged command",
        5,
        lambda: not device.find(text="Thinking…"),
    )
    summary["idle_unlogged_command_not_thinking"] = True
    device.screenshot("15-idle-unlogged-command")


def resume(device: Device, deck: Deck, grok_id: str, summary: dict) -> None:
    """Resume an earlier conversation from the phone instead of starting one.

    Owner, 2026-10-31: "resume a existing session either claude/codex/agy etc
    where you pick from a list of sessions", then "and the android app needs
    that implemented as well". The deck holds no session; the list is read
    from the seeded Claude transcript and Grok prompt history.
    """
    device.tap(device.wait("the home screen's Resume button", desc="Resume a conversation"))
    device.wait("the Resume sheet", timeout=15, text="Resume a conversation")
    device.wait("the Claude conversation", timeout=45, text="Tidy the parser")
    device.screenshot("resume-list")
    # Rows below the first can be off screen; select Grok through the filter.
    device.type_into(device.fields()[-1], "grok")
    device.hide_keyboard()
    wait_for("the filter to leave only Grok", 10, lambda: not device.find(text="Tidy the parser"))
    device.wait("the Grok conversation after the filter", text="Fix the build")
    device.screenshot("resume-filtered")
    device.tap(device.wait("the Grok conversation", text="Fix the build"))
    wait_for("the sheet to close", 30, lambda: not device.find(text="Resume a conversation"))
    wait_for("the resumed session", 30, lambda: deck.sessions())
    sessions = deck.sessions()
    if [(s["tool"], s.get("conversation")) for s in sessions] != [("grok", grok_id)]:
        raise CheckFailed(f"expected one Grok session resuming {grok_id}, found {sessions}")
    native_unavailable(device, "the resumed native conversation")
    summary["resumed_from_home"] = sessions[0]["id"]
    device.screenshot("resume-opened")

    # Again from New session: the list marks it open and shows that session.
    device.tap(device.wait("Menu", desc="Menu"))
    device.tap(device.wait("Sessions", text="Local agents"))
    device.tap(device.wait("New session", text="New session"))
    device.tap(device.wait("Resume earlier…", text="Resume earlier…"))
    device.wait("the Resume sheet again", text="Resume a conversation")
    device.type_into(device.wait("the Resume filter", cls="android.widget.EditText"), "grok")
    device.hide_keyboard()
    device.wait("the conversation marked open", timeout=45, contains="open in the deck")
    device.screenshot("resume-open-in-deck")
    device.tap(device.wait("the Grok conversation", text="Fix the build"))
    wait_for("the sheet to close again", 30, lambda: not device.find(text="Resume a conversation"))
    native_unavailable(device, "the same resumed native conversation")
    again = deck.sessions()
    if [s["id"] for s in again] != [sessions[0]["id"]]:
        raise CheckFailed(f"resuming an open conversation started another session: {again}")
    summary["resumed_again_shows_same_session"] = True


def search(device: Device, deck: Deck, summary: dict) -> None:
    """Type a word of an instruction and find the session it was given to.

    Owner, 2026-11-02: "i need to be able to search conversations, just like
    to type in "agy" and find which one i gave a particular instruction to
    about the agent deck agy agents. the sessions need search". Two Claude
    sessions keep transcripts; only the second one's second message mentions
    agy, so no title holds the word and only the deck's search of the
    conversations can find it.
    """
    instruction = "make the agent deck start agy agents"
    transcripts = deck.claude_dir / "projects"

    def logged(text: str) -> bool:
        return any(text in path.read_text() for path in transcripts.glob("*/*.jsonl"))

    def start_claude() -> int:
        # Back only from a session's chat: on the list it closes the app.
        if device.find(desc="More"):
            device.shell("input keyevent 4")
        before = {s["id"] for s in deck.sessions()}
        device.tap(device.wait("New session", timeout=30, text="New session"))
        device.tap(device.wait("the Claude chip", text="Claude"))
        device.tap(device.wait("Start", text="Start"))
        wait_for("a Claude session", 30, lambda: {s["id"] for s in deck.sessions()} - before)
        device.wait("the Claude session's empty chat", timeout=60, contains="can't be read yet")
        return max({s["id"] for s in deck.sessions()} - before)

    parser = start_claude()
    send_from_app(device, deck, parser, "tidy the parser")
    builder = start_claude()
    send_from_app(device, deck, builder, "check the build")
    send_from_app(device, deck, builder, instruction)
    wait_for(
        "both sessions' messages in their transcripts",
        LOG_DELAY + 30,
        lambda: logged("tidy the parser") and logged(instruction),
    )

    # Search from the other session, so the result has somewhere to take the phone.
    device.shell("input keyevent 4")
    device.wait("the session list", timeout=10, text="Local agents")
    device.tap(session_rows(device)[0])
    device.wait("the first session's chat", timeout=20, desc="More")
    device.wait("the first session's message", timeout=20, text="tidy the parser")
    device.tap(device.wait("the menu button", desc="Menu"))
    device.type_into(
        device.wait("the drawer's search box", timeout=10, desc="Search sessions"), "AGY"
    )
    device.hide_keyboard()
    row = device.wait(
        "the session given the instruction", timeout=30, contains=f"You: {instruction}"
    )
    device.wait("the count of sessions found", timeout=10, text="1 session")
    # Only a search result describes itself as "Session N, …".
    found = [node["desc"] for node in device.nodes() if node["desc"].startswith("Session ")]
    if len(found) != 1:
        raise CheckFailed(f"the search should find only the session told about agy: {found}")
    summary["search_found"] = found[0]
    device.screenshot("search-drawer")
    device.tap(row)
    wait_for("the drawer to close", 10, lambda: not device.find(desc="Search sessions"))
    device.wait("the found session's chat", timeout=20, contains=f"done: {instruction}")
    if device.find(text="tidy the parser"):
        raise CheckFailed("the result did not open the session given the instruction")
    summary["search_opened_session"] = builder
    device.screenshot("search-opened")

    # The home list holds the same search, and Back clears it there.
    device.shell("input keyevent 4")
    device.wait("the search on the home list", timeout=10, contains=f"You: {instruction}")
    device.screenshot("search-home")
    device.shell("input keyevent 4")
    wait_for(
        "Back to clear the search", 10, lambda: not device.find(contains=f"You: {instruction}")
    )
    # The list's "Sessions" heading shows only while no search is typed.
    device.wait("the whole list again", timeout=10, text="Local agents")
    summary["search_back_clears"] = True


def system_bars(device: Device, deck: Deck, summary: dict) -> None:
    """Sheet windows keep the app's bar colors, even with the opposite OS theme."""
    from PIL import Image

    device.shell(
        "cmd overlay enable-exclusive --category com.android.internal.systemui.navbar.threebutton"
    )
    device.tap(device.wait("the new-session button", desc="New session"))
    device.wait("New session", text="Agent")
    device.tap(device.wait("the Claude program", text="Claude"))
    device.tap(device.wait("Start", text="Start"))
    device.wait("the chat", timeout=40, desc="More")
    failures = []
    checked = []

    def check(name: str, dark: bool) -> None:
        dump = device.shell("dumpsys window")
        (device.shots / f"{name}-window.txt").write_text(dump)
        focus = re.search(r"mCurrentFocus=(Window\{[^}]+\})", dump)
        if not focus:
            raise CheckFailed(f"{name}: no focused window")
        blocks = re.split(r"\n  Window #\d+ ", dump)
        window = next((b for b in blocks if b.startswith(focus[1] + ":")), "")
        # The attributes include nested sim={...}; read through the next field.
        attrs = re.search(r"mAttrs=(\{.*?)\n\s+Requested ", window, re.S)
        if not attrs:
            raise CheckFailed(f"{name}: no focused window attributes")
        appearance = {bar: f"LIGHT_{bar}_BARS" in attrs[1] for bar in ("STATUS", "NAVIGATION")}
        if any(light != (not dark) for light in appearance.values()):
            failures.append(f"{name}: light-bar appearance {appearance}, dark app={dark}")
        with Image.open(device.screenshot(name)) as shot:
            r, g, b = shot.convert("RGB").getpixel((shot.width // 10, shot.height - 3))
        light_background = (r + g + b) / 3 > 128
        if light_background == dark:
            failures.append(f"{name}: navigation background {(r, g, b)}, dark app={dark}")
        checked.append(
            {"screen": name, "dark": dark, "appearance": appearance, "navigation_rgb": [r, g, b]}
        )

    for theme, system, dark in (
        ("Dark", "no", True),
        ("Light", "yes", False),
        ("System default", "yes", True),
        ("System default", "no", False),
    ):
        device.shell(f"cmd uimode night {system}")
        open_screen(device, "Settings")
        device.tap(scroll_to(device, "the Theme row", contains="Theme"))
        device.tap(device.wait("the app theme", text=theme))
        device.shell("input keyevent 4")
        device.wait("the chat after Settings", desc="More")
        prefix = f"bars-{theme.lower().replace(' ', '-')}-system-{system}"
        check(prefix + "-chat", dark)
        device.tap(device.wait("the new-session button", desc="Start a new session"))
        device.wait("New session", text="Agent")
        check(prefix + "-new", dark)
        device.tap(device.wait("Resume earlier", text="Resume earlier…"))
        device.wait("Resume", text="Resume a conversation")
        check(prefix + "-resume", dark)
        device.shell("input keyevent 4")
        device.wait("the chat after Resume", desc="More")
        device.tap(device.wait("the model picker", contains="Model: "))
        device.wait("the model sheet", text="Model")
        check(prefix + "-model", dark)
        device.shell("input keyevent 4")
        device.wait("the chat after the model picker", desc="More")
        check(prefix + "-dismissed", dark)
    summary["system_bars"] = checked
    if failures:
        raise CheckFailed("System bars changed: " + "; ".join(failures))


def journey(
    device: Device, deck: Deck, apk: Path, summary: dict, *, stats_only: bool = False
) -> None:
    settle(device)
    release = os.environ.get("CEPHEUS_RELEASE_APK")
    if release:
        release_starts(device, Path(release), summary)
    device.adb("install", "-r", "-g", str(apk), timeout=300)
    # Each journey owns a new gateway. A previous pairing points at its old port.
    if device.shell(f"pm clear {PACKAGE}").strip() != "Success":
        raise CheckFailed("could not reset the isolated Deck app before pairing")
    # Clearing data also revokes the install's -g notification grant (Android 13+).
    device.shell(f"pm grant {PACKAGE} android.permission.POST_NOTIFICATIONS", check=False)
    # The owner's setup: exempt from battery optimization (the app asks for it).
    device.shell(f"dumpsys deviceidle whitelist +{PACKAGE}")
    device.shell(f"am start -n {PACKAGE}/{ACTIVITY}")

    # Pair by typing the four details, the code in lower case with a dash.
    code = deck.open_pairing()
    device.wait("the pairing form", contains="Pair this phone")
    device.hide_keyboard()
    device.wait("the pairing screen", text="Pair")
    values = ["10.0.2.2", str(deck.port), deck.fingerprint(), f"{code[:4].lower()}-{code[4:]}"]
    for index, value in enumerate(values):
        device.type_into(device.fields()[index], value)
    typed = [field["text"] for field in device.fields()[:4]]
    if typed != values:
        raise CheckFailed(f"the pairing fields hold {typed}, not {values}")
    # settle() disables IMEs for this focused journey; a stale input-method
    # flag can otherwise send Back and leave the pairing screen.
    if os.environ.get("CEPHEUS_ONLY") != "tool-details":
        device.hide_keyboard()
    device.screenshot("01-pairing-filled")
    # A keyboard (voice typing, once Gboard is off) can cover the button:
    # scroll the form up until it shows.
    for _ in range(4):
        if device.find(text="Pair"):
            break
        device.shell("input swipe 360 700 360 250 300")
        time.sleep(0.8)
    device.tap(device.wait("the Pair button", text="Pair"))
    device.wait("the session list", timeout=40, text="Local agents")
    summary["paired"] = len(deck.devices()) == 1
    device.screenshot("02-paired-list")
    if os.environ.get("CEPHEUS_ONLY") == "system-bars":
        system_bars(device, deck, summary)
        return
    if stats_only:
        stats(device, summary)
        return
    if os.environ.get("CEPHEUS_COMMANDS_ONLY") == "1":
        command_picker(device, deck, summary)
        return
    if os.environ.get("CEPHEUS_ONLY") == "search-keyboard":
        search_keyboard(device, summary)
        return
    if os.environ.get("CEPHEUS_ONLY") == "tool-details":
        tool_details_while_working(device, deck, summary)
        return
    if os.environ.get("CEPHEUS_ONLY") == "chat":
        chat_before_log(device, deck, summary)
        return
    if os.environ.get("CEPHEUS_ONLY") == "activity":
        activity_state(device, deck, summary)
        return
    if os.environ.get("CEPHEUS_ONLY") == "resume":
        resume(device, deck, deck.grok_id, summary)
        return
    if os.environ.get("CEPHEUS_ONLY") == "navigation":
        home_navigation(device, deck, summary)
        return
    if os.environ.get("CEPHEUS_ONLY") == "search":
        search(device, deck, summary)
        return
    if os.environ.get("CEPHEUS_ONLY") == "send_now":
        send_now(device, deck, summary)
        return
    background_off(device, deck, summary)
    stats(device, summary)
    background_on(device, deck, summary)

    # Start a session: Grok's unavailable transcript must keep a native chat.
    device.tap(device.wait("New session", text="New session"))
    device.tap(device.wait("the Grok chip", text="Grok"))
    device.screenshot("03-new-session-sheet")
    device.tap(device.wait("Start", text="Start"))
    native_unavailable(device, "the stand-in native conversation")
    sessions = deck.sessions()
    if len(sessions) != 1:
        raise CheckFailed(f"expected one session on the deck, found {len(sessions)}")
    session = sessions[0]["id"]
    wait_for(
        "the stand-in PTY to be ready", 30, lambda: "stand-in agent ready" in deck.screen(session)
    )
    summary["session"] = session
    device.screenshot("04-session")

    # A message from the app reaches the program's screen, and the app shows it.
    message = "hello from the phone"
    send_from_app(device, deck, session, message)
    wait_for(
        "the message on the stand-in's screen",
        30,
        lambda: f"you said: {message}" in deck.screen(session),
    )
    wait_for("the native outgoing bubble", 20, lambda: device.chat_messages(message))
    summary["sent"] = True
    device.screenshot("05-sent")

    # Its turn ends: a "finished" notification.
    wait_for("the end of the turn", 30, lambda: f"done: {message}" in deck.screen(session))
    wait_for(
        "the finished notification",
        30,
        lambda: has_notification(device.notifications(), "finished"),
    )
    summary["finished_notification"] = True
    attach(device, deck, session, summary)

    # A question, answered from its notification.
    device.hide_keyboard()
    send_from_app(device, deck, session, "please ask me")
    wait_for(
        "the needs-you notification",
        40,
        lambda: has_notification(device.notifications(), "needs_you"),
    )
    device.hide_keyboard()
    device.shell("input keyevent 3")  # home
    # The launcher's return animation collapsed a shade opened at once (the
    # 2026-10-27 run's screenshot showed the home screen), so let it settle,
    # and pull the shade down by hand if the command did not hold.
    time.sleep(2)
    button: list[dict] = []
    for attempt in range(4):
        if attempt % 2:
            device.shell("input swipe 360 5 360 1300 300")
        else:
            device.shell("cmd statusbar expand-notifications")
        time.sleep(2)
        button = device.find(text="No") or device.find(text="NO")
        if button:
            break
        expand = device.find(desc="Expand") or device.find(contains="Do you want to proceed?")
        if expand:
            device.tap(expand[0])
            time.sleep(1)
            button = device.find(text="No") or device.find(text="NO")
            if button:
                break
    device.screenshot("06-question-notification")
    if not button:
        raise CheckFailed("the notification has no No button")
    device.tap(button[0])
    wait_for(
        "the answer on the stand-in's screen", 30, lambda: "answered: No" in deck.screen(session)
    )
    summary["answered_from_notification"] = "No"
    wait_for(
        "the needs-you notification to clear",
        15,
        lambda: not has_notification(device.notifications(), "needs_you"),
    )
    summary["needs_you_cleared"] = True
    device.screenshot("07-answered")
    device.shell("cmd statusbar collapse")

    # Doze: with the screen off and the device forced idle, a new question
    # from the deck still raises its notification.
    device.shell("dumpsys battery unplug")
    device.shell("input keyevent 223")  # screen off
    device.shell("dumpsys deviceidle force-idle", check=False)
    summary["doze_state"] = device.shell("dumpsys deviceidle get deep", check=False).strip()
    deck.ctl("send", str(session), "doze ask\\r")
    arrived = True
    try:
        wait_for(
            "a notification during Doze",
            60,
            lambda: has_notification(device.notifications(), "needs_you"),
        )
    except CheckFailed:
        arrived = False
    summary["alert_during_doze"] = arrived
    device.shell("dumpsys deviceidle unforce")
    device.shell("dumpsys battery reset")
    device.shell("input keyevent 224")  # screen on
    device.shell("wm dismiss-keyguard", check=False)
    if not arrived:
        wait_for(
            "the Doze notification after Doze",
            60,
            lambda: has_notification(device.notifications(), "needs_you"),
        )
        summary["alert_after_doze"] = True
    # As a tap on the notification does: open the app at that session.
    device.shell(f"am start -n {PACKAGE}/{ACTIVITY} --el session {session}")
    time.sleep(3)
    try:
        wait_for("the app to be connected", 30, lambda: not offline(device))
        summary["connected_after_doze"] = True
    except CheckFailed:
        summary["connected_after_doze"] = False
    device.screenshot("08-after-doze")
    # The app still drives the deck: answer the Doze question from the app.
    device.tap(device.wait("the question's Yes option", timeout=20, text="Yes"))
    wait_for(
        "the in-app answer on the stand-in's screen",
        30,
        lambda: "answered: Yes" in deck.screen(session),
    )
    summary["answered_in_app_after_doze"] = "Yes"
    wait_for(
        "the answered question card to close",
        15,
        lambda: not device.find(contains="Do you want to proceed?"),
    )
    device.wait("the native chat controls after answering", desc="More")
    if device.find(contains="answered: Yes"):
        raise CheckFailed("the answer exposed the raw terminal")
    summary["native_chat_after_answer"] = True
    device.screenshot("09-answered-in-app")
    if not summary["connected_after_doze"]:
        raise CheckFailed("the app showed it was offline after Doze")
    several_choices(device, deck, session, summary)
    reorder(device, deck, session, summary)
    drawer(device, summary)
    chat_before_log(device, deck, summary)


def several_choices(device: Device, deck: Deck, session: int, summary: dict) -> None:
    """A several-choice question: a box ticked on its card, text from the message box.

    The phone could not answer one at all, and a message sent instead failed
    with "skipped #28 (waiting on a question)" (owner, 2026-10-30). The deck
    ticks Olives, pastes the text on "Type something", presses Submit and
    then confirms the review of the answers.
    """
    send_from_app(device, deck, session, "please pick several")
    device.tap(device.wait("the Olives box", timeout=30, text="Olives"))
    device.screenshot("10-several-choices")
    device.type_into(device.fields()[-1], "mango")
    device.tap(device.wait("the message box's answer button", timeout=10, desc="Send answer"))
    wait_for(
        "the boxes on the stand-in's screen",
        40,
        lambda: "picked: Olives, mango" in deck.screen(session),
    )
    wait_for(
        "the answered several-choice card to close",
        15,
        lambda: not device.find(text="Pick toppings?"),
    )
    device.wait("the native chat controls after several choices", desc="More")
    if device.find(contains="picked: Olives, mango"):
        raise CheckFailed("the several-choice answer exposed the raw terminal")
    summary["several_choices"] = "Olives, mango"
    device.screenshot("10-several-answered")
    # Typing left the message box focused under the keyboard, which took the
    # reorder step's Back for itself in both 2026-10-30 runs: let go of it
    # here, staying in the chat.
    focused = re.compile(r'class="android\.widget\.EditText"[^>]*focused="true"')
    for _ in range(3):
        device.nodes()
        if not focused.search(device.last_dump):
            break
        device.shell("input keyevent 4")
        time.sleep(1)
    if device.find(text="New session"):  # a Back left the chat: open it again
        device.tap(session_rows(device)[0])
    device.wait("the chat, still open", timeout=15, desc="More")
    device.wait("the native composer, still open", desc="Dictate")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--stats-only", action="store_true", help="check Stats without the other phone journeys"
    )
    args = parser.parse_args()
    serial = os.environ.get("ANDROID_SERIAL")
    binary = Path(os.environ.get("CEPHEUS_BIN", ""))
    apk = Path(os.environ.get("CEPHEUS_APK", ""))
    if not serial or not binary.is_file() or not apk.is_file():
        print("set ANDROID_SERIAL, CEPHEUS_BIN and CEPHEUS_APK", file=sys.stderr)
        return 2
    guard()
    root = Path(tempfile.mkdtemp(prefix="cepheus-check-", dir=storage_root()))
    shots = Path(os.environ.get("CEPHEUS_SHOTS") or root / "shots")
    shots.mkdir(parents=True, exist_ok=True)
    deck = Deck(binary, root)
    device = Device(serial, shots)
    summary: dict = {
        "serial": serial,
        "api_level": device.shell("getprop ro.build.version.sdk").strip(),
    }
    ok = False
    try:
        deck.seed_usage()
        if os.environ.get("CEPHEUS_ONLY") == "resume":
            deck.grok_id = deck.seed_conversations()
        deck.start()
        journey(device, deck, apk, summary, stats_only=args.stats_only)
        ok = True
    except (CheckFailed, subprocess.SubprocessError, OSError) as error:
        summary["error"] = str(error)
        log(f"FAILED: {error}")
        try:
            device.screenshot("failure")
            # The screenshots go with a scratch run's directory; the log keeps what the screen said.
            seen = [
                f"{node['class'].rsplit('.', 1)[-1]}:{node['text'] or node['desc']}"
                for node in device.nodes()
                if node["text"] or node["desc"]
            ]
            log("screen at the failure: " + " | ".join(seen))
        except (CheckFailed, subprocess.SubprocessError, OSError):
            pass
        for name in ("service.log", "state/remote.log"):
            path = root / name
            if path.exists():
                tail = path.read_text(errors="replace").splitlines()[-20:]
                log(f"{name} (last lines):\n" + "\n".join(tail))
    finally:
        deck.stop()
        summary["ok"] = ok
        (shots / "summary.json").write_text(json.dumps(summary, indent=2))
        log(json.dumps(summary))
        if os.environ.get("CEPHEUS_SHOTS"):
            shutil.rmtree(root, ignore_errors=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
