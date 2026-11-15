"""Owned fake Claude: the real bundled hook authorizes one fixed read only."""

from __future__ import annotations

import hashlib
import json
import os
import select
import subprocess
import sys
import termios
import time
import tty
from datetime import datetime
from datetime import timezone
from pathlib import Path

ROOT, DECK = Path(sys.argv[1]), sys.argv[2]
SESSION = os.environ["CEPHEUS_SESSION"]
KEY = f"hub-{SESSION}"
CONVERSATION = sys.argv[sys.argv.index("--session-id") + 1]
HELPER = Path(os.environ["CEPHEUS_PERMISSION_HOOK"])
HOOK_ARGV = json.loads(os.environ["CEPHEUS_PERMISSION_HOOK_ARGV"])
assert isinstance(HOOK_ARGV, list) and len(HOOK_ARGV) == 3
assert all(isinstance(argument, str) for argument in HOOK_ARGV)
interpreter = Path(HOOK_ARGV[0])
assert interpreter.is_absolute() and interpreter.is_file()
assert HOOK_ARGV[0] == str(interpreter.resolve())
assert HOOK_ARGV[1:] == ["hook", "permission-request"]
assert HOOK_ARGV[0] == str(HELPER.resolve())
POLICY = json.loads((ROOT / "fixture-policy.json").read_text())
CONTROL = ROOT / "controls" / f"{KEY}.json"
EVENTS = ROOT / "events" / f"{KEY}.jsonl"
REQUESTS = ROOT / f"native-requests-{KEY}.jsonl"
OUTPUTS = ROOT / f"native-read-{KEY}.jsonl"
SAFE = Path.cwd() / "safe.txt"
FD = sys.stdin.fileno()
sequence = None


def event(category: str, **metadata: int) -> None:
    with EVENTS.open("a") as out:
        out.write(
            json.dumps({"category": category, "at_ns": time.monotonic_ns(), **metadata}) + "\n"
        )


def birth(pid: int, label: str) -> None:
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return
    start = int(stat.rsplit(")", 1)[1].split()[19])
    value = {
        "pid": pid,
        "start": start,
        "uid": os.getuid(),
        "boot": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
    }
    (ROOT / "actors" / f"native-{label}-{KEY}-{pid}-{start}.json").write_text(json.dumps(value))


def child(argv: list[str], payload: dict) -> subprocess.CompletedProcess:
    process = subprocess.Popen(
        argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )
    birth(process.pid, "hook")
    try:
        stdout, stderr = process.communicate(
            json.dumps(payload), timeout=POLICY["hook_timeout_seconds"]
        )
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate()
        raise
    return subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)


def request(candidate: dict) -> None:
    payload = {
        "hook_event_name": "PermissionRequest",
        "session_id": CONVERSATION,
        "prompt_id": candidate["prompt_id"],
        "cwd": str(Path.cwd().resolve()),
        "tool_name": "Read",
        "tool_input": {"file_path": str(SAFE.resolve())},
    }
    payload.update(candidate.get("overrides", {}))
    result = child(HOOK_ARGV, payload)
    try:
        response = json.loads(result.stdout) if result.stdout.strip() else None
    except json.JSONDecodeError:
        response = {"invalid_json": True}
    with REQUESTS.open("a") as out:
        out.write(
            json.dumps(
                {
                    "sequence": candidate["sequence"],
                    "prompt_id": candidate["prompt_id"],
                    "exit_code": result.returncode,
                    "stdout": result.stdout,
                    "response": response,
                    "stderr_bytes": len(result.stderr),
                }
            )
            + "\n"
        )
    expected = {
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": {"behavior": "allow"},
        }
    }
    if response == expected:
        # The requested arbitrary path is never executed. This fake implements
        # exactly one known native Read, and only after real helper approval.
        assert payload["tool_name"] == "Read" and payload["tool_input"] == {
            "file_path": str(SAFE.resolve())
        }
        with OUTPUTS.open("a") as out:
            out.write(
                json.dumps({"prompt_id": candidate["prompt_id"], "text": SAFE.read_text()}) + "\n"
            )
        event("native_read_completed")
    event("control", sequence=candidate["sequence"])


birth(os.getpid(), "provider")
assert HELPER.is_absolute() and HELPER.is_file()
manifest = {
    "helper": str(HELPER.resolve()),
    "sha256": hashlib.sha256(HELPER.read_bytes()).hexdigest(),
    "argv": HOOK_ARGV,
}
(ROOT / f"native-helper-{KEY}.json").write_text(json.dumps(manifest))
config = Path(os.environ["CLAUDE_CONFIG_DIR"])
slug = "".join(c if c.isascii() and c.isalnum() else "-" for c in os.getcwd())
transcript_path = config / "projects" / slug / f"{CONVERSATION}.jsonl"
transcript_path.parent.mkdir(parents=True, exist_ok=True)
transcript = transcript_path.open("a", buffering=1)
transcript.write(
    json.dumps(
        {
            "type": "user",
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "cwd": os.getcwd(),
            "message": {"role": "user", "content": "Private native hook fixture"},
        }
    )
    + "\n"
)
sessions = config / "sessions"
sessions.mkdir(parents=True, exist_ok=True)
(sessions / f"{os.getpid()}.json").write_text(
    json.dumps(
        {"pid": os.getpid(), "sessionId": CONVERSATION, "cwd": os.getcwd(), "kind": "interactive"}
    )
)
old = termios.tcgetattr(FD)
tty.setraw(FD)
try:
    sys.stdout.write(
        "\x1b[2J\x1b[H\x1b[?25hPrivate native Read permission fixture\r\n"
        + "─" * 79
        + "\r\n❯ \r\n"
        + "─" * 79
        + "\x1b[1A\r\x1b[2C"
    )
    sys.stdout.flush()
    result = child(
        [DECK, "hook", "claude"], {"hook_event_name": "Stop", "session_id": CONVERSATION}
    )
    assert result.returncode == 0
    event("native_ready")
    while True:
        try:
            candidate = json.loads(CONTROL.read_text())
        except FileNotFoundError:
            candidate = None
        if candidate is not None and candidate["sequence"] != sequence:
            sequence = candidate["sequence"]
            request(candidate)
        if select.select([FD], [], [], 0.03)[0]:
            data = os.read(FD, 65536)
            if not data:
                break
            event("raw_input", bytes=len(data))
finally:
    termios.tcsetattr(FD, termios.TCSANOW, old)
