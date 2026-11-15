# Repository mirror

The repository lives on the hub. Agent hosts can keep a clone that follows
it one way. This optional plugin is disabled by default.

## How it works

1. **Hub, event-driven.** When the plugin is enabled and hosts are configured,
   the service starts one thread that watches its checkout's shared Git
   directory with inotify:
   - every directory under `refs/heads` and `refs/tags`, including
     directories created later;
   - `packed-refs`, which Git replaces by renaming its lock file over it.

   `*.lock` files and every other Git file are ignored. The thread blocks in
   the kernel between changes.
2. **Debounced.** A change is announced once the refs have been quiet for 2
   seconds, or after 10 seconds of continuous change. A burst of commits,
   branch moves and a `pack-refs` therefore becomes one round.
3. **One fetch at a time per host.** Each host's existing host channel thread
   sees the new round and sends that host a single `MirrorFetch` request. It
   then waits for the answer on that same channel, which is the in-flight
   guard: a host never runs two fetches, and changes made during a fetch start
   the next round once it finishes. A host also fetches once when its channel
   connects. A failed fetch is retried after 60 seconds, or sooner if the
   refs change again.
4. **The host fetches, the hub never pushes.** The host runs
   `git fetch --quiet --prune --tags --no-write-fetch-head <remote> +refs/heads/*:refs/remotes/<remote>/*`
   in its own checkout. It is limited to 10 minutes and waits on the child's
   pidfd. `<remote>` comes from the host's own configuration; the hub never
   sends a remote, URL, refspec or path.
   - The hub's branches land in `refs/remotes/<remote>/*`.
   - Branches the hub deleted are pruned there.
   - The host's own branches and tags are never removed.
5. **`master` fast-forwards only when safe.** The host then moves its `master`
   to `refs/remotes/<remote>/master` only when that is a fast-forward. It
   leaves `master` alone, and the hub reports why, when any of these is true:
   - `master` is checked out with uncommitted work;
   - a merge, rebase, cherry-pick, revert or bisect is in progress;
   - `master` is checked out in another worktree;
   - the host's `master` is ahead of the hub's;
   - the two have diverged.

   Nothing here merges, rebases, resets or pushes.

Git LFS payloads are not transferred. Retrieve those separately when needed.

Fast-forwarding a checked-out `master` runs the host repository's `post-merge`
hook, as any fast-forward merge there does.

## Configuration

On the hub, in `config.json`:

```json
"plugins": { "git-mirror": true }
```

On each agent host, in its own `config.json`:

```json
"plugins": { "git-mirror": true },
"plugin_settings": { "git-mirror": { "remote": "hub" } }
```

The host's checkout also needs that remote, pointing at the hub's repository
over SSH, for example:

```sh
git remote add hub user@hub.example:projects/my-project
```

A host without these settings does not advertise `git-mirror-v1`. The hub then
reports that the host does not accept the repository mirror, and sends it no
request.
