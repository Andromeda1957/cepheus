//! Dot's pet: the character the owner picked for their Dot in ChatGPT, animated beside its name the way ChatGPT
//! animates it.
//!
//! The Dot bridge keeps the Dot's `profile.json` (name, avatar kind, pet, picture key and any stored picture)
//! current from ChatGPT, so a new pet, icon or name shows up here by itself. Built-in pets come from OpenAI's
//! public asset host, the sheet ChatGPT's own web client loads, fetched at run time and never committed. Any
//! other picture is the still image the bridge stored.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::Read;
use std::path::Path;

/// Where chatgpt.com loads its assets from; no sign-in is needed.
const CDN: &str = "https://cdn.oaistatic.com/assets/";

/// The built-in pets and the sheet the ChatGPT client loads for each.
const SHEETS: &[(&str, &str)] = &[
    ("bsod", "bsod-spritesheet-v5.f4386b1066.webp"),
    ("codex", "codex-spritesheet-v6.922b99db4b.webp"),
    ("dewey", "dewey-spritesheet-v5.81cf696c13.webp"),
    ("fireball", "fireball-spritesheet-v5.864d94523f.webp"),
    ("hoots", "hoots-spritesheet-v8.52075ad356.webp"),
    ("null-signal", "null-signal-spritesheet-v7.e7af0b42f5.webp"),
    ("rocky", "rocky-spritesheet-v5.03e1d8b0f2.webp"),
    ("seedy", "seedy-spritesheet-v10.c613f3fce1.webp"),
    ("stacky", "stacky-spritesheet-v6.62c1fa318f.webp"),
];

/// The client's sheet layout: 8 columns of 192x208 cells, 9 rows (version 1)
/// or 11 (version 2 adds the look directions).
const COLUMNS: u32 = 8;
const CELL_WIDTH: u32 = 192;
const CELL_HEIGHT: u32 = 208;
const SHEET_MAX: u64 = 8 << 20;
const PICTURE_MAX: u64 = 4 << 20;
const PROFILE_MAX: u64 = 64 << 10;

/// What the bridge stores beside Dot's transcript.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Profile {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub pet: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
}

impl Profile {
    pub fn read(dir: &Path) -> Option<Profile> {
        let bytes = read_limited(&dir.join("profile.json"), PROFILE_MAX).ok()?;
        let mut profile: Profile = serde_json::from_slice(&bytes).ok()?;
        profile.name = profile
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty() && n.chars().count() <= 64 && !n.chars().any(char::is_control));
        profile.file = profile.file.filter(|f| matches!(f.as_str(), "avatar.png" | "avatar.webp"));
        Some(profile)
    }

    /// What the art depends on; a change loads it again.
    pub fn art_key(&self) -> Option<String> {
        if let Some(pet) = self.pet.as_deref().filter(|p| sheet(p).is_some()) {
            return Some(format!("pet:{pet}"));
        }
        let file = self.file.as_deref()?;
        Some(format!("picture:{file}:{}", self.image.as_deref().unwrap_or_default()))
    }
}

/// How Dot is doing, each shown with the ChatGPT client's own animation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    /// Off or ended: the first idle frame, unmoving.
    Still,
    /// Connected and idle: the client's slow blink.
    Ready,
    /// A turn is running: the pet at its laptop, as ChatGPT shows "Thinking".
    Working,
    /// Waiting on the owner: chin in hand.
    Waiting,
}

impl Mood {
    pub const ALL: [Mood; 4] = [Mood::Still, Mood::Ready, Mood::Working, Mood::Waiting];

    /// The sheet row and each frame's duration, from the client's animation
    /// table (its idle row slowed six times, as it plays it at rest).
    fn frames(self) -> (u32, &'static [u32]) {
        match self {
            Mood::Still => (0, &[0]),
            Mood::Ready => (0, &[1680, 660, 660, 840, 840, 1920]),
            Mood::Working => (7, &[120, 120, 120, 120, 120, 220]),
            Mood::Waiting => (6, &[150, 150, 150, 150, 150, 260]),
        }
    }
}

