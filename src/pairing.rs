//! The deck's pairing screen for the phone app (`Alt+p` from the list): a one-time code, shown as a QR code and
//! as text with the gateway's address and certificate fingerprint, and the paired phones, any of which can be
//! removed. The code stays open for `store::PAIRING_SECS` and closes with the screen.

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use qrcode::{EcLevel, QrCode};

use crate::config::Config;
use crate::remote::store::{self, Device};

/// Light modules around the code, which scanners need to find its edge.
const QUIET: usize = 3;

/// The pairing scheme. It keeps its name from before the rename to Cepheus,
/// the only one a phone with an earlier app accepts; the app reads
/// `cepheus://` too.
const SCHEME: &str = "agentdeck"; // legacy-name

/// What the phone scans: the gateway's address, the SHA-256 it pins and the
/// code. The app reads the same four fields when they are typed. The scheme
/// keeps its name from before the rename to Cepheus, the only one a phone
/// with an earlier app accepts; the app reads `cepheus://` too.
pub fn payload(host: &str, port: u16, fingerprint: &str, code: &str) -> String {
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    let code = store::normalize_code(code);
    format!("{SCHEME}://pair?host={host}&port={port}&fp={fingerprint}&code={code}")
}

/// The QR code's modules with a quiet zone, a row at a time; `true` is dark.
pub fn qr_modules(data: &str) -> Result<Vec<Vec<bool>>, String> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L).map_err(|e| format!("QR code: {e}"))?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 2 * QUIET;
    let mut rows = vec![vec![false; size]; size];
    for y in 0..width {
        for x in 0..width {
            rows[y + QUIET][x + QUIET] = colors[y * width + x] == qrcode::Color::Dark;
        }
    }
    Ok(rows)
}

/// Two module rows to a text row, drawn dark on light with half blocks, so
/// the code keeps square modules in a terminal.
pub fn half_blocks(modules: &[Vec<bool>]) -> Vec<String> {
    modules
        .chunks(2)
        .map(|pair| {
            let bottom = pair.get(1);
            pair[0]
                .iter()
                .enumerate()
                .map(|(x, &top)| match (top, bottom.is_some_and(|row| row[x])) {
                    (false, false) => ' ',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (true, true) => '█',
                })
                .collect()
        })
        .collect()
}

/// A code the phone can use now.
#[derive(Debug)]
pub struct Offer {
    /// As shown: two groups of four.
    pub code: String,
    pub address: String,
    pub fingerprint: String,
    pub expires: u64,
    pub modules: Vec<Vec<bool>>,
}

pub struct Pairing {
    dir: PathBuf,
    /// The open code, or why there is none.
    pub offer: Result<Offer, String>,
    pub devices: Vec<Device>,
    pub sel: usize,
}

impl Pairing {
    /// Open the screen: a fresh code when the gateway can take one.
    pub fn open(dir: &Path, config: &Config, now: u64) -> Pairing {
        let mut pairing =
            Pairing { dir: dir.to_path_buf(), offer: Err(String::new()), devices: store::load_devices(dir), sel: 0 };
        pairing.renew(config, now);
        pairing
    }

    /// A new code in place of the one shown.
    pub fn renew(&mut self, config: &Config, now: u64) {
        store::clear_pairing(&self.dir);
        self.offer = offer(&self.dir, config, now);
    }

    pub fn expired(&self, now: u64) -> bool {
        self.offer.as_ref().is_ok_and(|o| now >= o.expires)
    }

    /// Forget the highlighted phone; the gateway disconnects it.
    pub fn remove_selected(&mut self) -> io::Result<Option<Device>> {
        if self.sel >= self.devices.len() {
            return Ok(None);
        }
        // Remove from what is on disk now, which the gateway may have
        // changed (a phone's last-seen time) since the screen opened, under
        // the lock it takes for its own changes (ROOT-035).
        let target = self.devices[self.sel].id.clone();
        let devices = store::update_devices(&self.dir, |devices| {
            devices.retain(|d| d.id != target);
            true
        })?;
        let removed = self.devices.remove(self.sel);
        self.devices = devices;
        self.sel = self.sel.min(self.devices.len().saturating_sub(1));
        Ok(Some(removed))
    }

    pub fn select(&mut self, step: isize) {
        let last = self.devices.len().saturating_sub(1) as isize;
        self.sel = (self.sel as isize + step).clamp(0, last.max(0)) as usize;
    }

    /// Close the screen, and with it the code.
    pub fn close(&self) {
        store::clear_pairing(&self.dir);
    }
}

fn offer(dir: &Path, config: &Config, now: u64) -> Result<Offer, String> {
    if !config.remote {
        return Err("The phone gateway is off. Set \"remote\": true in the config file and open this again in a few \
                    seconds; the service starts it."
            .into());
    }
    let Some(fingerprint) = store::fingerprint(dir) else {
        return Err("The phone gateway has not started yet: it makes its certificate when it first starts. Open \
                    this again in a few seconds, or see remote.log in the deck's state folder."
            .into());
    };
    let (host, port) = config
        .remote_listen
        .rsplit_once(':')
        .and_then(|(h, p)| Some((h.trim_matches(['[', ']']).to_string(), p.parse::<u16>().ok()?)))
        .ok_or_else(|| format!("remote_listen \"{}\" is not an address and port.", config.remote_listen))?;
    if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified()) {
        return Err(format!(
            "remote_listen is {host}: set it to the address the phone reaches, such as 198.51.100.102."
        ));
    }
    let code = store::new_code().map_err(|e| format!("{e:#}"))?;
    let modules = qr_modules(&payload(&host, port, &fingerprint, &code))?;
    store::write_pairing(dir, &code, now).map_err(|e| format!("Could not open a pairing code: {e}"))?;
    Ok(Offer {
        code: store::display_code(&code),
        address: format!("{host}:{port}"),
        fingerprint,
        expires: now + store::PAIRING_SECS,
        modules,
    })
}

#[cfg(test)]
#[path = "tests/pairing.rs"]
mod tests;
