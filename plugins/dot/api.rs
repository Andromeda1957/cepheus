//! Direct, private owner-message delivery to the existing Dot controller.
//! No terminal input, login material, signed URLs or executable paths cross it.
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const IMAGE_MAX: usize = 20 << 20;
pub const REQUEST_MAX: usize = 29 << 20;
const SOCKET: &str = "control.sock";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub name: String,
    pub data: String,
}

impl Image {
    pub fn new(name: &str, bytes: &[u8]) -> Result<Self, String> {
        let image = Self { name: name.into(), data: crate::util::base64(bytes) };
        image.bytes()?;
        Ok(image)
    }

    pub fn from_file(path: &Path) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| "The attached image is unavailable.")?;
        let meta = file.metadata().map_err(|_| "The attached image is unavailable.")?;
        if !meta.is_file() || meta.len() > IMAGE_MAX as u64 {
            return Err("Dot images may be at most 20 MiB.".into());
        }
        let mut bytes = Vec::new();
        file.take(IMAGE_MAX as u64 + 1).read_to_end(&mut bytes).map_err(|_| "Could not read the attached image.")?;
        Self::new(path.file_name().and_then(|n| n.to_str()).unwrap_or("image"), &bytes)
    }

    pub fn bytes(&self) -> Result<(Vec<u8>, &'static str), String> {
        if self.name.is_empty()
            || self.name.len() > 100
            || self.name.chars().any(|c| c.is_control() || "/\\\"".contains(c))
            || self.data.len() > IMAGE_MAX.div_ceil(3) * 4
        {
            return Err("Invalid Dot image or name; images may be at most 20 MiB.".into());
        }
        let bytes = crate::util::unbase64(&self.data)
            .filter(|b| !b.is_empty() && b.len() <= IMAGE_MAX)
            .ok_or("Invalid Dot image; images may be at most 20 MiB.")?;
        let media = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            return Err("Dot accepts PNG, JPEG, GIF and WebP images.".into());
        };
        Ok((bytes, media))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub text: String,
    #[serde(default)]
    pub images: Vec<Image>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    Reconnect,
    Resolve,
    Quit,
}
impl Control {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "/reconnect" => Some(Self::Reconnect),
            "/resolve" => Some(Self::Resolve),
            "/quit" => Some(Self::Quit),
            _ => None,
        }
    }
}
pub fn acknowledged(data: &Value, control: Option<Control>) -> bool {
    match control {
        Some(expected) => serde_json::from_value::<Control>(data["control"].clone()).ok() == Some(expected),
        None => data["id"].as_str().is_some_and(|id| !id.is_empty()),
    }
}
impl Message {
    pub fn control(&self) -> Option<Control> {
        if !self.images.is_empty() {
            return None;
        }
        Control::parse(&self.text)
    }
}
impl Message {
    pub fn validate(&self) -> Result<(), String> {
        if self.text.chars().count() > 32_000
            || self.text.contains('\0')
            || self.text.trim().is_empty() && self.images.is_empty()
            || self.images.len() > 10
        {
            return Err("Write a message or attach an image (at most 32000 characters and 10 images).".into());
        }
        let mut size = 0;
        for image in &self.images {
            size += image.bytes()?.0.len();
        }
        if size > IMAGE_MAX {
            return Err("Dot attachments may total at most 20 MiB per message.".into());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    controller: u32,
    expires: u64,
    message: Message,
}

#[derive(Clone)]
pub struct OwnerRequest {
    socket: PathBuf,
    controller: u32,
    start_ticks: u64,
    message: Message,
}
impl OwnerRequest {
    pub fn prepare(provider: &crate::providers::Provider, pid: u32, text: &str) -> Result<Self, String> {
        Self::with_images(provider, pid, Message { text: text.into(), images: Vec::new() })
    }
    pub fn with_images(provider: &crate::providers::Provider, pid: u32, message: Message) -> Result<Self, String> {
        message.validate()?;
        if provider.id != "dot" || pid <= 1 {
            return Err("The Dot controller identity is unavailable.".into());
        }
        let dir = provider.chat_snapshot.as_ref().and_then(|p| p.parent()).ok_or("The Dot snapshot is unavailable.")?;
        private(dir, false)?;
        let socket = dir.join(SOCKET);
        private(&socket, true).map_err(|_| {
            "Dot needs the updated native controller for direct sending; the draft and images are kept.".to_string()
        })?;
        let start_ticks = crate::procs::stat(pid).ok_or("The Dot controller is unavailable.")?.start_ticks;
        Ok(Self { socket, controller: pid, start_ticks, message })
    }
    pub fn deliver(&self, deadline: Instant, cancel: &AtomicBool) -> Result<Value, String> {
        if !crate::procs::alive_with_start(self.controller, Some(self.start_ticks)) {
            return Err("The Dot controller changed; nothing was sent.".into());
        }
        let mut stream = crate::remote::streaming::connect_local(&self.socket, deadline, cancel)
            .map_err(|_| "Dot is disconnected or delivery was canceled; the draft and images are kept.")?;
        let (uid, pid) = peer(&stream)?;
        // SAFETY: getuid has no preconditions.
        if uid != unsafe { libc::getuid() }
            || pid != self.controller
            || !crate::procs::alive_with_start(self.controller, Some(self.start_ticks))
        {
            return Err("The Dot controller changed; nothing was sent.".into());
        }
        if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return Err("Dot delivery was canceled before submission.".into());
        }
        stream.set_write_timeout(Some(Duration::from_secs(2))).map_err(|_| "Dot connection unavailable.")?;
        let remaining = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(120));
        let expires = crate::util::now_ms().saturating_add(remaining.as_millis() as u64);
        write_frame(&mut stream, &Request { controller: self.controller, expires, message: self.message.clone() })
            .map_err(|_| "Dot delivery status is unknown. Check the conversation before retrying.")?;
        stream.set_read_timeout(Some(Duration::from_millis(100))).map_err(|_| "Dot connection unavailable.")?;
        let mut bytes = Vec::new();
        let mut wanted = None;
        loop {
            if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
                return Err("Dot delivery status is unknown. Check the conversation before retrying; nothing is resent automatically.".into());
            }
            let mut part = [0; 4096];
            match stream.read(&mut part) {
                Ok(0) => return Err("Dot delivery status is unknown. Check the conversation before retrying.".into()),
                Ok(n) => bytes.extend_from_slice(&part[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Err(_) => return Err("Dot delivery status is unknown. Check the conversation before retrying.".into()),
            }
            if bytes.len() >= 4 && wanted.is_none() {
                let n = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                if n > 16 * 1024 {
                    return Err("Invalid Dot delivery response.".into());
                }
                wanted = Some(n + 4);
            }
            if let Some(n) = wanted.filter(|n| bytes.len() >= *n) {
                let response: Value =
                    serde_json::from_slice(&bytes[4..n]).map_err(|_| "Invalid Dot delivery response.")?;
                return if let Some(error) = response["error"].as_str() {
                    Err(error.into())
                } else if acknowledged(&response, self.message.control()) {
                    Ok(response)
                } else {
                    Err("Dot delivery acknowledgement is uncertain. Check the conversation.".into())
                };
            }
        }
    }
}

fn private(path: &Path, socket: bool) -> Result<(), String> {
    let m = fs::symlink_metadata(path).map_err(|_| "Dot private state is unavailable.")?;
    // SAFETY: getuid has no preconditions.
    if m.uid() != unsafe { libc::getuid() }
        || m.mode() & 0o077 != 0
        || if socket { !m.file_type().is_socket() } else { !m.is_dir() }
    {
        return Err("Dot private state ownership or permissions are invalid.".into());
    }
    Ok(())
}
fn peer(stream: &UnixStream) -> Result<(u32, u32), String> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the kernel fills the correctly sized credentials of this socket.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    } != 0
    {
        return Err("Could not verify the Dot connection owner.".into());
    }
    Ok((cred.uid, cred.pid as u32))
}
fn write_frame(stream: &mut UnixStream, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "Invalid Dot request.")?;
    if bytes.len() > REQUEST_MAX {
        return Err("Dot request is too large.".into());
    }
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .and_then(|()| stream.write_all(&bytes))
        .map_err(|_| "Dot connection unavailable.".into())
}

