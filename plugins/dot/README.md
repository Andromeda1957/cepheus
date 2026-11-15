# ChatGPT Dot bridge

An optional, unofficial provider for an existing cloud Dot. The default transport
uses a signed-in native ChatGPT Linux host over SSH (or locally), without opening
a browser. Windows can remain Dot's connected computer for Computer Use;
selecting a Linux chat host does not change that connection.

## Install and configure

The native host needs the ChatGPT Linux app with managed ChatGPT login and the
matching Cepheus binary on its command path. The Deck host needs the same
Cepheus build and, for a remote host, a working SSH configuration alias. Native
transport, delivery and live activity run in Rust; no Python, Node or browser
packages are needed for native mode. Noninteractive SSH first uses `cepheus` on
the host command path, then the standard `~/.local/bin/cepheus` installation; it
does not depend on the host shell loading an interactive login profile. The remote helper answers a version check
before room access or selection updates; an older host must be upgraded through
the normal fleet release before connection.

```sh
python3 tools/install_plugins.py dot --dry-run
python3 tools/install_plugins.py dot
```

Reopen Deck to load the plugin, then run
`:dot setup https://chatgpt.com/dots/EXISTING-DOT-ID HOST_ALIAS` and `:dot probe`.
These commands run the deck binary and config directory of the current client,
including an existing legacy profile.

Use `--native-host local` when the app and Deck run on the same Linux host.
The choice is saved privately; no personal host is hardcoded. Setup preserves
other settings and a different existing provider. Restart the Deck service once
to load the provider manifest; setup itself never restarts anything.

`:dot on` (also `:dot` or `:new dot`) connects or selects the existing controller.
`:dot off` disconnects it. Other commands: `:dot probe`, `:dot login`,
`:dot setup URL [HOST_ALIAS]`. Native login prints sign-in instructions for the
native app and never launches a window.

The Lua command uses `deck.exe dot`, the binary running the current client.
Run `cepheus dot setup --config-dir CONFIG` without configuration flags to
point an existing deck registration at that binary, for example after the binary
moved. This holds the existing controller lock, refuses unresolved delivery, and
changes only its provider command. Settings, pinned room, journal, feed and
service stay intact. The deck runs Dot in Rust only: a registration that runs
anything else, including the removed Python helper, is preserved as a custom
provider and never run.

When replacing a retained native helper, keep the existing deck row and private
state. Wait for delivery to settle: `pending.json` must be exactly JSON `null`,
and the bridge's last terminal prompt must contain no draft after `›`. The
sidebar alone proves neither. Use the bridge's `/quit`
control, without Close, then run the installed `~/.local/bin/cepheus dot setup
--config-dir CONFIG` without `--url` or `--native-host`. This changes only the managed
provider command under its controller lock; the Dot URL, room, journal and feed
remain pinned. Load the changed provider through the normal service handover,
run `cepheus dot probe --config-dir CONFIG`, then reopen the same deck row.
Only the local controller PID changes; Dot's cloud identity and work continue.
Keep the working view open: its Dot composer and unacknowledged send text are
held in that view's memory, independently of the bridge. Replacing the view
requires an empty composer and no in-flight send, or preservation of that view.

A new build installed at the registered path needs none of this. A running
chat switches itself to it when idle, and the service starts a chat that does
not, an older build, again on the installed program in the same deck row and
state directory once it is idle, with no message queued, on its way or
unsettled in `pending.json` through the normal in-place service handover.
That start writes the feed again, so a stale failure from the old build
leaves the sidebar and the phone.

The first native chat connection also refreshes Dot's verified profile card and
picture. The view follows the card's modification time without a restart. A
recognized configured pet uses its matching public animated sprite sheet; other
pictures use the fetched PNG or WebP. A cached picture is identity evidence, not
an instruction to replace a configured pet's animations with a still image.

`install_plugins.py --repo-sources` delegates validation and publication for a
Dot registration to the matching `cepheus dot repo-sources` binary on the
installer's command path; its controller lock covers the profile write.
Disconnect Dot first and resolve uncertain delivery. Custom registrations,
untrusted binary paths and active controllers are preserved and rejected.
Dry-run creates no state or lock. Only the plugin source selection changes;
the registered binary, state, settings, pinned room, journal and feed are
preserved. Direct `setup` without URL/host flags performs the binary move
described above; explicit configuration can follow separately.
Ordinary plugin installation preserves the selected profile mode.

New defaults prefer nonempty `CEPHEUS_CONFIG` and `CEPHEUS_STATE_DIR`; the former
names a config file and the latter a Deck state root. Legacy environment names
and an existing legacy directory remain fallbacks. An existing proven Dot
registration keeps its exact state directory, even when the default changes.
No state is moved or copied.

## Native transport and delivery

An owned headless app-server uses the app's normal account RPC and HTTPS room
APIs. Current account/user must match the selected Dot's cached profile. The
room ID, Dot ID, direct-message room type and sender membership are checked.
The task root is execution metadata and is never used as the chat transcript.

Credentials stay in memory on the native host. Managed renewal uses the normal
app-server method; HTTP 401 retries once after verifying the refreshed principal.
Tokens, signed WebSocket URLs and cookies never cross SSH or enter arguments,
snapshots or logs. No login file, browser profile or keychain is extracted.

The room is pinned across reconnects. An unexpected change stops the bridge;
explicit setup of the intended URL can select it again. Setup refuses selection
changes while delivery is uncertain.

After reboot, the bridge retries while the native host or app profile cache is
still starting. It reads both the legacy shared cache and the app's account-scoped
cache, matching the signed-in account, user and existing Dot. No manual app-open
or reconnect step is needed once the saved login and profile are available.
Conflicting room identities still stop the connection; pending sends are never
automatically replayed.

