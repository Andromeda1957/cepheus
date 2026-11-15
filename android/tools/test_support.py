"""Standalone storage, deadlines and device safeguards for Android journeys."""

from __future__ import annotations

import atexit
import math
import os
import subprocess
from pathlib import Path

_descriptors: dict[Path, int] = {}


def storage_root() -> Path:
    value = os.environ.get("CEPHEUS_TEST_TMPDIR")
    if not value:
        raise RuntimeError("Set CEPHEUS_TEST_TMPDIR to an existing disposable directory")
    path = Path(value).resolve(strict=True)
    if not path.is_dir() or not os.access(path, os.W_OK | os.X_OK):
        raise RuntimeError("CEPHEUS_TEST_TMPDIR must be a writable directory")
    return path


def is_managed_path(path: Path | str) -> bool:
    """Limit fixture inputs and captures to the selected disposable directory."""
    return Path(path).resolve().is_relative_to(storage_root())


def short_path(path: Path | str) -> Path:
    """Keep Unix socket paths short without writing aliases outside test storage."""
    physical = storage_root()
    candidate = Path(path).resolve()
    if not candidate.is_relative_to(physical):
        raise ValueError("Fixture path escapes CEPHEUS_TEST_TMPDIR")
    if len(os.fsencode(candidate)) < 100:
        return candidate
    if physical not in _descriptors:
        descriptor = os.open(physical, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        _descriptors[physical] = descriptor
        atexit.register(os.close, descriptor)
    return Path(f"/proc/{os.getpid()}/fd/{_descriptors[physical]}") / candidate.relative_to(physical)


def scaled(seconds: float) -> float:
    scale = float(os.environ.get("CEPHEUS_TEST_TIMEOUT_SCALE", "1"))
    if not math.isfinite(scale) or scale < 1:
        raise ValueError("CEPHEUS_TEST_TIMEOUT_SCALE must be a finite number of at least 1")
    return seconds * scale


def assert_private_emulator() -> None:
    """Require an explicit disposable emulator selection before clearing app data."""
    serial = os.environ.get("ANDROID_SERIAL", "")
    if not serial.startswith("emulator-") or serial != os.environ.get("CEPHEUS_TEST_EMULATOR_SERIAL"):
        raise RuntimeError("Set ANDROID_SERIAL and CEPHEUS_TEST_EMULATOR_SERIAL to the same disposable emulator ID")
    result = subprocess.run(
        ["adb", "-s", serial, "shell", "getprop", "ro.kernel.qemu"],
        text=True, capture_output=True, check=True, timeout=10,
    )
    if result.stdout.strip() != "1":
        raise RuntimeError("Android journeys require a disposable emulator, not a physical device")
    storage_root()
