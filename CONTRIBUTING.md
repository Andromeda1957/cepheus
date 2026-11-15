# Contributing

Build from this repository alone with `cargo build --release --locked`.
Use `cargo fmt --check` for Rust formatting and
`python3 tools/install_plugins.py --check` for plugin payload validation.
Do not add credentials, saved conversations, runtime configuration or build
output to commits. Keep generic facilities in Rust and optional integrations
in their plugin packages.

## Verification

Run the full Rust suite in a disposable Linux environment with a private home,
state, runtime and temporary directory. Some integration tests deliberately
exercise skill installation, hooks and client settings. Set
`CEPHEUS_TEST_DISPOSABLE_HOME=1` only in that environment; it acknowledges that
those settings belong to the disposable test home. Tests start their own
synthetic services and providers and need no signed-in AI account.

```sh
cargo build --release --locked
CEPHEUS_TEST_DISPOSABLE_HOME=1 cargo test --release --locked
python3 -m unittest discover -s tests -p 'test_*.py'
```

Use an unprivileged user and keep temporary files in the environment's
disposable storage. Preserve provider and transport checks when changing
session lifecycle, message delivery or host authentication. New tests should
cover behavior that can realistically regress.

The initial standalone full-suite run reported 1,331 passes, 30 failures and
one ignored documentation capture. Some failures assert earlier terminal-based
broadcast, image submission, Send now and Clear behavior, while current native
provider tests cover their replacements. Other failures involve protocol and
service-readiness fixtures. The profile/path and permission fixture corrections
pass their targeted tests; the entire suite is not claimed to pass.
Preserve the native transport and captured-target safeguards when investigating
these failures. Do not restore terminal injection to satisfy stale expectations.

The Android app is a separate Gradle project:

```sh
cd android
./gradlew testDebugUnitTest assembleDebug
```

See [Android testing](android/README.md#tests-and-screenshots) for its explicit
disposable-emulator requirements.

## Screenshots

The [gallery](docs/screenshots/README.md) documents the running desktop TUI and
Android app using disposable demo data.

Desktop images are native Foot window captures of the live TUI, including its
real provider client. Run the demo service and clients in an isolated container
with a private PID namespace and home, so discovery cannot include your existing
sessions. Use a small neutral Git repository and real task sources. Do not draw
or replay an invented provider conversation.

Start a dedicated tmux server with a private socket and attach its pane to the
container's TUI. Install Foot, tmux, grim with toplevel capture support, and
JetBrainsMono Nerd Font on a Hyprland Wayland session. Capture the selected pane:

```sh
python3 tools/capture_screenshots.py --live /path/to/private/tmux.sock \
  --target capture:0.0 --capture /path/to/captures/01-workspace.png \
  --columns 160 --rows 38
```

The capture tool attaches only to the supplied socket; it does not start an
agent or read credentials. Use an empty, inactive capture workspace (10 by
default). The command keeps that workspace hidden and closes its own terminal
window afterward. Open each overlay in the live application before capturing it.

Prepare a schema 1 `manifest.json` alongside the images, with a `screens` list
containing an `id`, `title`, and PNG basename in `image` for each view. Import a
reviewed batch to update the images and gallery:

```sh
python3 tools/capture_screenshots.py --desktop /path/to/captures
```

Android captures run the real debug APK against isolated synthetic gateway
fixtures. After preparing the environment described in the Android README:

```sh
python3 tools/capture_android_screenshots.py
python3 tools/capture_screenshots.py --android "$CEPHEUS_SHOTS"
```

Capture only disposable sessions and emulator data. Review screenshots for
readability and private information before updating the gallery.

## Source archives

`python3 tools/export_source.py --output /path/to/new/cepheus.tar` exports the
current committed public source with no Git history. The destination must not
already exist. The exporter rejects archives containing `private/`, and
`.gitattributes` excludes that directory. Commit the intended files before
exporting; uncommitted changes are not included.

Contributions are licensed under the repository's MIT license. Preserve
third-party attribution when copying or modifying derived code.
