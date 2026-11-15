"""Private raw-PTY fake. No model process or installed service is invoked."""

from __future__ import annotations

import fcntl
import json
import os
import select
import struct
import subprocess
import sys
import termios
import time
import tty
import uuid
from datetime import datetime
from datetime import timezone
from pathlib import Path

ROOT, HOST, PROVIDER, DECK = Path(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
SESSION = os.environ["CEPHEUS_SESSION"]
KEY = f"{HOST}-{SESSION}"
FD = sys.stdin.fileno()
POLICY = json.loads((ROOT / "fixture-policy.json").read_text())
CONTROL = ROOT / "controls" / f"{KEY}.json"
CAPTURE = ROOT / "captures" / f"{KEY}.jsonl"
EVENTS = ROOT / "events" / f"{KEY}.jsonl"
state = {"mode": "idle", "show_draft": True}
draft = ""
cursor_home = False
pending = b""
sequence = None
CONVERSATION = (
    sys.argv[sys.argv.index("--session-id") + 1]
    if "--session-id" in sys.argv
    else sys.argv[sys.argv.index("--resume") + 1]
    if "--resume" in sys.argv
    else sys.argv[sys.argv.index("resume") + 1]
    if "resume" in sys.argv
    else str(uuid.uuid4())
)
STAMP = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
if PROVIDER == "codex":
    transcript_path = (
        Path(os.environ["CODEX_HOME"])
        / "sessions"
        / datetime.now(timezone.utc).strftime("%Y/%m/%d")
        / f"rollout-{CONVERSATION}.jsonl"
    )
else:
    slug = "".join(c if c.isascii() and c.isalnum() else "-" for c in os.getcwd())
    transcript_path = (
        Path(os.environ["CLAUDE_CONFIG_DIR"]) / "projects" / slug / f"{CONVERSATION}.jsonl"
    )
transcript_path.parent.mkdir(parents=True, exist_ok=True)
transcript = transcript_path.open("a", buffering=1)


def record(value: dict) -> None:
    value.setdefault("timestamp", datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"))
    transcript.write(json.dumps(value) + "\n")


if PROVIDER == "codex":
    record(
        {
            "type": "session_meta",
            "payload": {
                "id": CONVERSATION,
                "cwd": os.getcwd(),
                "timestamp": STAMP,
                "thread_source": "user",
            },
        }
    )
    record({"type": "event_msg", "payload": {"type": "user_message", "message": os.getcwd()}})
else:
    record(
        {"type": "user", "cwd": os.getcwd(), "message": {"role": "user", "content": os.getcwd()}}
    )
    sessions = Path(os.environ["CLAUDE_CONFIG_DIR"]) / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    (sessions / f"{os.getpid()}.json").write_text(
        json.dumps(
            {
                "pid": os.getpid(),
                "sessionId": CONVERSATION,
                "cwd": os.getcwd(),
                "kind": "interactive",
            }
        )
    )


def event(category: str, **counts: int) -> None:
    with EVENTS.open("a") as out:
        out.write(json.dumps({"category": category, "at_ns": time.monotonic_ns(), **counts}) + "\n")


def helper(child: subprocess.Popen) -> None:
    """Record a helper's birth identity, so cleanup waits for it to end."""
    try:
        stat = Path(f"/proc/{child.pid}/stat").read_text()
    except FileNotFoundError:
        return
    birth = int(stat.rsplit(")", 1)[1].split()[19])
    identity = {
        "pid": child.pid,
        "start": birth,
        "uid": os.getuid(),
        "boot": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
    }
    (ROOT / "actors" / f"helper-{KEY}-{child.pid}-{birth}.json").write_text(json.dumps(identity))


def request(args: list[str], payload: dict) -> subprocess.CompletedProcess:
    child = subprocess.Popen(
        args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )
    helper(child)
    try:
        stdout, stderr = child.communicate(
            json.dumps(payload), timeout=POLICY["hook_timeout_seconds"]
        )
    except subprocess.TimeoutExpired:
        child.kill()
        child.communicate()
        raise
    return subprocess.CompletedProcess(args, child.returncode, stdout, stderr)


def hook() -> None:
    payload = (
        {"hook_event_name": "Stop", "session_id": CONVERSATION}
        if PROVIDER == "claude"
        else {"type": "agent-turn-complete", "thread-id": CONVERSATION}
    )
    args = [DECK, "hook", PROVIDER]
    if PROVIDER == "codex":
        args.append(json.dumps(payload))
    result = request(args, payload)
    event("status_hook", exit_code=result.returncode)
    if result.returncode:
        raise RuntimeError("private fake status hook failed")


# Owner messages reach Claude as they reach the real one through the deck's
# `cepheus-owner-input` plugin (`plugins/providers/claude-owner-input`): a
# listener the session runs hands over each message, Claude submits it
# through its own prompt interface, beside whatever draft its composer
# holds, and a receipt reports what Claude did with it.
OWNER_INPUT = os.environ.get("CEPHEUS_OWNER_INPUT") if PROVIDER == "claude" else None


def owner_input_listener() -> subprocess.Popen | None:
    if not OWNER_INPUT:
        return None
    with (ROOT / "events" / f"{KEY}.owner-input.log").open("ab") as log:
        child = subprocess.Popen(
            [OWNER_INPUT, "hook", "owner-input", "listen", CONVERSATION],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=log,
        )
    helper(child)
    return child


def receipt(message: str, outcome: str) -> None:
    payload = {"message": message, "outcome": outcome}
    result = request([OWNER_INPUT, "hook", "owner-input", "receipt"], payload)
    event("owner_input_receipt", exit_code=result.returncode)


def owner_input(line: bytes) -> None:
    handed = json.loads(line)
    if handed["conversation"] != CONVERSATION:
        receipt(handed["message"], "moved")
        return
    event("owner_input", characters=len(handed["text"]), draft_chars=len(draft))
    if not state.get("answer", True):
        # Claude never reports what it did with it: its outcome is unknown.
        return
    with CAPTURE.open("a") as out:
        out.write(json.dumps({"text": handed["text"], "at_ns": time.monotonic_ns()}) + "\n")
    receipt(handed["message"], "submitted")
    if state.get("mode", "idle") == "idle":
        # An idle Claude starts the turn at once; a busy one after its turn.
        receipt(handed["message"], "started")
        if state.get("hook", True):
            hook()


def permission_packet() -> None:
    token = os.environ.get("CEPHEUS_ATTENTION_TOKEN")
    generation = os.environ.get("CEPHEUS_ATTENTION_SPAWNED_MS")
    if token is None or generation is None:
        event("permission_unsupported")
        return
    payload = {
        "kind": "attention_permission",
        "token": token,
        "request": state["permission_request"],
        "spawned_ms": int(generation),
        "conversation": CONVERSATION,
        "question": "Would you like to run the following command?\ncat safe.txt",
        "selected": "Allow once",
        "command": "cat safe.txt",
        "permission_tool": "Bash",
        "cwd": os.path.realpath(os.getcwd()),
        "executable": os.path.realpath("/usr/bin/cat"),
        "resolved_simple_shell": True,
        "complete": True,
        "single_prompt": True,
        "allow_once_selected": True,
    }
    payload.update(state.get("packet_overrides", {}))
    args = [DECK, "hook", PROVIDER]
    if PROVIDER == "codex":
        args.append(json.dumps(payload))
    result = request(args, payload)
    event("permission_packet", exit_code=result.returncode)


def render() -> None:
    mode = state.get("mode", "idle")
    message = "Private attention fixture ready"
    if mode == "working":
        message = "Working (esc to interrupt)"
    elif mode == "question":
        message = "Choose an option (enter to select)\r\n1. Yes\r\n2. No"
    elif mode == "approval":
        message = "Would you like to run the following command?\r\ncargo test --locked\r\n1. Yes, once\r\n2. No"
    elif mode == "trusted_permission":
        options = (
            "❯ 1. Allow once\r\n  2. Deny"
            if state.get("selection", "allow") == "allow"
            else "  1. Allow once\r\n❯ 2. Deny"
        )
        message = "Would you like to run the following command?\r\ncat safe.txt\r\n" + options
    elif mode == "error":
        message = "Error: private fixture failed"
    rows, cols, _, _ = struct.unpack("HHHH", fcntl.ioctl(FD, termios.TIOCGWINSZ, bytes(8)))
    text = "\x1b[2J\x1b[H\x1b[?25h\x1b[?2004h" + message + "\r\n"
    shown = draft if state.get("show_draft", True) else ""
    if PROVIDER == "claude":
        rule = "─" * max(10, (cols or 80) - 1)
        text += rule + "\r\n❯ " + shown.replace("\n", "\r\n") + "\r\n" + rule
        text += "\x1b[1A\r\x1b[2C"
    else:
        text += "\r\n› "
        if shown:
            text += shown.replace("\n", "\r\n")
        else:
            text += "\x1b[2mAsk Codex to do anything\x1b[0m"
        text += "\r\x1b[2C"
    if cursor_home:
        text += "\r\x1b[2C"
    sys.stdout.write(text)
    sys.stdout.flush()
    event("render", draft_chars=len(shown), rows=rows)


def refresh_control() -> None:
    global sequence, state, draft, cursor_home
    try:
        candidate = json.loads(CONTROL.read_text())
    except FileNotFoundError:
        return
    if candidate["sequence"] == sequence:
        return
    sequence = candidate["sequence"]
    state = candidate
    if "draft" in state:
        draft = state["draft"]
    cursor_home = bool(state.get("cursor_home", False))
    for value in state.get("records", []):
        record(value)
    if "context" in state:
        payload = {
            "session_id": CONVERSATION,
            "context_window": {"used_percentage": state["context"]},
        }
        payload.update(state.get("context_identity", {}))
        result = request([DECK, "statusline"], payload)
        event("context_statusline", exit_code=result.returncode)
    if state.get("redraw", True):
        render()
    if "permission_request" in state:
        permission_packet()
    if state.get("mode", "idle") == "idle" and state.get("hook", True):
        hook()
    event("control", sequence=sequence)


def submit() -> None:
    global draft
    if state.get("mode") == "trusted_permission":
        event("permission_enter")
        if state.get("selection", "allow") == "allow":
            # Fixed literal argv constructed by this explicitly trusted adapter.
            # No shell, arbitrary command payload or public test injection runs.
            result = request(["/usr/bin/cat", "safe.txt"], {})
            with (ROOT / f"tool-output-{KEY}.jsonl").open("a") as out:
                out.write(json.dumps({"stdout": result.stdout, "code": result.returncode}) + "\n")
            event("tool_completed", exit_code=result.returncode)
        return
    with CAPTURE.open("a") as out:
        out.write(json.dumps({"text": draft, "at_ns": time.monotonic_ns()}) + "\n")
    event("submitted", characters=len(draft))
    draft = ""
    render()
    if state.get("hook", True):
        hook()


def consume() -> None:
    global pending, draft, cursor_home
    while pending:
        if pending.startswith(b"\x1b[200~"):
            if b"\x1b[201~" not in pending:
                return
            body, pending = pending[6:].split(b"\x1b[201~", 1)
            draft += body.decode("utf-8")
            event("paste_received", characters=len(body))
            if state.get("show_draft", True):
                render()
                event("draft_visible", characters=len(draft))
        elif pending[:1] in (b"\r", b"\n"):
            pending = pending[1:]
            submit()
        elif pending.startswith(b"\x1b[H") or pending.startswith(b"\x1bOH"):
            pending = pending[3:]
            cursor_home = True
            render()
        elif pending.startswith(b"\x1b"):
            # Wait for a complete known control; record unknown input without
            # pretending it emptied the draft or submitted a permission.
            if len(pending) < 6 and b"\x1b[200~".startswith(pending):
                return
            event("unknown_input", bytes=len(pending))
            pending = b""
        else:
            amount = next((i for i, c in enumerate(pending) if c in (13, 10, 27)), len(pending))
            draft += pending[:amount].decode("utf-8")
            pending = pending[amount:]
            render()


tty.setraw(FD, termios.TCSANOW)
identity = {
    "pid": os.getpid(),
    "start": int(Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()[19]),
    "uid": os.getuid(),
    "boot": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
    "session": int(SESSION),
    "host": HOST,
}
with (ROOT / "actors" / f"{KEY}-{identity['pid']}-{identity['start']}.json").open("x") as out:
    json.dump(identity, out)
listener = owner_input_listener()
listened = b""
render()
hook()
while True:
    refresh_control()
    sources = [FD] if listener is None else [FD, listener.stdout]
    available, _, _ = select.select(sources, [], [], 0.05)
    if listener is not None and listener.stdout in available:
        chunk = os.read(listener.stdout.fileno(), 65536)
        if not chunk:
            event("owner_input_closed")
            listener = None
        listened += chunk
        while b"\n" in listened:
            line, listened = listened.split(b"\n", 1)
            owner_input(line)
    if FD not in available:
        continue
    try:
        chunk = os.read(FD, 65536)
    except OSError:
        break
    if not chunk:
        break
    event("raw_input", bytes=len(chunk))
    pending += chunk
    if len(pending) > 1024 * 1024:
        raise ValueError("private fake input exceeded bound")
    consume()