pub struct Call {
    pub message: Message,
    pub deadline: Instant,
    pub reply: mpsc::Sender<Result<Value, String>>,
}
pub struct Server {
    thread: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    wake: Arc<super::Wake>,
}
fn serve(mut stream: UnixStream, dispatch: &(dyn Fn(Call) -> bool + Send + Sync), controller_stop: &AtomicBool) {
    // SAFETY: getuid has no preconditions.
    if !peer(&stream).is_ok_and(|(uid, _)| uid == unsafe { libc::getuid() }) {
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let result = (|| -> Result<Value, String> {
        let mut head = [0; 4];
        stream.read_exact(&mut head).map_err(|_| "Incomplete Dot request.")?;
        let size = u32::from_be_bytes(head) as usize;
        if size > REQUEST_MAX {
            return Err("Dot request is too large.".into());
        }
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).map_err(|_| "Incomplete Dot request.")?;
        let request: Request = serde_json::from_slice(&bytes).map_err(|_| "Invalid Dot request.")?;
        if request.controller != std::process::id() {
            return Err("The Dot controller changed; nothing was sent.".into());
        }
        request.message.validate()?;
        let remaining = request.expires.saturating_sub(crate::util::now_ms()).min(120_000);
        if remaining == 0 {
            return Err("Dot request expired before submission; the draft and images are kept.".into());
        }
        let deadline = Instant::now() + Duration::from_millis(remaining);
        let (reply, receive) = mpsc::channel();
        if !dispatch(Call { message: request.message, deadline, reply }) {
            return Err("Dot is unavailable; nothing was sent.".into());
        }
        // The controller answers every call it takes, and drops the rest when it closes.
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| Err("Dot delivery status is unknown. Check the conversation.".into()))
    })();
    let response = result.unwrap_or_else(|error| json!({"error":error}));
    let quitting = response["control"] == json!(Control::Quit);
    let _ = write_frame(&mut stream, &response);
    if quitting {
        controller_stop.store(true, Ordering::SeqCst);
    }
}

