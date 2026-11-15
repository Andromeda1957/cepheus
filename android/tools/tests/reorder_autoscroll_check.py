#!/usr/bin/env python3
"""Hold a dragged session at the list's edges and see the list scroll under it.

Owner, 2026-11-03, with a screenshot of a row held under the search box: "when
i try to sort sessions it doesnt auto scroll up so i can sort them like any
other app does". Fourteen sessions of an isolated deck fill more than a
screen. With the list scrolled to its end, the top row is dragged to the
search box and held still: the list must keep scrolling until that session is
first. It is then dragged to the screen's bottom and held: it must end last.

Run it through `a disposable emulator` in managed scratch with
`CEPHEUS_BIN`, `CEPHEUS_APK` and `CEPHEUS_SHOTS` set, like the other
checks in this directory.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from chat_ui_check import Device
from chat_ui_check import assert_private_guest
from chat_ui_check import m
from chat_ui_check import touch_target
from device_waits import at_rest

from test_support import scaled

ROWS = 14
# How long the finger stays still at an edge. Scrolling past every other row
# takes well under this; before the fix the list did not move while held.
HOLD = 5


def rows(device: Device) -> list[tuple[str, list[int]]]:
    """Each fully shown session card below the search box, top first: its name and bounds."""
    nodes = device.nodes()
    search = next(node for node in nodes if node["desc"] == "Search sessions")
    shown = []
    for node in nodes:
        if not re.fullmatch(r"Row \d\d", node["text"]):
            continue
        card = touch_target(nodes, node)["bounds"]
        if card[1] >= search["bounds"][3]:
            shown.append((node["text"], card))
    return sorted(shown, key=lambda row: row[1][1])


def hold_at(device: Device, x: int, y_from: int, y_to: int, shot: str) -> None:
    """Long-press, move to `y_to` in steps, stay still there for HOLD seconds, let go.

    The hold is the gesture under test, kept whole: the order reaches the deck
    only with the drop, and the caller reads it from there.
    """
    nudge = 3 if y_to > y_from else -3
    # The first move stays under the touch slop (device_check.long_press_drag).
    device.shell(
        f"input motionevent DOWN {x} {y_from}; sleep 1; input motionevent MOVE {x} {y_from + nudge}"
    )
    moves = "; ".join(
        f"input motionevent MOVE {x} {y_from + (y_to - y_from) * step // 8}" for step in range(1, 9)
    )
    device.shell(moves)
    device.screenshot(shot)  # held still at the edge
    time.sleep(HOLD)
    device.shell(f"input motionevent UP {x} {y_to}")
    # The dropped row settles into its place before the list is read again.
    at_rest(device, scaled(10))


def main() -> None:
    assert_private_guest()
    m.guard()
    root = Path(tempfile.mkdtemp(prefix="reorder-autoscroll-"))
    shots = Path(os.environ["CEPHEUS_SHOTS"])
    shots.mkdir(parents=True, exist_ok=True)
    deck = m.Deck(Path(os.environ["CEPHEUS_BIN"]), root)
    device = Device(os.environ["ANDROID_SERIAL"], shots)
    summary: dict = {}
    try:
        base_settle = m.settle

        def settle(current):
            base_settle(current)
            current.shell("pm disable-user --user 0 com.google.android.tts", check=False)

        m.settle = settle
        m.stats = lambda *_: None  # pair, then stop: this check needs only the list
        deck.start()
        m.journey(device, deck, Path(os.environ["CEPHEUS_APK"]), summary, stats_only=True)
        for index in range(1, ROWS + 1):
            deck.ctl("new", "grok", "--name", f"Row {index:02}")
        m.wait_for(f"{ROWS} sessions", 60, lambda: len(deck.sessions()) == ROWS)
        ids = {s["title"]: s["id"] for s in deck.sessions()}
        if sorted(ids) != [f"Row {index:02}" for index in range(1, ROWS + 1)]:
            raise m.CheckFailed(f"the sessions are not named Row 01 to Row {ROWS}: {sorted(ids)}")
        device.wait("the first row", timeout=30, text="Row 01")

        # Scroll to the list's end, so most rows are hidden above. Each swipe
        # takes over the last one's fling; the list is read once it has stopped.
        for _ in range(8):
            device.shell("input swipe 360 1000 360 300 200")
        at_rest(device, scaled(10))
        width, height = (int(n) for n in re.findall(r"\d+", device.shell("wm size"))[-2:])
        search = device.wait("the search box", desc="Search sessions")["bounds"]
        shown = rows(device)
        if len(shown) < 2 or shown[0][0] == "Row 01":
            raise m.CheckFailed(f"the list did not scroll to its end: {[r[0] for r in shown]}")
        name, card = shown[0]
        summary["dragged"] = name
        summary["hidden_above"] = int(name.split()[1]) - 1
        device.screenshot("01-list-end")

        # Up: hold the top row against the search box.
        x = width // 2
        hold_at(device, x, (card[1] + card[3]) // 2, search[3], "02-held-at-top")
        m.wait_for("the drop to reach the deck", 15, lambda: bool(m.order_file(deck)))
        order = m.order_file(deck)
        summary["order_after_up"] = order
        device.screenshot("03-after-up")
        if order[0] != ids[name]:
            place = order.index(ids[name]) + 1 if ids[name] in order else None
            raise m.CheckFailed(
                f"{name} held at the top edge ended at place {place} of {ROWS}, not first"
            )

        # Down: hold the same row, now first, at the screen's bottom.
        card = dict(rows(device))[name]
        hold_at(device, x, (card[1] + card[3]) // 2, height * 95 // 100, "04-held-at-bottom")
        m.wait_for("the drop to reach the deck", 15, lambda: m.order_file(deck)[:1] != [ids[name]])
        order = m.order_file(deck)
        summary["order_after_down"] = order
        device.screenshot("05-after-down")
        if order[-1] != ids[name]:
            place = order.index(ids[name]) + 1
            raise m.CheckFailed(
                f"{name} held at the bottom edge ended at place {place} of {ROWS}, not last"
            )
        summary["ok"] = True
    except Exception as error:
        summary["error"] = str(error)
        device.screenshot("failure")
        (shots / "failure-ui.xml").write_text(getattr(device, "last_dump", ""))
        raise
    finally:
        deck.stop()
        (shots / "reorder-summary.json").write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
