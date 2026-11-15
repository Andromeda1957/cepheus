"""Waits on what a pool guest shows, read until it holds instead of slept.

`tools/checks/check_test_hygiene.py` refuses fixed sleeps in these checks. A
read of the device returns as soon as the phone is ready, and its deadline
still bounds a slow one.
"""

from __future__ import annotations

import time
from collections.abc import Callable
from typing import Protocol
from typing import TypeVar

T = TypeVar("T")

# Seconds between reads. Each read is an adb round trip, and a uiautomator
# dump alone waits for a second without accessibility events, so this only
# paces the cheap reads.
INTERVAL = 0.2


class Screen(Protocol):
    def nodes(self) -> list[dict]: ...


def poll(seconds: float, check: Callable[[], T]) -> T:
    """`check()`'s first truthy result within `seconds`, else its last result.

    The last read comes at or after the deadline, so a falsy result means no
    read in the whole window saw it: a wait that ran out, or a watched
    problem that never appeared.
    """
    deadline = time.monotonic() + seconds
    while True:
        result = check()
        if result or time.monotonic() >= deadline:
            return result
        time.sleep(INTERVAL)


def at_rest(device: Screen, seconds: float) -> list[dict]:
    """The screen's nodes once two dumps in a row agree: a fling or a drop has settled.

    Rows read while a list still moves are not where a tap lands. After
    `seconds` the latest dump is returned as it is.
    """
    previous: list[tuple] | None = None
    deadline = time.monotonic() + seconds
    while True:
        nodes = device.nodes()
        placed = [(node["text"], node["desc"], node["bounds"]) for node in nodes]
        if (nodes and placed == previous) or time.monotonic() >= deadline:
            return nodes
        previous = placed
        time.sleep(INTERVAL)