impl Server {
    /// The caller already holds the controller lock for this directory.
    pub fn start(
        directory: &Path,
        controller_stop: Arc<AtomicBool>,
        dispatch: impl Fn(Call) -> bool + Send + Sync + 'static,
    ) -> Result<Self, String> {
        private(directory, false)?;
        let path = directory.join(SOCKET);
        if path.exists() {
            private(&path, true)?;
            fs::remove_file(&path).map_err(|_| "Could not replace the stale Dot socket.")?;
        }
        let listener = UnixListener::bind(&path).map_err(|_| "Could not bind the private Dot socket.")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "Could not protect the Dot socket.")?;
        listener.set_nonblocking(true).map_err(|_| "Could not listen for Dot messages.")?;
        let inode = fs::symlink_metadata(&path).map_err(|_| "Dot socket unavailable.")?.ino();
        let wake = Arc::new(super::Wake::new().map_err(|_| "Could not listen for Dot messages.")?);
        let woken = Arc::clone(&wake);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let dispatch: Arc<dyn Fn(Call) -> bool + Send + Sync> = Arc::new(dispatch);
        let thread = std::thread::spawn(move || {
            let mut handlers: Vec<std::thread::JoinHandle<()>> = Vec::new();
            loop {
                let mut ready = [
                    libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: woken.descriptor(), events: libc::POLLIN, revents: 0 },
                ];
                // SAFETY: two initialized pollfds borrowed for one wait, which
                // ends on a connection or when the server is dropped.
                let polled = unsafe { libc::poll(ready.as_mut_ptr(), 2, -1) };
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                if polled < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
                for index in (0..handlers.len()).rev() {
                    if handlers[index].is_finished() {
                        let _ = handlers.swap_remove(index).join();
                    }
                }
                let (stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {
                        continue;
                    }
                    Err(_) => break,
                };
                if handlers.len() >= 8 {
                    continue;
                }
                let dispatch = Arc::clone(&dispatch);
                let controller_stop = Arc::clone(&controller_stop);
                handlers.push(std::thread::spawn(move || serve(stream, &*dispatch, &controller_stop)));
            }
            for handler in handlers {
                let _ = handler.join();
            }
            if fs::symlink_metadata(&path).is_ok_and(|m| m.ino() == inode && m.file_type().is_socket()) {
                let _ = fs::remove_file(path);
            }
        });
        Ok(Self { thread: Some(thread), stop, wake })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[path = "../../src/tests/dot_api.rs"]
mod tests;
