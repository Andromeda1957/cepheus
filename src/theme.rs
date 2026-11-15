//! Deck colours derived from the outer terminal's palette, so the sidebar,
//! gauges and dialogs match the kitty theme on dark and light backgrounds.
//!
//! Semantic colours are ANSI palette indices, which the terminal draws in its
//! own theme (and keeps current when the theme changes); each picks whichever
//! of the normal or bright variant reads better on the background. Subtle
//! shades (muted text, separators, selection) are blends of the theme's own
//! foreground and background.

use ratatui::style::{Color, Modifier, Style};

use crate::config::{Palette, Rgb8};

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub accent: Color,
    /// Secondary text: labels, hints, timings.
    pub dim: Color,
    /// Separators, gauge tracks and ended sessions.
    pub faint: Color,
    pub selected_bg: Color,
    /// An overlay window's background, a shade lifted from the screen's so
    /// the window reads as in front of it.
    pub surface: Color,
    /// Text drawn on a coloured background (the terminal background colour).
    pub on_color: Color,
    pub claude: Color,
    pub codex: Color,
    pub antigravity: Color,
    pub grok: Color,
    pub opencode: Color,
    pub working: Color,
    pub needs: Color,
    pub finished: Color,
    pub task: Color,
    pub review: Color,
    pub ok: Color,
    pub warn: Color,
    pub bad: Color,
}

fn linear(v: u8) -> f64 {
    let c = f64::from(v) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance.
pub fn luminance(c: Rgb8) -> f64 {
    0.2126 * linear(c.0) + 0.7152 * linear(c.1) + 0.0722 * linear(c.2)
}

/// WCAG contrast ratio between two colours (1 to 21).
pub fn contrast(a: Rgb8, b: Rgb8) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `from` moved `t` (0 to 1) of the way towards `to`.
pub fn mix(from: Rgb8, to: Rgb8, t: f64) -> Rgb8 {
    let m = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round().clamp(0.0, 255.0) as u8;
    (m(from.0, to.0), m(from.1, to.1), m(from.2, to.2))
}

fn rgb(c: Rgb8) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// The normal (`n`) or bright (`n + 8`) ANSI colour with more contrast.
fn ansi(p: &Palette, n: u8, bg: Rgb8) -> Color {
    let normal = contrast(p.get(usize::from(n)), bg);
    let bright = contrast(p.get(usize::from(n) + 8), bg);
    Color::Indexed(if bright > normal { n + 8 } else { n })
}

/// The quietest blend of `bg` towards `fg` that still reaches `target`
/// contrast against `bg` (or `fg` itself when the theme cannot).
fn muted(bg: Rgb8, fg: Rgb8, target: f64) -> Rgb8 {
    (55..=100).map(|step| mix(bg, fg, f64::from(step) / 100.0)).find(|c| contrast(*c, bg) >= target).unwrap_or(fg)
}

impl Theme {
    pub fn from_palette(p: &Palette) -> Theme {
        let fg = p.get(256);
        let bg = p.get(257);
        let dark = luminance(bg) <= luminance(fg);
        Theme {
            dark,
            accent: ansi(p, 4, bg),
            dim: rgb(muted(bg, fg, 4.0)),
            faint: rgb(mix(bg, fg, 0.24)),
            selected_bg: rgb(mix(bg, fg, 0.13)),
            surface: rgb(mix(bg, fg, 0.045)),
            on_color: rgb(bg),
            // Claude's terracotta, deepened on light backgrounds for contrast.
            claude: rgb(if dark { (0xd9, 0x77, 0x57) } else { (0xa8, 0x4b, 0x2a) }),
            codex: ansi(p, 6, bg),
            antigravity: ansi(p, 12, bg),
            // Grok's mark is monochrome: the theme's own foreground.
            grok: rgb(fg),
            opencode: ansi(p, 13, bg),
            working: ansi(p, 3, bg),
            needs: ansi(p, 1, bg),
            finished: ansi(p, 2, bg),
            task: ansi(p, 5, bg),
            review: ansi(p, 4, bg),
            ok: ansi(p, 2, bg),
            warn: ansi(p, 3, bg),
            bad: ansi(p, 1, bg),
        }
    }

    pub fn load(cfg: &crate::config::Config) -> Theme {
        Theme::from_palette(&Palette::load(cfg))
    }

    pub fn dim(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn faint(&self) -> Style {
        Style::default().fg(self.faint)
    }

    pub fn accent(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn bold(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// A label drawn on a coloured block (mode tags, banners).
    pub fn tag(&self, bg: Color) -> Style {
        Style::default().fg(self.on_color).bg(bg).add_modifier(Modifier::BOLD)
    }

    pub fn tool(&self, tool: crate::protocol::Tool) -> Color {
        match tool {
            crate::protocol::Tool::Claude => self.claude,
            crate::protocol::Tool::Codex => self.codex,
            crate::protocol::Tool::Antigravity => self.antigravity,
            crate::protocol::Tool::Grok => self.grok,
            crate::protocol::Tool::OpenCode => self.opencode,
            crate::protocol::Tool::Shell => self.dim,
            crate::protocol::Tool::Dynamic(_) => self.accent,
        }
    }

    /// Gauge colour for a usage percentage.
    pub fn level(&self, percent: f64) -> Color {
        if percent >= 85.0 {
            self.bad
        } else if percent >= 60.0 {
            self.warn
        } else {
            self.ok
        }
    }
}

#[cfg(test)]
#[path = "tests/theme.rs"]
mod tests;
