//! Markdown rendered to styled lines that are wrapped later at the width
//! they are drawn: Dot's replies.
use super::*;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

pub(super) fn clean(text: &str) -> String {
    text.replace('\t', "    ").chars().filter(|c| !c.is_control()).collect()
}

/// One line before wrapping: what starts its first row, what starts the
/// rows it wraps onto, and its text.
#[derive(Default)]
pub(super) struct Logical {
    pub lead: Vec<Span<'static>>,
    pub hang: Vec<Span<'static>>,
    pub spans: Vec<Span<'static>>,
}

impl Logical {
    pub fn blank(&self) -> bool {
        self.lead.is_empty() && self.spans.iter().all(|s| s.content.trim().is_empty())
    }
}

struct Markdown<'a> {
    t: &'a Theme,
    out: Vec<Logical>,
    line: Option<Logical>,
    bold: u8,
    italic: u8,
    strike: u8,
    heading: bool,
    code: bool,
    quote: usize,
    lists: Vec<Option<u64>>,
    link: bool,
}

impl Markdown<'_> {
    fn style(&self) -> Style {
        let mut s = if self.heading { self.t.bold().fg(self.t.accent) } else { plain() };
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link {
            s = s.add_modifier(Modifier::UNDERLINED);
        }
        s
    }

    /// Quote bars and list indents, with the item's bullet on its first row
    /// and the same width of space under it.
    fn prefix(&self, bullet: Option<String>) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let (mut lead, mut hang) = (Vec::new(), Vec::new());
        if self.quote > 0 {
            let bar = Span::styled("│ ".repeat(self.quote), self.t.dim());
            lead.push(bar.clone());
            hang.push(bar);
        }
        if !self.lists.is_empty() {
            let indent = "  ".repeat(self.lists.len() - 1);
            let mark = bullet.unwrap_or_else(|| "  ".into());
            hang.push(Span::raw(format!("{indent}{}", " ".repeat(mark.width()))));
            lead.push(Span::raw(format!("{indent}{mark}")));
        }
        (lead, hang)
    }

    fn start(&mut self, bullet: Option<String>) {
        let (lead, hang) = self.prefix(bullet);
        self.line = Some(Logical { lead, hang, spans: Vec::new() });
    }

    fn push(&mut self, text: &str, style: Style) {
        if self.line.is_none() {
            self.start(None);
        }
        if let Some(line) = &mut self.line {
            line.spans.push(Span::styled(clean(text), style));
        }
    }

    fn end_line(&mut self) {
        if let Some(line) = self.line.take() {
            self.out.push(line);
        }
    }

    fn gap(&mut self) {
        self.end_line();
        if self.out.last().is_some_and(|l| !l.blank()) {
            self.out.push(Logical::default());
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                self.gap();
                self.heading = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                self.heading = false;
                self.gap();
            }
            Event::End(TagEnd::Paragraph) => {
                if self.lists.is_empty() {
                    self.gap();
                } else {
                    self.end_line();
                }
            }
            Event::Start(Tag::BlockQuote) => {
                self.end_line();
                self.quote += 1;
            }
            Event::End(TagEnd::BlockQuote) => {
                self.end_line();
                self.quote = self.quote.saturating_sub(1);
                self.gap();
            }
            Event::Start(Tag::CodeBlock(_)) => {
                self.gap();
                self.code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.code = false;
                self.gap();
            }
            Event::Start(Tag::List(start)) => {
                self.end_line();
                self.lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                self.end_line();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.gap();
                }
            }
            Event::Start(Tag::Item) => {
                self.end_line();
                let bullet = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => "• ".into(),
                };
                self.start(Some(bullet));
            }
            Event::End(TagEnd::Item) => self.end_line(),
            Event::Start(Tag::Emphasis) => self.italic += 1,
            Event::End(TagEnd::Emphasis) => self.italic = self.italic.saturating_sub(1),
            Event::Start(Tag::Strong) => self.bold += 1,
            Event::End(TagEnd::Strong) => self.bold = self.bold.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => self.strike += 1,
            Event::End(TagEnd::Strikethrough) => self.strike = self.strike.saturating_sub(1),
            Event::Start(Tag::Link { .. }) => self.link = true,
            // A link reads as its underlined text alone; the address never follows it.
            Event::End(TagEnd::Link) => {
                self.link = false;
            }
            Event::Text(text) if self.code => {
                for line in text.lines() {
                    let (mut lead, mut hang) = self.prefix(None);
                    lead.push(Span::raw("  "));
                    hang.push(Span::raw("  "));
                    let code = Span::styled(clean(line).trim_end().to_string(), self.t.accent());
                    self.out.push(Logical { lead, hang, spans: vec![code] });
                }
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                let style = self.style();
                for (n, part) in text.split('\n').enumerate() {
                    if n > 0 {
                        self.end_line();
                    }
                    if !part.is_empty() {
                        self.push(part, style);
                    }
                }
            }
            Event::Code(text) => self.push(&text, self.t.accent()),
            Event::SoftBreak | Event::HardBreak => self.end_line(),
            Event::Rule => {
                self.gap();
                self.out
                    .push(Logical { spans: vec![Span::styled("─".repeat(24), self.t.dim())], ..Default::default() });
                self.gap();
            }
            Event::TaskListMarker(done) => self.push(if done { "[x] " } else { "[ ] " }, plain()),
            Event::FootnoteReference(name) => self.push(&format!("[{name}]"), self.t.dim()),
            _ => {}
        }
    }
}

pub(super) fn markdown(text: &str, t: &Theme) -> Vec<Logical> {
    let mut md = Markdown {
        t,
        out: Vec::new(),
        line: None,
        bold: 0,
        italic: 0,
        strike: 0,
        heading: false,
        code: false,
        quote: 0,
        lists: Vec::new(),
        link: false,
    };
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        md.event(event);
    }
    md.end_line();
    while md.out.last().is_some_and(Logical::blank) {
        md.out.pop();
    }
    md.out
}

/// `text`'s Markdown in rows of at most `width` cells, each row after
/// `lead` (the first) or `hang`.
pub(super) fn rows(
    text: &str,
    lead: &[Span<'static>],
    hang: &[Span<'static>],
    width: usize,
    t: &Theme,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for (n, line) in markdown(text, t).into_iter().enumerate() {
        let mut first = if n == 0 { lead.to_vec() } else { hang.to_vec() };
        first.extend(line.lead);
        let mut next = hang.to_vec();
        next.extend(line.hang);
        rows.extend(crate::wrap::spans(&line.spans, &first, &next, width).into_iter().map(Line::from));
    }
    rows
}