The desktop Deck draws Dot natively in Rust from the bridge's message snapshot,
the way it draws local agents: the owner's messages after `›` on a shaded band,
Dot's replies after a bullet in its colour with rendered Markdown, each
message's send time on a dim row under it (the time alone today, else
`Mar 4, 1:30 PM`, as the phone writes it), rows wrapped
between words, local scrollback (PgUp/PgDn, wheel, End, Ctrl-Home) and a
composer that grows to six rows. Enter sends through the existing `provider-dot/send`
Plugin channel and the controller's private Unix socket, without typing into a
terminal. The socket checks its owning UID, exact controller PID and process
start time; connection attempts honor cancellation and the delivery deadline. Ctrl-V
attaches clipboard PNG, JPEG, GIF or WebP images (up to ten and 20 MiB total);
phone uploads use the same adapter. Images upload through the app's existing
room-scoped multipart API, then their file IDs accompany the idempotent message.
An older controller or host without this capability refuses before submission;
it never falls back to terminal paste. Alt-Enter or Ctrl-J adds a line; a refused
or unconfirmed send returns its text and images to the composer. Backspace on
empty text removes the last image. Over 32,000 characters is rejected without
truncation. Ctrl-U clears input and attachments; `/quit` disconnects and `/reconnect`
reopens the same pinned conversation. Dot is always on in ChatGPT, so the sidebar
shows its name and pet without ready, working, finished, off or host words;
only a failure from the running bridge (it cannot connect, delivery is
uncertain, a turn failed) shows under the name and in the chat. Readable message columns
and the composer use the current theme, without shortcut help or a cloud agent
execution-host label. A failed snapshot read retains the last valid history.
The bridge merges recent windows by message ID, updates streamed replies in
place, and removes only explicit tombstones or the oldest retained history at
its 500-message/4 MiB bound. An acknowledged message stays visible until its ID
reaches the snapshot.

Before POST, a private journal records text, uploaded file IDs, prior IDs, room and one request UUID.
The same UUID is the server idempotency token. A matching new user echo confirms
delivery. Lost acknowledgements never cause automatic resend. Further sends stay
blocked until reconnect finds the echo or the owner checks ChatGPT and enters
`/resolve`. Resolve clears uncertainty without sending.

Each native request bounds connecting (20 s), any read or write stall (30 s)
and the response head (60 s), within 100 s overall. A slow or stalled response
is a temporary failure.

Network, service and timeout failures reconnect on their own after 5 s,
doubling to at most five minutes, with a bounded Retry-After for HTTP 429. A
problem found after connecting reconnects once at once, then waits the same
way. A problem found while connecting, such as a signed-out app, a login
another host is renewing or an older host build, shows its notice with
"Retrying automatically." and waits the same way: each attempt starts a fresh
app-server that reads the current login and repeats every room and identity
check, and nothing is resent.
A send while disconnected connects first. A failed send says "Not sent" when
it never reached ChatGPT and "Delivery unconfirmed" once the journal holds it;
an acknowledged send is never reported as failed.

## Live status and clients

The bundled Rust WebSocket consumer reads the normal native room subscription. Room,
subscription ID and monotonic revision must match. Only authoritative attached
state supplies turn completion; message arrival does not imply completion.
Stalled bootstrap reconnects and unavailable activity is recorded as unknown.
The host announces each live activity change, so the chat refreshes at once
instead of on a timer. It refreshes every 2 s while Dot works; when idle the
wait doubles to one minute, or 8 s when live activity is unavailable.

Desktop and Android retain **Cloud agents** above **Local agents**, native text
bubbles and On/Off controls. Off closes only Deck's owned helpers. Dot's cloud
work and connected computer continue. The desktop sidebar lists only cloud
agents that are on, so `:dot off` with no other cloud agent on removes the
whole **Cloud agents** section and `:dot on` brings it back. Both the Deck host and native chat host
must be online for phone access. The feed retains bounded message history and
image attachment names. Full attachment previews, task cards and inline
approvals remain in ChatGPT.

## Private state

`$XDG_STATE_HOME/cepheus/dot/`, normally `~/.local/state/cepheus/dot/`,
remains mode 0700 with bridge-written files mode 0600:

- `settings.json`: URL and native-host alias.
- `selection.json`: pinned URL, Dot and room identity.
- `pending.json`: uncertain delivery and original request UUID.
- `transcript.json`: bounded feed with integer-millisecond timestamps.
- `bridge.lock`: prevents concurrent controllers.
- `profile.json`: verified name, avatar type, selected pet and image identity.
- `avatar.png` or `avatar.webp`: the current verified profile picture.

The old `browser/` login from the removed Python browser route is preserved
owner data; nothing reads or syncs it. That copied-cookie route reproduced 403
challenges twice. Never delete browser profiles as a reset step.

## Compatibility and validation

The bridge uses private signed-in app interfaces. Identity and response
validation fail closed when an interface changes. Isolated fixtures cover
terminal input, registration changes, pinned-room validation, delivery
journal reconciliation, stalled responses and reconnect pacing. Live account
acceptance must be verified separately for the installed app version.

Official references: [app-server auth](https://learn.chatgpt.com/docs/app-server#auth-endpoints),
[Dot computers](https://learn.chatgpt.com/docs/dots/computers-and-apps),
[Linux app](https://learn.chatgpt.com/docs/linux/linux-app).

Deck activity comes from a private `activity.json` beside the transcript, tied to
its current bridge process and refreshed from live snapshots. Missing or stale
activity cannot mark Dot as working; text in messages and drafts is never used.
Cloud Dot connections keep their existing conversation and reject `:cd` without
stopping the bridge.
