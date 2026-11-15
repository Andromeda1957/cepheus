//! One invocation's source and caches belong to one disposable container.
//! The independent SSH stdin is its lifetime channel, including caller death.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{hosts::BuildHost, remote_command, ssh_command, valid_assignment};
use crate::config::OffloadContainer;

pub const SOURCE: &str = "/scratch/source";
#[cfg(test)]
pub const STATE: &str = "/scratch/source/.cepheus-offload/state";

// No host payload directory: rsync writes straight into this anonymous volume.
// Only the exact fresh container, with its nonce label, may be removed.
pub(crate) const START: &str = r#"set -euo pipefail
name=$1; image=$2; sdk=$3; shift 3
[[ "$name" =~ ^cepheus-offload-[a-f0-9]{32}$ ]] || exit 125
if podman --remote=false container exists "$name"; then
  echo '[offload] invocation container already exists; preserved' >&2; exit 125
else
  result=$?; [ "$result" = 1 ] || exit "$result"
fi
cid=; volume=; creator=$$
absent() {
  if podman --remote=false "$1" exists "$2"; then return 1; else
    result=$?; [ "$result" = 1 ] || exit 125
  fi
}
cleanup() {
  status=$?
  trap - EXIT HUP INT TERM
  if ! absent container "$name"; then
    found=$(podman --remote=false container inspect --format '{{.Id}} {{index .Config.Labels "io.cepheus.offload"}} {{index .Config.Labels "io.cepheus.offload.supervisor"}}' "$name") || exit 125
    read -r id label owner <<< "$found"
    if [[ ! "$id" =~ ^[a-f0-9]{64}$ ]] || [ "$label" != "$name" ] || [ "$owner" != "$creator" ] || { [ -n "$cid" ] && [ "$id" != "$cid" ]; }; then
      echo '[offload] container ownership changed; preserved' >&2
      exit 125
    fi
    volume=$(podman --remote=false container inspect --format '{{range .Mounts}}{{if eq .Destination "/scratch"}}{{.Name}}{{end}}{{end}}' "$id") || exit 125
    [[ "$volume" =~ ^[a-f0-9]{64}$ ]] || exit 125
    podman --remote=false rm --force --volumes "$id" >&2 || exit 125
    absent container "$id" || exit 125
  fi
  if [ -n "$volume" ]; then
    absent volume "$volume" || podman --remote=false volume rm "$volume" >&2 || exit 125
    absent volume "$volume" || exit 125
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
podman --remote=false image exists "$image" || { echo "[offload] provision offload.container.image on this host: $image" >&2; exit 125; }
options=()
if [ -n "$sdk" ]; then
  [[ "$sdk" = /* && "$sdk" != *[,$'\n'$'\r']* ]] || exit 125
  [ "$(realpath -e -- "$sdk")" = "$sdk" ] || exit 125
  [ -d "$sdk/platforms" ] && [ -d "$sdk/build-tools" ] && [ -d "$sdk/licenses" ] || exit 125
  options+=(--mount "type=bind,src=$sdk,dst=/opt/android-sdk,ro,relabel=shared" --env ANDROID_HOME=/opt/android-sdk --env ANDROID_SDK_ROOT=/opt/android-sdk)
fi
for assignment in "$@"; do options+=(--env "$assignment"); done
cap=$(( $(nproc) / 2 )); [ "$cap" -ge 1 ] || cap=1
cid=$(podman --remote=false run --detach --rm --init --pull=never --timeout=28800 \
  --name "$name" --label "io.cepheus.offload=$name" --label "io.cepheus.offload.supervisor=$creator" \
  --userns=keep-id --user "$(id -u):$(id -g)" --mount type=volume,destination=/scratch,U=true \
  --read-only --read-only-tmpfs=false --cap-drop=ALL --security-opt=no-new-privileges \
  --pids-limit=2048 --memory=4g --memory-swap=6g --cpus "$cap" \
  "${options[@]}" \
  --env HOME=/scratch/home --env TMPDIR=/scratch/tmp --env TMP=/scratch/tmp --env TEMP=/scratch/tmp \
  --env XDG_CACHE_HOME=/scratch/cache --env CARGO_HOME=/scratch/cargo --env CARGO_TARGET_DIR=/scratch/target \
  --env UV_CACHE_DIR=/scratch/cache/uv --env UV_PYTHON_INSTALL_DIR=/scratch/python \
  --env PIP_CACHE_DIR=/scratch/cache/pip --env GRADLE_USER_HOME=/scratch/gradle \
  --env npm_config_cache=/scratch/cache/npm --env RUSTUP_HOME=/scratch/rustup \
  --entrypoint=/bin/sh "$image" -c 'trap "exit 0" TERM INT HUP; while :; do sleep 60 & wait $!; done')
[[ "$cid" =~ ^[a-f0-9]{64}$ ]] || exit 125
podman --remote=false exec "$cid" mkdir -p /scratch/source/.cepheus-offload/state /scratch/home /scratch/tmp /scratch/cache
root=$(podman --remote=false container inspect --format '{{range .Mounts}}{{if eq .Destination "/scratch"}}{{.Source}}{{end}}{{end}}' "$cid")
volume=$(podman --remote=false container inspect --format '{{range .Mounts}}{{if eq .Destination "/scratch"}}{{.Name}}{{end}}{{end}}' "$cid")
podman --remote=false unshare test -d "$root/source"
podman --remote=false unshare test ! -L "$root"
python3 -c 'import json,sys;print(json.dumps({"id":sys.argv[1],"root":sys.argv[2]}),flush=True)' "$cid" "$root"
cat >/dev/null
"#;

#[derive(Deserialize)]
struct Ready {
    id: String,
    root: String,
}

pub struct Lease {
    pub host: BuildHost,
    pub name: String,
    pub source: String,
    lifetime: Mutex<Option<(Child, ChildStdin)>>,
}

impl Lease {
    pub fn start(host: &BuildHost, name: &str, config: &OffloadContainer) -> Result<Self> {
        if config.image.is_empty() || config.image.starts_with('-') || config.image.contains(char::is_whitespace) {
            bail!("offload.container.image must name a locally provisioned tools image");
        }
        if config.environment.iter().any(|value| !valid_assignment(value)) {
            bail!("offload.container.environment takes NAME=VALUE assignments");
        }
        if !config.android_sdk.is_empty()
            && (!config.android_sdk.starts_with('/')
                || config.android_sdk.contains([',', '\n', '\r'])
                || std::path::Path::new(&config.android_sdk)
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir)))
        {
            bail!("offload.container.android_sdk must be a canonical absolute public SDK directory");
        }
        let mut args = vec![name, config.image.as_str(), config.android_sdk.as_str()];
        args.extend(config.environment.iter().map(String::as_str));
        let mut child = ssh_command(host)
            .arg(remote_command(START, &args))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting offload container supervisor")?;
        let hold = child.stdin.take().expect("piped lifetime");
        let mut output = BufReader::new(child.stdout.take().expect("piped readiness"));
        let mut line = String::new();
        let ready = output
            .read_line(&mut line)
            .context("reading offload container readiness")
            .and_then(|_| serde_json::from_str::<Ready>(&line).context("offload container did not become ready"));
        let ready = match ready {
            Ok(ready)
                if ready.id.len() == 64
                    && ready.id.bytes().all(|b| b.is_ascii_hexdigit())
                    && ready.root.starts_with('/')
                    && !ready.root.contains(['\n', '\r']) =>
            {
                ready
            }
            _ => {
                drop(hold);
                let _ = child.wait();
                bail!("{}: offload container did not become ready", host.name);
            }
        };
        Ok(Self {
            host: host.clone(),
            name: name.into(),
            source: format!("{}/source", ready.root),
            lifetime: Mutex::new(Some((child, hold))),
        })
    }

    pub fn finish(&self) -> Result<()> {
        if let Some((mut child, hold)) = self.lifetime.lock().expect("lease mutex").take() {
            drop(hold);
            let status = child.wait().context("waiting for offload storage cleanup")?;
            if !status.success() {
                bail!("{}: offload storage cleanup failed ({status})", self.host.name);
            }
        }
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("[offload] {error:#}");
        }
    }
}
