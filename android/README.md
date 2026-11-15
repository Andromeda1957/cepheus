# Cepheus for Android

A native Android companion for Cepheus. Pair it with a desktop or server gateway
to read agent conversations, send messages and attachments, answer questions, and
receive notifications. Each saved connection pins its gateway's TLS certificate;
credentials are encrypted with Android Keystore and device backups are disabled.

The app includes:

- Home, drawer, session search, saved conversations and a new-session form.
- Native chat with Markdown, tool details, images, skills, model controls and
  question cards; saved drafts and offline conversation history.
- Saved connections, appearance and notification settings, Stats and Errors.
- Deck tools for Branches, Ask, Broadcast, Terminals, Scans, Extensions,
  Scripting, Service, Scheduler, Jobs and Queue.
- Light, dark and system themes, background delivery and notification replies.

Dark is the default theme from launch. Choose Light or System default in Settings → Theme;
explicit saved choices stay in effect.

The application ID is `io.github.cepheus.app`; debug builds use
`io.github.cepheus.app.debug`. These builds install independently of other apps.
The desktop gateway and phone must support the same protocol facilities. A
feature unavailable on an older gateway displays its update requirement.

## Build

Install JDK 17 or newer and the Android SDK platform required by
[app/build.gradle](app/build.gradle), with its build tools and accepted licenses.
Set `JAVA_HOME` and `ANDROID_HOME` for your installation. Dependencies and plugin
versions are pinned in [gradle/libs.versions.toml](gradle/libs.versions.toml) and
Gradle lockfiles; the wrapper downloads its pinned Gradle distribution.

```sh
cd android
./gradlew testDebugUnitTest assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Enable the phone gateway in the desktop's `~/.config/cepheus/config.json`.
Set the listener to the desktop's reachable LAN or VPN address and allow your
phone's address. For example, replace these example addresses with your own:

```json
{
  "remote": true,
  "remote_listen": "192.0.2.10:8765",
  "remote_allow": ["192.0.2.20"]
}
```

The service reloads these settings. Open the app, choose Add connection and
scan the QR code from the desktop's
pairing screen (`Alt+p`), or enter its address, certificate fingerprint and pairing
code. The gateway must be reachable from your phone through your own network or
VPN. The app verifies the pinned certificate on every connection.

## Release signing

Release builds are unsigned unless all four variables are explicitly supplied:
`CEPHEUS_KEYSTORE`, `CEPHEUS_KEY_ALIAS`, `CEPHEUS_KEYSTORE_PASSWORD` and
`CEPHEUS_KEY_PASSWORD`. Keep your signing key and passwords outside the checkout;
use your secret manager to provide passwords without adding them to shell history.

```sh
./gradlew assembleRelease
```

Use `apksigner verify --print-certs` from your installed Android build tools to
verify a signed APK before distributing it. Retain the same signing key when
publishing updates to an existing installation.

## Tests and screenshots

JVM tests under `app/src/test` cover protocol parsing, TLS pinning, reconnects,
message delivery, uploads, Markdown, notification identity, settings, search and
session state. Run them with `./gradlew testDebugUnitTest`.

Device journeys use a disposable Android Studio emulator and an isolated Cepheus
service with synthetic providers, conversations and credentials. They clear the
test app's data, install the selected APK, and change emulator settings. Do not
select an emulator containing data you want to keep. The scripts reject physical
devices and require both serial variables to name the same emulator explicitly.
They leave your normal Cepheus service, provider accounts and physical phone alone.

Install Python 3, Pillow (`python3 -m pip install pillow`), `websockets` for the
transport/queue checks, and `adb`. Build the native release binary and debug APK,
then set paths for your machine:

```sh
export CEPHEUS_BIN=/absolute/path/to/cepheus/target/release/cepheus
export ANDROID_SERIAL=emulator-5554
export CEPHEUS_TEST_EMULATOR_SERIAL=emulator-5554
export CEPHEUS_TEST_TMPDIR=/absolute/path/to/disposable-test-directory
export TMPDIR="$CEPHEUS_TEST_TMPDIR"
export CEPHEUS_APK=/absolute/path/to/cepheus/android/app/build/outputs/apk/debug/app-debug.apk
export CEPHEUS_SHOTS="$CEPHEUS_TEST_TMPDIR/screenshots"
python3 android/tools/device_check.py
```

Run from the repository root. `CEPHEUS_TEST_TMPDIR` must already exist and be
writable. Capture directories must stay beneath it. On Linux, the harness uses
`/proc` directory descriptors when a Unix socket needs a shorter path.
`CEPHEUS_TEST_TIMEOUT_SCALE` optionally multiplies hang deadlines (minimum `1`).
`CEPHEUS_PACKAGE` and `CEPHEUS_ACTIVITY` select alternative build identities.

The main journey covers pairing, Home, Stats, Settings, New session, chat,
attachments, notifications, questions, the drawer, session ordering and reconnection.
For a focused journey, set `CEPHEUS_ONLY` to `chat`, `navigation`, `resume`,
`search`, `search-keyboard`, `tool-details` or `system-bars`.
`CEPHEUS_COMMANDS_ONLY=1` exercises the skills and slash-command picker.

Additional journeys under `tools/tests` include `deck_tools_check.py`,
`connections_check.py`, `jobs_check.py`, `scheduler_check.py`, `queue_check.py`,
`reply_actions_check.py`, `subagents_check.py` and `phone_transport_check.py`.
Run them with the same environment. `device_check.py` exports the `Deck` and
`Device` fixtures for additional screenshot journeys; all fixture services are
stopped at the end and screenshots remain in the selected output directory.
