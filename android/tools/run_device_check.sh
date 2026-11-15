#!/usr/bin/env bash
# Run the Android journey against an explicitly selected disposable emulator.
# Build artifacts first; this script never installs or restarts your normal deck.
set -euo pipefail

if [ "$#" -lt 2 ]; then
    echo 'usage: run_device_check.sh DEBUG_APK SCREENSHOT_DIR [RELEASE_APK]' >&2
    exit 2
fi
: "${CEPHEUS_BIN:?set CEPHEUS_BIN to an absolute release binary path}"
: "${ANDROID_SERIAL:?select a disposable Android Studio emulator}"
: "${CEPHEUS_TEST_EMULATOR_SERIAL:?confirm the same disposable emulator ID}"
: "${CEPHEUS_TEST_TMPDIR:?select an existing disposable directory}"
export TMPDIR="$CEPHEUS_TEST_TMPDIR"
export CEPHEUS_APK
export CEPHEUS_SHOTS
CEPHEUS_APK=$(realpath "$1")
CEPHEUS_SHOTS=$(realpath -m "$2")
if [ "$#" -ge 3 ]; then
    export CEPHEUS_RELEASE_APK
    CEPHEUS_RELEASE_APK=$(realpath "$3")
fi
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$script_dir/device_check.py"
