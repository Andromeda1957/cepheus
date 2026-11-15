//! A job's independent parent: gated execution and durable terminal evidence.
use super::{store, Exit, Identity, Intent, Launch, Spec};
use anyhow::{Context, Result};
use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn command(args: &[String], execute: bool) -> i32 {
    let result = (|| -> Result<()> {
        let dir = Path::new(args.first().context("job directory missing")?);
        store::check_directory(dir)?;
        if execute {
            execute_child(dir, args.get(1).context("job gate missing")?.parse()?)
        } else {
            supervise(dir)
        }
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("job supervisor: {error:#}");
            1
        }
    }
}
fn supervise(dir: &Path) -> Result<()> {
    let intent: Intent = store::read(&dir.join("intent.json"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join("run.lock"))?;
    // The private lock never reaches the workload or another exec.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(error.into());
    }
    if store::optional::<Exit>(&dir.join("exit.json"))?.is_some() {
        return Ok(());
    }
    if store::optional::<Launch>(&dir.join("launch.json"))?.is_some() {
        anyhow::bail!("previous supervisor lost after launch; automatic retry disabled")
    }
    // Recheck under run.lock: an acknowledged pause cannot race a daemon
    // which spawned this supervisor but has not published owner.json yet.
    if !super::policy::permitted(dir, &intent)? {
        return Ok(());
    }
    let owner = Identity::read(std::process::id())?;
    store::write(&dir.join("owner.json"), &owner)?;
    let stopped = || -> Result<bool> {
        Ok(store::optional::<String>(&dir.join("stop.json"))?.is_some_and(|nonce| nonce == intent.nonce))
    };
    if stopped()? {
        store::write(
            &dir.join("exit.json"),
            &Exit {
                nonce: intent.nonce,
                code: None,
                signal: None,
                finished_at: crate::util::now_secs(),
                stopped: true,
                error: Some("cancelled before execution".into()),
            },
        )?;
        return Ok(());
    }
    let mut gate = [-1; 2];
    anyhow::ensure!(unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) } == 0, "cannot create private job gate");
    struct Gate([i32; 2]);
    impl Drop for Gate {
        fn drop(&mut self) {
            for fd in self.0 {
                if fd >= 0 {
                    unsafe {
                        libc::close(fd);
                    }
                }
            }
        }
    }
    let mut gate = Gate(gate);
    let read = gate.0[0];
    let mut command = Command::new("/proc/self/exe");
    command
        .arg("__job-exec")
        .arg(dir)
        .arg(read.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::fcntl(read, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("cannot spawn gated job")?;
    unsafe {
        libc::close(gate.0[0]);
    }
    gate.0[0] = -1;
    let launch = Launch {
        nonce: intent.nonce.clone(),
        supervisor: owner,
        child: Identity::read(child.id())?,
        pgid: child.id(),
        started_at: crate::util::now_secs(),
    };
    if let Err(error) = store::write(&dir.join("launch.json"), &launch) {
        drop(gate);
        let _ = child.wait();
        return Err(error);
    }
    // No workload body can run before its exact identity and launch nonce are durable.
    if !stopped()? {
        let allowed = [1u8];
        unsafe {
            libc::write(gate.0[1], allowed.as_ptr().cast(), 1);
        }
    }
    drop(gate);
    let mut stopping = None;
    let mut killed = false;
    let observed = loop {
        if stopped()? && stopping.is_none() {
            stopping = Some(Instant::now());
            unsafe {
                libc::killpg(launch.pgid as i32, libc::SIGINT);
                libc::killpg(launch.pgid as i32, libc::SIGTERM);
            }
        }
        if stopping.is_some_and(|at| at.elapsed() >= Duration::from_secs(5)) && !killed {
            unsafe {
                libc::killpg(launch.pgid as i32, libc::SIGKILL);
            }
            let _ = child.kill();
            killed = true;
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result =
            unsafe { libc::waitid(libc::P_PID, child.id(), &mut info, libc::WEXITED | libc::WNOWAIT | libc::WNOHANG) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if unsafe { info.si_pid() } != 0 {
            break info;
        }
        thread::sleep(Duration::from_millis(25));
    };
    // Keep the zombie leader until group cleanup and the terminal receipt are durable.
    unsafe {
        libc::killpg(launch.pgid as i32, libc::SIGKILL);
    }
    let code = unsafe { observed.si_status() };
    let normal = observed.si_code == libc::CLD_EXITED;
    let exit = Exit {
        nonce: intent.nonce,
        code: normal.then_some(code),
        signal: (!normal).then_some(code),
        finished_at: crate::util::now_secs(),
        stopped: stopping.is_some(),
        error: None,
    };
    if let Ok(log) = store::open_log(&dir.join("output.log")) {
        let _ = log.sync_all();
    }
    // Storage failure must not throw away the only exact wait status.
    while store::write(&dir.join("exit.json"), &exit).is_err() {
        thread::sleep(Duration::from_millis(250));
    }
    let _ = child.wait();
    Ok(())
}
fn execute_child(dir: &Path, gate: i32) -> Result<()> {
    let mut allowed = [0u8];
    let read = unsafe { libc::read(gate, allowed.as_mut_ptr().cast(), 1) };
    unsafe {
        libc::close(gate);
    }
    anyhow::ensure!(read == 1 && allowed[0] == 1, "job gate closed before execution");
    let mut spec: Spec = store::read(&dir.join("execution.json"))?;
    spec.validate()?;
    spec.cwd = spec.cwd.canonicalize().context("job working directory unavailable")?;
    anyhow::ensure!(spec.cwd.is_dir(), "job working directory must be a directory");
    let log = store::append_log(&dir.join("output.log"))?;
    let mut input = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join("input"))?;
    let text = if spec.provider.is_some() && !spec.stdin.is_empty() {
        crate::keys::non_owner_message(&spec.stdin)
    } else {
        spec.stdin.clone()
    };
    input.write_all(text.as_bytes())?;
    input.seek(SeekFrom::Start(0))?;
    let mut command = Command::new(&spec.argv[0]);
    command.args(&spec.argv[1..]).current_dir(&spec.cwd).stdin(input).stdout(log.try_clone()?).stderr(log);
    if spec.env_clear {
        let kept: Vec<_> =
            spec.env_allowlist.iter().filter_map(|name| std::env::var_os(name).map(|value| (name, value))).collect();
        command.env_clear();
        command.envs(kept);
    }
    for suffix in ["SESSION", "ACCOUNT", "ACCOUNT_DIRECTORY", "ACCOUNT_ID"] {
        for name in crate::paths::env_names(suffix) {
            command.env_remove(name);
        }
    }
    // The job's own temporary folder (`client_temp.rs`). A cleared
    // environment keeps the owner's Java options only when it lists them.
    let java = std::env::var("JAVA_TOOL_OPTIONS")
        .ok()
        .filter(|_| !spec.env_clear || spec.env_allowlist.iter().any(|name| name == "JAVA_TOOL_OPTIONS"));
    command.envs(crate::client_temp::vars_with("job", java)?);
    command.envs(&spec.env);
    let error = command.exec();
    Err(error.into())
}