pub struct Frame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Milliseconds before the next frame.
    pub gap: u32,
}

pub enum Art {
    /// One animation per mood, in `Mood::ALL` order.
    Animated(Vec<Vec<Frame>>),
    /// A PNG the terminal decodes itself.
    Png(Vec<u8>),
    Still(Frame),
}

pub fn sheet(pet: &str) -> Option<&'static str> {
    SHEETS.iter().find(|(id, _)| *id == pet).map(|(_, file)| *file)
}

/// The art for a profile. It may download and decode, so callers run it off
/// the interface thread.
pub fn load(profile: &Profile, state_dir: &Path) -> Result<Art> {
    if let Some(name) = profile.pet.as_deref().and_then(sheet) {
        match fetch_sheet(name).and_then(|bytes| animate(&bytes)) {
            Ok(art) => return Ok(art),
            // A custom pet's still picture is better than nothing.
            Err(error) if profile.file.is_none() => return Err(error),
            Err(_) => {}
        }
    }
    let file = profile.file.as_deref().context("no picture")?;
    let bytes = read_limited(&state_dir.join(file), PICTURE_MAX)?;
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(Art::Png(bytes));
    }
    let (rgba, width, height) = decode_webp(&bytes)?;
    Ok(Art::Still(Frame { rgba, width, height, gap: 0 }))
}

fn fetch_sheet(name: &str) -> Result<Vec<u8>> {
    let dir = crate::paths::cache_dir().join("pets");
    let path = dir.join(name);
    if let Ok(bytes) = read_limited(&path, SHEET_MAX) {
        if is_webp(&bytes) {
            return Ok(bytes);
        }
    }
    let mut response = ureq::get(&format!("{CDN}{name}")).call().context("fetching Dot's pet")?;
    let bytes = response.body_mut().with_config().limit(SHEET_MAX).read_to_vec()?;
    if !is_webp(&bytes) {
        bail!("Dot's pet sheet is not a WebP image");
    }
    std::fs::create_dir_all(&dir)?;
    let partial = dir.join(format!(".{name}.{}", std::process::id()));
    std::fs::write(&partial, &bytes)?;
    std::fs::rename(&partial, &path)?;
    Ok(bytes)
}

fn animate(bytes: &[u8]) -> Result<Art> {
    let (rgba, width, height) = decode_webp(bytes)?;
    if width != COLUMNS * CELL_WIDTH || height % CELL_HEIGHT != 0 || !matches!(height / CELL_HEIGHT, 9 | 11) {
        bail!("unknown pet sheet layout {width}x{height}");
    }
    Ok(Art::Animated(
        Mood::ALL
            .iter()
            .map(|mood| {
                let (row, gaps) = mood.frames();
                gaps.iter().enumerate().map(|(column, gap)| cell(&rgba, width, column as u32, row, *gap)).collect()
            })
            .collect(),
    ))
}

/// One sheet cell at half size. The art is drawn in blocks of at least two
/// pixels, so keeping every other pixel stays crisp at a quarter of the bytes.
fn cell(rgba: &[u8], width: u32, column: u32, row: u32, gap: u32) -> Frame {
    let (w, h) = (CELL_WIDTH / 2, CELL_HEIGHT / 2);
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let at = (((row * CELL_HEIGHT + y * 2) * width + column * CELL_WIDTH + x * 2) * 4) as usize;
            out.extend_from_slice(&rgba[at..at + 4]);
        }
    }
    Frame { rgba: out, width: w, height: h, gap }
}

fn decode_webp(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes))?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        bail!("picture size {width}x{height} is out of range");
    }
    let mut buffer = vec![0; decoder.output_buffer_size().context("picture is too large")?];
    decoder.read_image(&mut buffer)?;
    let rgba = if decoder.has_alpha() {
        buffer
    } else {
        buffer.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect()
    };
    Ok((rgba, width, height))
}

fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

fn read_limited(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        bail!("{} is too large", path.display());
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "tests/pet.rs"]
mod tests;
