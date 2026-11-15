//! Host-side custody and bounded retention for offload storage.

use super::hosts::BuildHost;
use super::sync::MirrorLock;
use crate::config::OffloadConfig;
use anyhow::{bail, Result};

/// Host-side custody shared by copying, setup, commands and artifact transfer.
///
/// The versioned namespace leaves legacy clients and their caches alone.
/// Cache collection takes the stable global lock exclusively; every live
/// invocation and operation holds it shared. Mirror lock identities are never
/// reused, and late operations verify active/owned records after locking.
macro_rules! storage_sh {
    () => {
        r#"
umask 077
storage_parent="$HOME/.cache/cepheus-offload"
storage_root="$storage_parent/storage-v1"
control="$storage_root/control"
cache="$storage_root/cache"
private_dir() {
  [ ! -L "$1" ] || return 1
  mkdir -p -m 700 -- "$1" || return 1
  [ -d "$1" ] && [ -O "$1" ] && [ "$(stat -c %a -- "$1")" = 700 ]
}
read_marker() {
  python3 - "$1" <<'CEPHEUS_OFFLOAD_MARKER'
import os
import stat
import sys

limit = 1024
path_fd = data_fd = None
try:
    path_fd = os.open(sys.argv[1], os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC)
    info = os.fstat(path_fd)
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_uid != os.getuid()
        or info.st_nlink != 1
        or info.st_mode & 0o077
        or not 0 < info.st_size <= limit
    ):
        raise ValueError("invalid marker")
    data_fd = os.open(
        f"/proc/self/fd/{path_fd}",
        os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC,
    )
    opened = os.fstat(data_fd)
    if (opened.st_dev, opened.st_ino) != (info.st_dev, info.st_ino):
        raise ValueError("marker identity changed")
    value = os.read(data_fd, limit + 1)
    if (
        not 0 < len(value) <= limit
        or not value.endswith(b"\n")
        or any(byte < 32 or byte > 126 for byte in value[:-1])
        or os.fstat(data_fd).st_size > limit
    ):
        raise ValueError("invalid marker content")
except (OSError, ValueError):
    raise SystemExit(1)
finally:
    if data_fd is not None:
        os.close(data_fd)
    if path_fd is not None:
        os.close(path_fd)
sys.stdout.buffer.write(value)
CEPHEUS_OFFLOAD_MARKER
}
storage_init() {
  [ ! -L "$HOME/.cache" ] && [ ! -L "$storage_parent" ] || return 1
  mkdir -p -m 700 -- "$storage_parent" || return 1
  [ -O "$storage_parent" ] || return 1
  exec 5>"$storage_parent/.storage-v1.init.lock"
  flock -x 5 || return 1
  if [ ! -e "$storage_root" ] && [ ! -L "$storage_root" ]; then
    mkdir -m 700 -- "$storage_root" || return 1
    printf '%s\n' cepheus-offload-storage-v1 > "$storage_root/.owner"
  fi
  private_dir "$storage_root" || return 1
  [ "$(read_marker "$storage_root/.owner")" = cepheus-offload-storage-v1 ] || return 1
  private_dir "$control" && private_dir "$storage_root/mirrors" || return 1
  exec 5>&-
}
storage_path() {
  relative=$1
  id=${relative##*/}
  [[ "$id" =~ ^task-[A-Za-z0-9_.-]+-[0-9a-f]{32}$ ]] || return 1
  [ "$relative" = ".cache/cepheus-offload/storage-v1/mirrors/$id" ] || return 1
  mirror="$HOME/$relative"
}
use_storage() {
  storage_init && storage_path "$1" || return 1
  exec 8>"$control/cache.lock"
  flock -s 8 || return 1
  exec 9<"$control/live-$id.lock"
  flock -s 9 || return 1
  [ -f "$control/$id.active" ] && [ ! -L "$control/$id.active" ] &&
    [ "$(read_marker "$control/$id.owned")" = "$relative" ] &&
    [ -d "$mirror" ] && [ ! -L "$mirror" ] || return 1
  private_dir "$cache" || return 1
  export UV_CACHE_DIR="$cache/uv-downloads"
  export UV_PYTHON_INSTALL_DIR="$cache/python"
}
cache_snapshot() {
  max_bytes=$1; idle_seconds=$2
  cache_bytes=0; cache_expired=0
  [ -e "$cache" ] || return 0
  private_dir "$cache" || return 1
  cache_bytes=$(du -sx -B1 -- "$cache") || return 1
  cache_bytes=${cache_bytes%%[[:space:]]*}
  now=$(date +%s)
  last=$(stat -c %Y -- "$control/cache.used" 2>/dev/null || printf '%s' "$now")
  if [ "$cache_bytes" -gt "$max_bytes" ] || [ "$((now-last))" -ge "$idle_seconds" ]; then
    cache_expired=1
  fi
}
# Call only with exclusive cache custody; never remove individual Cargo files.
retire_cache() {
  cache_snapshot "$1" "$2" || return 1
  [ "$cache_expired" = 1 ] || return 0
  rm -rf -- "$cache" || return 1
  private_dir "$cache" || return 1
  printf '[offload] retired quiescent reusable cache (%s bytes)\n' "$cache_bytes" >&2
}
collect_cache() (
  exec 7>"$control/cache.lock"
  flock -n -x 7 || exit 0
  retire_cache "$1" "$2"
)
wait_lease_lock() {
  while ! flock -n "$1" "$2"; do
    lease_owned && [ ! -f "$lease_cancel" ] || return 143
    sleep 0.1
  done
  lease_owned && [ ! -f "$lease_cancel" ] || return 143
}
admit_cache() {
  # A lease observing expiry fences subsequent admissions until existing
  # consumers finish. Their operations bypass admission and retain custody.
  # This prevents an endless stream of leases from pinning an oversized cache.
  exec 7>"$control/admission.lock"
  wait_lease_lock -x 7 || return $?
  cache_snapshot "$1" "$2" || return 1
  exec 8>"$control/cache.lock"
  if [ "$cache_expired" = 1 ]; then
    wait_lease_lock -x 8 || return $?
    retire_cache "$1" "$2" || return 1
  fi
  wait_lease_lock -s 8 || return $?
  private_dir "$cache" || return 1
  touch "$control/cache.used" || return 1
  exec 7>&-
}
reap_mirrors() {
  for record in "$control"/*.owned; do
    [ -f "$record" ] && [ ! -L "$record" ] || continue
    (
      candidate=$(read_marker "$record") || exit 1
      storage_path "$candidate" || exit 1
      [ "$record" = "$control/$id.owned" ] || exit 1
      exec 6>"$control/live-$id.lock"
      flock -n -x 6 || exit 0
      rm -f -- "$control/$id.active" || exit 1
      rm -rf -- "$mirror" || exit 1
      rm -f -- "$record" "$control/live-$id.lock" "$control/copy-$id.lock" "$control/$id.cancel" "$control/$id.done"
    ) || return 1
  done
}

# A fresh subreaper owns this command and every adopted descendant. Custody
# stays in the supervisor even when a child changes session or closes its fds.
owned_run() (
  mode=$1; cleanup_dir=$2; shift 2
  context=$(mktemp -d "$mirror/.cepheus-offload/process.XXXXXX") || exit 1
  pid=; channel_watch=
  close_owned_slots() {
    if declare -F close_slots >/dev/null; then close_slots; fi
  }
  finish_owned() {
    result=$?
    trap - EXIT
    trap '' HUP INT TERM PIPE
    set +e
    : > "$context/cancel"
    [ -z "$pid" ] || wait "$pid" 2>/dev/null
    : > "$context/done"
    [ -z "$channel_watch" ] || wait "$channel_watch" 2>/dev/null
    if [ -f "$context/admitted" ] && [ ! -f "$context/reaped" ]; then
      trap '' HUP INT TERM PIPE
      printf '[offload] descendant shutdown is unverified; retaining custody at %s\n' "$context" >&2
      while :; do sleep 1; done
    fi
    if [ -n "$cleanup_dir" ]; then rm -rf -- "$cleanup_dir" || result=1; fi
    rm -rf -- "$context" || result=1
    exit "$result"
  }
  trap finish_owned EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  supervisor=$(cat <<'CEPHEUS_OFFLOAD_SUPERVISOR'
import ctypes
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

context, active = Path(sys.argv[1]), Path(sys.argv[2])
command = sys.argv[3:]
stops = (signal.SIGHUP, signal.SIGINT, signal.SIGTERM)
cancelled = 0
child = None
child_descriptor = None
owns_children = False
result = 1


def interrupted(signum, _frame):
    global cancelled
    if not cancelled:
        cancelled = 128 + signum


def report(message):
    try:
        print(message, file=sys.stderr, flush=True)
    except (OSError, ValueError):
        pass


def children():
    # This fresh single-threaded supervisor has no unrelated children or reaper.
    return [int(pid) for pid in Path("/proc/thread-self/children").read_text().split()]


def drain():
    term_sent, kill_sent = set(), set()
    deadline = time.monotonic() + 2
    last_error = None
    notice = time.monotonic() + 10
    failed = False
    while True:
        try:
            while True:
                try:
                    pid, status = os.waitpid(-1, os.WNOHANG)
                except ChildProcessError:
                    break
                if not pid:
                    break
                term_sent.discard(pid)
                kill_sent.discard(pid)
                if child is not None and pid == child.pid:
                    child.returncode = os.waitstatus_to_exitcode(status)
            owned = children()
            if not owned:
                return not failed
            signum = signal.SIGTERM if time.monotonic() < deadline else signal.SIGKILL
            sent = term_sent if signum == signal.SIGTERM else kill_sent
            for pid in owned:
                if pid in sent:
                    continue
                # An unreaped direct child pins its PID. Only this supervisor
                # reaps it, so opening its kernel handle cannot select a reused
                # unrelated process. Adopted setsid/double-fork children qualify.
                descriptor = os.pidfd_open(pid)
                try:
                    try:
                        signal.pidfd_send_signal(descriptor, signum)
                    except ProcessLookupError:
                        pass
                finally:
                    os.close(descriptor)
                sent.add(pid)
        except BaseException as error:
            failed = True
            message = f"{type(error).__name__}: {error}"
            if message != last_error:
                report(f"[offload] retaining custody until owned children are reaped: {message}")
                last_error = message
        if time.monotonic() >= notice:
            report("[offload] owned children remain after cancellation; retaining storage custody")
            notice = time.monotonic() + 30
        # No deadline releases custody while any owned child remains alive.
        time.sleep(0.02)


for signum in stops:
    signal.signal(signum, interrupted)
try:
    if signal.getsignal(signal.SIGCHLD) != signal.SIG_DFL or children():
        raise OSError("offload supervisor already has children or a child reaper")
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0):  # PR_SET_CHILD_SUBREAPER
        raise OSError(ctypes.get_errno(), "cannot own offload descendants")
    owns_children = True
    # Fail before launch if kernel process handles or custody are unavailable.
    descriptor = os.pidfd_open(os.getpid())
    try:
        signal.pidfd_send_signal(descriptor, 0)
    finally:
        os.close(descriptor)
    os.fstat(8)
    os.fstat(9)
    if cancelled or not active.is_file() or (context / "cancel").exists():
        result = cancelled or 143
    else:
        # If the supervisor disappears after admission, the outer custodian
        # retains the allocation rather than inferring shutdown from its exit.
        (context / "admitted").touch()
        # Literal argv; inherited stdin carries setup/plan/rsync data or /dev/null.
        # The supervisor retains custody; payloads cannot pin it by inheriting fds.
        child = subprocess.Popen(command, close_fds=True)
        child_descriptor = os.pidfd_open(child.pid)
        while True:
            status = child.poll()
            if status is not None:
                result = status if status >= 0 else 128 - status
                break
            if cancelled or not active.is_file() or (context / "cancel").exists():
                result = cancelled or 143
                break
            time.sleep(0.05)
except BaseException as error:
    report(f"[offload] owned command failed: {error}")
    result = 1
finally:
    for signum in stops:
        signal.signal(signum, signal.SIG_IGN)
    if owns_children and not drain():
        result = 1
    if child_descriptor is not None:
        os.close(child_descriptor)

# These receipts are emitted only after the command and adopted children are reaped.
(context / "final.new").write_text(f"{result}\n", encoding="ascii")
os.replace(context / "final.new", context / "final.status")
(context / "reaped").touch()
raise SystemExit(result)
CEPHEUS_OFFLOAD_SUPERVISOR
)
  if [ "$mode" = channel ]; then
    exec 3<&0
    ( close_owned_slots; exec setsid -w python3 -c "$supervisor" "$context" "$control/$id.active" "$@" ) </dev/null 3<&- &
  else
    ( close_owned_slots; exec setsid -w python3 -c "$supervisor" "$context" "$control/$id.active" "$@" ) <&0 &
  fi
  pid=$!
  if [ "$mode" = channel ]; then
    (
      close_owned_slots; exec 8>&- 9>&-
      while [ -f "$control/$id.active" ] && [ ! -f "$context/done" ]; do
        if IFS= read -r -t 0.25 ignored <&3; then :; else
          state=$?
          if [ "$state" -le 128 ]; then : > "$context/cancel"; break; fi
        fi
      done
    ) >/dev/null 2>&1 &
    channel_watch=$!
    exec 3<&-
  fi
  wait "$pid" || true
  result=1
  if [ -f "$context/reaped" ] && [ -f "$context/final.status" ]; then
    IFS= read -r result < "$context/final.status"
    [[ "$result" =~ ^[0-9]+$ ]] && [ "$result" -le 255 ] || result=1
  fi
  exit "$result"
)
"#
    };
}
pub(crate) use storage_sh;

pub(crate) const LEASE: &str = concat!(
    storage_sh!(),
    r#"
set -euo pipefail
max_bytes=$2; idle_seconds=$3
storage_init
reap_mirrors
storage_path "$1"
exec 9>"$control/live-$id.lock"; flock -s 9
[ ! -e "$mirror" ] && [ ! -L "$mirror" ]
[ ! -e "$control/$id.owned" ]
printf '%s\n' "$relative" > "$control/$id.owned"
lease_cancel="$control/$id.cancel"
lease_done="$control/$id.done"
admitted=0
channel_watch=
lease_owned() {
  [ -f "$control/$id.owned" ] && [ ! -L "$control/$id.owned" ] &&
    [ "$control/live-$id.lock" -ef "/proc/$$/fd/9" ] &&
    [ -d "$mirror" ] && [ ! -L "$mirror" ]
}
finish_lease() {
  status=$?
  trap - EXIT HUP INT TERM
  set +e
  : > "$lease_done"
  [ -z "$channel_watch" ] || wait "$channel_watch" 2>/dev/null
  exec 7>&-
  rm -f -- "$control/$id.active" || status=1
  flock -u 9
  if flock -x 9 && rm -rf -- "$mirror"; then
    rm -f -- "$control/$id.owned" "$control/live-$id.lock" "$control/copy-$id.lock" "$lease_cancel" "$lease_done" || status=1
  else
    status=1
  fi
  exec 9>&- 8>&-
  if [ "$admitted" = 1 ]; then
    touch "$control/cache.used" || status=1
    collect_cache "$max_bytes" "$idle_seconds" || status=1
  fi
  exit "$status"
}
trap finish_lease EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
private_dir "$mirror"
private_dir "$mirror/.cepheus-offload"
exec 3<&0
(
  exec 9>&- 8>&-
  while [ ! -f "$lease_done" ]; do
    lease_owned || exit 143
    if IFS= read -r -t 0.25 ignored <&3; then :; else
      state=$?
      if [ "$state" -le 128 ]; then : > "$lease_cancel"; break; fi
    fi
  done
) >/dev/null 2>&1 &
channel_watch=$!
exec 3<&-
admit_cache "$max_bytes" "$idle_seconds"
admitted=1
[ ! -f "$lease_cancel" ] || exit 143
: > "$control/$id.active"
echo locked
wait "$channel_watch"

"#
);

/// Held from before copying through the last artifact or shard operation.
pub struct MirrorLease {
    hold: MirrorLock,
}

impl MirrorLease {
    pub fn take(host: &BuildHost, mirror: &str, cfg: &OffloadConfig, tag: &str) -> Result<Self> {
        if cfg.cache_max_bytes == 0
            || cfg.cache_max_bytes > i64::MAX as u64
            || cfg.cache_idle_seconds == 0
            || cfg.cache_idle_seconds > i64::MAX as u64
        {
            bail!("offload cache byte and idle limits must be positive signed 64-bit values");
        }
        let bytes = cfg.cache_max_bytes.to_string();
        let idle = cfg.cache_idle_seconds.to_string();
        Ok(Self { hold: MirrorLock::hold(host, LEASE, &[mirror, &bytes, &idle], tag)? })
    }

    pub fn finish(&mut self) -> Result<()> {
        self.hold.finish()
    }
}

pub fn finish_leases(leases: &mut Vec<MirrorLease>) -> Result<()> {
    let mut first = None;
    for lease in leases.iter_mut() {
        if let Err(error) = lease.finish() {
            if first.is_none() {
                first = Some(error);
            }
        }
    }
    leases.clear();
    match first {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
