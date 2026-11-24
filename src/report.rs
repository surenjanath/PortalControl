//! PDF reports, laid out like a certificate (0.14.0): a double-ruled frame,
//! the organisation's logo and address, a serial box with the report number
//! and a seal code, a centred title, a verdict panel, a grid of labelled
//! figures, banded sections, a sign-off with a seal and a signature line, and
//! a footer with the page count and the SHA-256 of the report's data.
//!
//! Three reports: the status report (everything), the site report (one
//! project, a section per application) and the security assessment. They are
//! built from the same target views the dashboard uses, so a report says what
//! the dashboard says, and from the analytics of the last 7 days.
//!
//! Secrets never appear: environment variables are counted, not listed;
//! webhook tokens and secrets are left out; the security checks read
//! variable values but only name the variable.

use crate::db::{Incident, RunRecord, ScanReport};
use crate::pdf::{fit, spaced_width, text_width, wrap, Document, Font, Jpeg, Page, Rgb, PAGE_H, PAGE_W};
use crate::posture::{Level, Posture, ScanFacts};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// The frame is drawn this far in from the page edge.
const FRAME: f64 = 20.0;
const MARGIN: f64 = 44.0;
const CONTENT_W: f64 = PAGE_W - 2.0 * MARGIN;
/// Content stops here; the footer is below.
const BOTTOM: f64 = PAGE_H - 64.0;
/// Content starts here on pages after the first (below the running head).
const RUN_TOP: f64 = 80.0;

const NAVY: Rgb = Rgb(0x1e, 0x3a, 0x5f);
const INK: Rgb = Rgb(0x0a, 0x19, 0x2f);
const SLATE: Rgb = Rgb(0x33, 0x4e, 0x68);
const SUB: Rgb = Rgb(0x47, 0x55, 0x69);
const DIM: Rgb = Rgb(0x64, 0x74, 0x8b);
const LINE: Rgb = Rgb(0xe2, 0xe8, 0xf0);
const RING: Rgb = Rgb(0xcb, 0xd5, 0xe1);
const PANEL: Rgb = Rgb(0xf8, 0xfa, 0xfc);
const BAND: Rgb = Rgb(0xf0, 0xf3, 0xff);
const SKY: Rgb = Rgb(0x02, 0x84, 0xc7);
const TEAL: Rgb = Rgb(0x0d, 0x94, 0x88);
const OK: Rgb = Rgb(0x04, 0x78, 0x57);
const WARN: Rgb = Rgb(0xb4, 0x53, 0x09);
const ERR: Rgb = Rgb(0xb9, 0x1c, 0x1c);
const INFO: Rgb = Rgb(0x1d, 0x4e, 0xd8);

/// Settings key of the report branding.
pub const BRAND_KEY: &str = "report_brand";

/// The organisation named on the reports until one is set in Settings > Reports.
pub const DEFAULT_ORG: &str = "InsightFusionTech";

/// Who the reports are issued by: shown in the masthead and the sign-off.
/// All optional; without a name the reports are issued by [`DEFAULT_ORG`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Brand {
    #[serde(default)]
    pub org_name: String,
    /// One line: street, city, country.
    #[serde(default)]
    pub address: String,
    /// One line: phone, email, website.
    #[serde(default)]
    pub contact: String,
    /// Name printed on the signature line.
    #[serde(default)]
    pub prepared_by: String,
    #[serde(default)]
    pub signatory_title: String,
    /// An extra sentence for the sign-off, for example a classification.
    #[serde(default)]
    pub note: String,
}

impl Brand {
    /// Trim the fields and check their lengths.
    pub fn clean(mut self) -> Result<Self, String> {
        for (name, value, max) in [
            ("organisation name", &mut self.org_name, 100),
            ("address", &mut self.address, 160),
            ("contact line", &mut self.contact, 160),
            ("prepared by", &mut self.prepared_by, 80),
            ("title", &mut self.signatory_title, 80),
            ("note", &mut self.note, 400),
        ] {
            *value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            if value.chars().count() > max {
                return Err(format!("the {name} is limited to {max} characters"));
            }
        }
        Ok(self)
    }
    fn org(&self) -> &str {
        if self.org_name.is_empty() {
            DEFAULT_ORG
        } else {
            &self.org_name
        }
    }
}

/// Average CPU %, average memory and peak memory of a target.
pub type Usage = (Option<f64>, Option<i64>, Option<i64>);

/// Everything a report is made from.
pub struct Input {
    /// `Some(name)` for a site (project) report, `None` otherwise.
    pub project: Option<String>,
    pub host: String,
    pub version: String,
    /// Local time the report was made, `YYYY-MM-DD HH:MM`.
    pub generated: String,
    /// Target views as sent to the dashboard (`api::views::target_view`).
    pub targets: Vec<Value>,
    /// Runs across the targets in the report, newest first.
    pub recent_runs: Vec<RunRecord>,
    /// Site report: recent runs per target.
    pub runs_by_target: HashMap<i64, Vec<RunRecord>>,
    /// The latest scan per target, with its findings.
    pub scans: HashMap<i64, ScanReport>,
    /// Average CPU %, average memory and peak memory over the last 24 hours.
    pub usage_24h: HashMap<i64, Usage>,
    pub brand: Brand,
    pub logo: Option<Jpeg>,
    /// `analytics::build` over the last 7 days, or `Null`.
    pub analytics: Value,
    /// Incidents of the period, newest first (7 days; 30 for the security report).
    pub incidents: Vec<Incident>,
    /// Security report only.
    pub posture: Option<Posture>,
    pub scan_facts: HashMap<i64, ScanFacts>,
}

impl Input {
    /// The report time as a stored-style UTC string, for comparing with
    /// stored times. `generated` is local, so it is converted back.
    fn generated_utc(&self) -> String {
        chrono::NaiveDateTime::parse_from_str(&self.generated, "%Y-%m-%d %H:%M")
            .ok()
            .and_then(|t| t.and_local_timezone(chrono::Local).earliest())
            .map(|t| t.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default()
    }
}

// ------------------------------------------------------------ layout --

#[derive(Clone)]
pub struct Cell {
    text: String,
    color: Rgb,
    font: Font,
    dot: Option<Rgb>,
    /// A second, smaller line in grey under the text.
    sub: Option<String>,
}

fn cell(text: impl Into<String>) -> Cell {
    Cell { text: text.into(), color: INK, font: Font::Regular, dot: None, sub: None }
}
fn colored(text: impl Into<String>, color: Rgb) -> Cell {
    Cell { color, ..cell(text) }
}
fn strong(text: impl Into<String>) -> Cell {
    Cell { font: Font::Bold, ..cell(text) }
}
fn mono(text: impl Into<String>) -> Cell {
    Cell { font: Font::Mono, color: SUB, ..cell(text) }
}
/// A status: a coloured dot and the text.
fn dotted(text: impl Into<String>, color: Rgb) -> Cell {
    Cell { dot: Some(color), ..cell(text) }
}
impl Cell {
    fn with_sub(mut self, sub: impl Into<String>) -> Cell {
        let s = sub.into();
        if !s.is_empty() {
            self.sub = Some(s);
        }
        self
    }
}

/// One labelled figure of a grid.
struct Figure {
    label: String,
    value: String,
    sub: String,
    color: Rgb,
}

fn fig(label: &str, value: impl Into<String>, sub: impl Into<String>, color: Rgb) -> Figure {
    Figure { label: label.into(), value: value.into(), sub: sub.into(), color }
}

/// Small capitals with letter spacing, like the certificate's labels.
fn caps(p: &mut Page, x: f64, y: f64, text: &str, size: f64, color: Rgb) {
    p.text_spaced(x, y, &text.to_uppercase(), Font::Bold, size, color, size * 0.08);
}
fn caps_width(text: &str, size: f64) -> f64 {
    spaced_width(&text.to_uppercase(), Font::Bold, size, size * 0.08)
}

struct Layout {
    doc: Document,
    y: f64,
    /// "Status report no."
    label: &'static str,
    number: String,
    logo: Option<(usize, f64)>,
    org: String,
}

impl Layout {
    fn new(title: &str, label: &'static str, number: String, brand: &Brand, logo: Option<&Jpeg>) -> Self {
        let mut doc = Document::new(title);
        let logo = logo.map(|j| (doc.add_jpeg(j.clone()), j.width as f64 / j.height as f64));
        let mut l = Layout { doc, y: 0.0, label, number, logo, org: brand.org().to_string() };
        l.doc.add_page();
        l.frame();
        l
    }

    fn page(&mut self) -> &mut Page {
        let i = self.doc.page_count() - 1;
        self.doc.page(i)
    }

    /// The double rule around the page.
    fn frame(&mut self) {
        let p = self.page();
        let o = FRAME - 3.5;
        p.stroke_rect(o, o, PAGE_W - 2.0 * o, PAGE_H - 2.0 * o, 0.5, RING);
        p.stroke_rect(FRAME, FRAME, PAGE_W - 2.0 * FRAME, PAGE_H - 2.0 * FRAME, 1.2, NAVY);
    }

    /// A new page with the running head: logo or name, and the report number.
    fn new_page(&mut self) {
        self.doc.add_page();
        self.frame();
        let (logo, org, label, number) = (self.logo, self.org.clone(), self.label, self.number.clone());
        let p = self.page();
        match logo {
            Some((idx, ratio)) => {
                let h = 22.0;
                let w = (h * ratio).min(200.0);
                p.image(idx, MARGIN, 36.0, w, w / ratio);
            }
            None => p.text(MARGIN, 42.0, &org, Font::Bold, 11.0, NAVY),
        }
        let right = PAGE_W - MARGIN;
        caps(p, right - caps_width(label, 6.0), 36.0, label, 6.0, SLATE);
        p.text_right(right, 45.0, &number, Font::Bold, 10.0, INK);
        p.rect(MARGIN, 63.0, CONTENT_W, 1.6, NAVY);
        self.y = RUN_TOP;
    }

    /// Start a new page if `h` more points do not fit.
    fn need(&mut self, h: f64) -> bool {
        if self.y + h > BOTTOM {
            self.new_page();
            true
        } else {
            false
        }
    }

    fn gap(&mut self, h: f64) {
        self.y += h;
    }

    /// The first page's head: logo and issuer on the left, the serial box
    /// on the right, a navy rule under both.
    fn masthead(&mut self, brand: &Brand, issued: &str, seal: &str) {
        let top = 38.0;
        let box_w = 196.0;
        let left_w = CONTENT_W - box_w - 18.0;
        let (logo, label, number) = (self.logo, self.label, self.number.clone());
        let p = self.page();
        let mut y = top;
        match logo {
            Some((idx, ratio)) => {
                let h = 40.0;
                let w = (h * ratio).min(left_w);
                p.image(idx, MARGIN, y, w, w / ratio);
                y += w / ratio + 7.0;
            }
            None => {
                p.text(MARGIN, y, &fit(brand.org(), Font::Bold, 16.0, left_w), Font::Bold, 16.0, NAVY);
                y += 23.0;
            }
        }
        let issuer = if brand.address.is_empty() {
            brand.org().to_uppercase()
        } else {
            format!("{} \u{00b7} {}", brand.org().to_uppercase(), brand.address)
        };
        for line in wrap(&issuer, Font::Regular, 7.4, left_w).into_iter().take(2) {
            p.text(MARGIN, y, &line, Font::Regular, 7.4, SLATE);
            y += 9.5;
        }
        if !brand.contact.is_empty() {
            p.text(MARGIN, y, &fit(&brand.contact, Font::Regular, 7.4, left_w), Font::Regular, 7.4, SUB);
            y += 9.5;
        }

        let bx = PAGE_W - MARGIN - box_w;
        let bh = 62.0;
        p.rect(bx, top, box_w, bh, PANEL);
        p.stroke_rect(bx, top, box_w, bh, 0.6, RING);
        caps(p, bx + 10.0, top + 9.0, label, 6.0, SLATE);
        p.text(bx + 10.0, top + 19.0, &number, Font::Bold, 12.5, INK);
        p.text(bx + 10.0, top + 36.0, &format!("Issued {issued}"), Font::Regular, 7.4, SUB);
        let code: String = seal.chars().take(16).collect::<String>().to_uppercase();
        p.text_spaced(bx + 10.0, top + 48.0, &format!("SEAL CODE {code}"), Font::Mono, 6.4, SKY, 0.3);

        let rule = y.max(top + bh) + 8.0;
        p.rect(MARGIN, rule, CONTENT_W, 2.0, NAVY);
        self.y = rule + 18.0;
    }

    /// The centred title, an optional subtitle and the lead paragraph.
    fn title(&mut self, title: &str, subtitle: Option<&str>, lead: &str) {
        let center = PAGE_W / 2.0;
        let size = 16.5;
        let w = caps_width(title, size);
        let y = self.y;
        let p = self.page();
        p.text_spaced(center - w / 2.0, y, &title.to_uppercase(), Font::Bold, size, INK, size * 0.08);
        self.y += 24.0;
        if let Some(s) = subtitle {
            let y = self.y;
            self.page().text_center(center, y, &fit(s, Font::Bold, 11.5, CONTENT_W), Font::Bold, 11.5, SKY);
            self.y += 17.0;
        }
        for line in wrap(lead, Font::Regular, 8.4, 430.0) {
            let y = self.y;
            self.page().text_center(center, y, &line, Font::Regular, 8.4, SLATE);
            self.y += 11.5;
        }
        self.y += 10.0;
    }

    /// The highlighted panel with the overall verdict, and up to two figures
    /// on the right.
    fn verdict(&mut self, label: &str, text: &str, color: Rgb, note: &str, figures: &[(&str, String)]) {
        let h = 60.0;
        self.need(h + 10.0);
        let y = self.y;
        let right_w = 112.0 * figures.len() as f64;
        let left_w = CONTENT_W - right_w - 28.0;
        let p = self.page();
        p.rect(MARGIN, y, CONTENT_W, h, BAND);
        p.rect(MARGIN, y, 3.5, h, color);
        caps(p, MARGIN + 16.0, y + 11.0, label, 6.3, SKY);
        let big = fit(&text.to_uppercase(), Font::Bold, 14.5, left_w);
        p.text(MARGIN + 16.0, y + 22.0, &big, Font::Bold, 14.5, color);
        p.text(MARGIN + 16.0, y + 42.0, &fit(note, Font::Regular, 7.8, left_w), Font::Regular, 7.8, SUB);
        for (n, (flabel, value)) in figures.iter().enumerate() {
            let x = PAGE_W - MARGIN - right_w + 112.0 * n as f64;
            p.line(x, y + 12.0, x, y + h - 12.0, 0.6, RING);
            caps(p, x + 12.0, y + 14.0, flabel, 5.8, SLATE);
            p.text(x + 12.0, y + 26.0, &fit(value, Font::Bold, 13.0, 96.0), Font::Bold, 13.0, INK);
        }
        self.y += h + 14.0;
    }

    /// Labelled figures in two columns, ruled like the certificate's grid.
    fn grid(&mut self, figures: &[Figure]) {
        let col_w = CONTENT_W / 2.0;
        let y0 = self.y;
        self.page().line(MARGIN, y0, MARGIN + CONTENT_W, y0, 0.6, LINE);
        for pair in figures.chunks(2) {
            let wrapped: Vec<(Vec<String>, Vec<String>)> = pair
                .iter()
                .map(|f| {
                    let mut v = wrap(&f.value, Font::Bold, 10.2, col_w - 22.0);
                    v.truncate(2);
                    let mut s = if f.sub.is_empty() {
                        vec![]
                    } else {
                        wrap(&f.sub, Font::Regular, 7.5, col_w - 22.0)
                    };
                    s.truncate(2);
                    (v, s)
                })
                .collect();
            let h = wrapped
                .iter()
                .map(|(v, s)| 22.0 + 13.0 * v.len() as f64 + 9.5 * s.len() as f64)
                .fold(0.0, f64::max)
                + 2.0;
            if self.need(h) {
                let y = self.y;
                self.page().line(MARGIN, y, MARGIN + CONTENT_W, y, 0.6, LINE);
            }
            let y = self.y;
            let p = self.page();
            for (i, (f, (vlines, slines))) in pair.iter().zip(&wrapped).enumerate() {
                let x = MARGIN + i as f64 * col_w + if i == 1 { 12.0 } else { 0.0 };
                caps(p, x, y + 8.0, &f.label, 6.0, SLATE);
                let mut ly = y + 19.0;
                for v in vlines {
                    p.text(x, ly, v, Font::Bold, 10.2, f.color);
                    ly += 13.0;
                }
                for s in slines {
                    p.text(x, ly, s, Font::Regular, 7.5, SUB);
                    ly += 9.5;
                }
            }
            p.line(MARGIN + col_w, y, MARGIN + col_w, y + h, 0.6, LINE);
            p.line(MARGIN, y + h, MARGIN + CONTENT_W, y + h, 0.6, LINE);
            self.y += h;
        }
        self.y += 12.0;
    }

    /// A section heading: a tinted band with a navy edge and caps.
    fn section(&mut self, title: &str) {
        self.need(70.0);
        self.gap(6.0);
        let y = self.y;
        let p = self.page();
        p.rect(MARGIN, y, CONTENT_W, 18.0, BAND);
        p.rect(MARGIN, y, 3.0, 18.0, NAVY);
        caps(p, MARGIN + 11.0, y + 5.6, title, 7.4, INK);
        self.gap(27.0);
    }

    /// A smaller heading with a note beside it. Room is kept for what follows.
    fn subheading(&mut self, text: &str, note: &str) {
        self.need(64.0);
        let y = self.y;
        let p = self.page();
        p.text(MARGIN, y, &fit(text, Font::Bold, 10.0, CONTENT_W * 0.6), Font::Bold, 10.0, INK);
        if !note.is_empty() {
            let w = text_width(text, Font::Bold, 10.0).min(CONTENT_W * 0.6);
            let room = CONTENT_W - w - 10.0;
            p.text(MARGIN + w + 10.0, y + 1.8, &fit(note, Font::Regular, 7.8, room), Font::Regular, 7.8, DIM);
        }
        self.gap(16.0);
    }

    fn paragraph(&mut self, text: &str, color: Rgb, size: f64) {
        for line in wrap(text, Font::Regular, size, CONTENT_W) {
            self.need(size + 4.0);
            let y = self.y;
            self.page().text(MARGIN, y, &line, Font::Regular, size, color);
            self.gap(size + 4.0);
        }
    }

    /// A table whose header repeats after a page break. Column widths are
    /// fractions of the content width; a cell wraps onto at most `max_lines`.
    fn table(&mut self, cols: &[(&str, f64)], rows: &[Vec<Cell>], max_lines: usize) {
        let size = 8.0;
        let lh = 10.4;
        let widths: Vec<f64> = cols.iter().map(|(_, f)| f * CONTENT_W).collect();
        let header = |l: &mut Layout| {
            let y = l.y;
            let p = l.page();
            let mut x = MARGIN;
            for ((title, _), w) in cols.iter().zip(&widths) {
                let t = fit(&title.to_uppercase(), Font::Bold, 6.0, w - 8.0);
                p.text_spaced(x + 4.0, y + 4.0, &t, Font::Bold, 6.0, SLATE, 0.45);
                x += w;
            }
            p.rect(MARGIN, y + 14.0, CONTENT_W, 0.9, NAVY);
            l.gap(16.0);
        };
        self.need(52.0);
        header(self);
        for row in rows {
            let laid: Vec<(Vec<String>, Vec<String>)> = row
                .iter()
                .zip(&widths)
                .map(|(c, w)| {
                    let room = w - 8.0 - if c.dot.is_some() { 10.0 } else { 0.0 };
                    let main = if max_lines <= 1 {
                        vec![fit(&c.text, c.font, size, room)]
                    } else {
                        let mut v = wrap(&c.text, c.font, size, room);
                        if v.len() > max_lines {
                            v.truncate(max_lines);
                            let last = v.pop().unwrap_or_default();
                            v.push(fit(&format!("{last}\u{2026}"), c.font, size, room));
                        }
                        v
                    };
                    let sub = c
                        .sub
                        .as_ref()
                        .map(|s| {
                            let mut v = wrap(s, Font::Regular, 7.0, room);
                            v.truncate(max_lines.max(1));
                            v
                        })
                        .unwrap_or_default();
                    (main, sub)
                })
                .collect();
            let lines =
                laid.iter().map(|(m, s)| m.len() as f64 * lh + s.len() as f64 * 9.0).fold(lh, f64::max);
            let row_h = lines + 7.0;
            if self.need(row_h) {
                header(self);
            }
            let y = self.y;
            let p = self.page();
            let mut x = MARGIN;
            for ((c, w), (main, sub)) in row.iter().zip(&widths).zip(&laid) {
                let mut tx = x + 4.0;
                if let Some(dot) = c.dot {
                    p.circle(tx + 3.0, y + 7.4, 2.6, dot);
                    tx += 10.0;
                }
                let mut ly = y + 4.0;
                for line in main {
                    p.text(tx, ly, line, c.font, size, c.color);
                    ly += lh;
                }
                for line in sub {
                    p.text(tx, ly, line, Font::Regular, 7.0, SUB);
                    ly += 9.0;
                }
                x += w;
            }
            p.line(MARGIN, y + row_h, MARGIN + CONTENT_W, y + row_h, 0.5, LINE);
            self.gap(row_h);
        }
        if rows.is_empty() {
            self.need(18.0);
            let y = self.y;
            self.page().text(MARGIN + 4.0, y + 4.0, "Nothing to show.", Font::Regular, size, DIM);
            self.gap(18.0);
        }
        self.gap(10.0);
    }

    /// Runs per day as stacked bars (succeeded, failed), with the incidents
    /// of each day written under it. `slots`: (label, ok, failed, incidents).
    fn bars(&mut self, slots: &[(String, u32, u32, u32)]) {
        let h = 128.0;
        self.need(h + 8.0);
        let y = self.y;
        let x0 = MARGIN + 26.0;
        let w = CONTENT_W - 26.0;
        let top = y + 10.0;
        let base = y + 82.0;
        let max = slots.iter().map(|s| s.1 + s.2).max().unwrap_or(0).max(1);
        // A round top value: 1, 2, 5, 10, 20, 50 ...
        let mut scale = 1u32;
        let mut step = 0;
        while scale < max {
            scale = if step % 3 == 1 { scale * 5 / 2 } else { scale * 2 };
            step += 1;
        }
        let p = self.page();
        for (frac, label) in [(0.0, scale), (0.5, scale / 2), (1.0, 0)] {
            let gy = top + (base - top) * frac;
            let base_line = frac == 1.0;
            p.line(
                x0,
                gy,
                x0 + w,
                gy,
                if base_line { 0.8 } else { 0.4 },
                if base_line { RING } else { LINE },
            );
            if scale > 1 || frac != 0.5 {
                p.text_right(x0 - 5.0, gy - 3.0, &label.to_string(), Font::Regular, 6.4, DIM);
            }
        }
        let n = slots.len().max(1) as f64;
        let slot_w = w / n;
        let bar_w = (slot_w * 0.5).min(34.0);
        let unit = (base - top) / scale as f64;
        for (i, (label, ok, failed, incidents)) in slots.iter().enumerate() {
            let cx = x0 + slot_w * (i as f64 + 0.5);
            let ok_h = *ok as f64 * unit;
            let fail_h = *failed as f64 * unit;
            if ok_h > 0.0 {
                p.rect(cx - bar_w / 2.0, base - ok_h, bar_w, ok_h, OK);
            }
            if fail_h > 0.0 {
                p.rect(cx - bar_w / 2.0, base - ok_h - fail_h, bar_w, fail_h, ERR);
            }
            if ok + failed > 0 {
                let t = (ok + failed).to_string();
                p.text_center(cx, base - ok_h - fail_h - 9.0, &t, Font::Bold, 6.6, SLATE);
            }
            p.text_center(cx, base + 5.0, label, Font::Regular, 6.8, SUB);
            if *incidents > 0 {
                let t = format!("{incidents} incident{}", if *incidents == 1 { "" } else { "s" });
                p.text_center(cx, base + 15.0, &t, Font::Bold, 6.2, WARN);
            }
        }
        let ly = y + 112.0;
        let mut lx = x0;
        for (color, text) in [(OK, "Runs succeeded"), (ERR, "Runs failed")] {
            p.rect(lx, ly + 1.0, 7.0, 7.0, color);
            p.text(lx + 11.0, ly, text, Font::Regular, 7.0, SUB);
            lx += 18.0 + text_width(text, Font::Regular, 7.0);
        }
        p.text(lx, ly, "Incidents opened are written under each day.", Font::Regular, 7.0, DIM);
        self.y += h;
    }

    /// The closing block: what the report is, the seal and the signature line.
    fn signoff(&mut self, brand: &Brand, notice: &str) {
        self.need(112.0);
        self.gap(10.0);
        let y = self.y;
        let left_w = CONTENT_W - 210.0;
        let p = self.page();
        p.line(MARGIN, y, MARGIN + CONTENT_W, y, 0.6, LINE);
        caps(p, MARGIN, y + 12.0, "About this report", 6.6, INK);
        let mut ly = y + 24.0;
        let mut text = notice.to_string();
        if !brand.note.is_empty() {
            text = format!("{text} {}", brand.note);
        }
        for line in wrap(&text, Font::Regular, 7.4, left_w).into_iter().take(8) {
            p.text(MARGIN, ly, &line, Font::Regular, 7.4, SLATE);
            ly += 9.6;
        }

        let right = PAGE_W - MARGIN;
        // The seal: a double teal border around a tick and its words.
        let seal = "Generated by PortalControl \u{00b7} verified data";
        let sw = caps_width(seal, 5.8) + 26.0;
        let sx = right - sw;
        let sy = y + 12.0;
        p.stroke_rect(sx, sy, sw, 17.0, 1.0, TEAL);
        p.stroke_rect(sx + 2.0, sy + 2.0, sw - 4.0, 13.0, 0.4, TEAL);
        p.line(sx + 7.0, sy + 8.8, sx + 9.6, sy + 11.4, 1.1, TEAL);
        p.line(sx + 9.6, sy + 11.4, sx + 14.0, sy + 5.8, 1.1, TEAL);
        caps(p, sx + 18.0, sy + 5.8, seal, 5.8, TEAL);

        let line_y = y + 66.0;
        if !brand.prepared_by.is_empty() {
            let name = fit(&brand.prepared_by, Font::Bold, 13.0, 190.0);
            p.text_right(right, line_y - 17.0, &name, Font::Bold, 13.0, INK);
        }
        p.line(right - 190.0, line_y, right, line_y, 0.7, INK);
        let role = if brand.signatory_title.is_empty() {
            "Reviewed by".to_string()
        } else {
            format!("Reviewed by \u{00b7} {}", brand.signatory_title)
        };
        p.text_right(right, line_y + 5.0, &fit(&role, Font::Bold, 7.6, 190.0), Font::Bold, 7.6, INK);
        let behalf =
            fit(&format!("For & on behalf of {}", brand.org()).to_uppercase(), Font::Bold, 5.6, 180.0);
        let bw = spaced_width(&behalf, Font::Bold, 5.6, 0.45);
        p.text_spaced(right - bw, line_y + 16.0, &behalf, Font::Bold, 5.6, SLATE, 0.45);
        self.y = ly.max(line_y + 26.0) + 6.0;
    }

    /// Footer on every page: page count, when, who; and the seal hash.
    fn finish(mut self, footer: &str, seal: &str) -> Vec<u8> {
        let total = self.doc.page_count();
        let hash: String = seal.chars().take(16).collect();
        for i in 0..total {
            let p = self.doc.page(i);
            let fy = PAGE_H - FRAME - 24.0;
            p.line(MARGIN, fy - 5.0, PAGE_W - MARGIN, fy - 5.0, 0.6, LINE);
            let left = format!("Page {} of {total} \u{00b7} {footer}", i + 1);
            p.text(MARGIN, fy, &fit(&left, Font::Regular, 6.6, CONTENT_W - 118.0), Font::Regular, 6.6, DIM);
            p.text_right(PAGE_W - MARGIN, fy, &format!("SHA-256 {hash}"), Font::Mono, 6.2, DIM);
        }
        self.doc.finish()
    }
}

// ----------------------------------------------------- reading the views --

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn i(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}
fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// The status text and colour the dashboard shows (`tStatus` in core.js).
pub fn status(t: &Value) -> (&'static str, Rgb) {
    let health = t.get("health").and_then(|h| h.get("status")).and_then(Value::as_str).unwrap_or("");
    let service = s(t, "run_mode") == "SERVICE";
    let has_url = !s(t, "health_url").is_empty();
    let last = t.get("last_run").and_then(|r| r.get("status")).and_then(Value::as_str);
    if t.get("running").is_some_and(|r| !r.is_null()) {
        if !service {
            return ("Deploying", INFO);
        }
        return match (has_url, health) {
            (false, _) => ("Running", OK),
            (true, "LIVE") => ("Running (healthy)", OK),
            (true, "SLOW") => ("Running (slow)", WARN),
            (true, "DOWN") => ("Running (unhealthy)", WARN),
            _ => ("Running (checking)", INFO),
        };
    }
    if t.get("restart_pending").and_then(Value::as_bool).unwrap_or(false) {
        return ("Restarting", WARN);
    }
    if service {
        if health == "LIVE" || health == "SLOW" {
            return ("Up, not supervised", WARN);
        }
        if last == Some("FAILED") {
            return ("Exited (failed)", ERR);
        }
        return ("Stopped", DIM);
    }
    match last {
        None => ("Never deployed", DIM),
        Some("SUCCESS") => ("Deployed", OK),
        Some(_) => ("Last deploy failed", ERR),
    }
}

fn git_text(t: &Value) -> (String, Rgb) {
    let Some(g) = t.get("git").filter(|g| !g.is_null()) else { return ("checking".into(), DIM) };
    if !g.get("is_repo").and_then(Value::as_bool).unwrap_or(false) {
        return ("not a repository".into(), DIM);
    }
    match s(g, "status") {
        "UPDATES_AVAILABLE" => {
            let n = i(g, "behind");
            (format!("{n} update{} waiting", if n == 1 { "" } else { "s" }), INFO)
        }
        "DIRTY" => (format!("{} local change(s)", i(g, "dirty_files")), WARN),
        _ => ("up to date".into(), OK),
    }
}

fn scan_counts(t: &Value) -> Option<(i64, i64)> {
    t.get("scan")
        .filter(|s| !s.is_null())
        .map(|s| (i(s, "critical_issues_found"), i(s, "vulnerable_packages_found")))
}

/// `YYYY-MM-DD HH:MM:SS` in UTC (as stored) to local `YYYY-MM-DD HH:MM`.
pub fn local_time(utc: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(utc.trim(), "%Y-%m-%d %H:%M:%S")
        .map(|t| {
            chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(t, chrono::Utc)
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| utc.to_string())
}

/// Unix seconds of a stored UTC time.
fn utc_secs(utc: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(utc.trim(), "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|t| t.and_utc().timestamp())
}

/// `2026-11-24 15:30` to `24 November 2026, 15:30`.
fn long_date(generated: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(generated, "%Y-%m-%d %H:%M")
        .map(|t| t.format("%-d %B %Y, %H:%M").to_string())
        .unwrap_or_else(|_| generated.to_string())
}

fn duration(ms: Option<i64>) -> String {
    match ms {
        None => "running".into(),
        Some(ms) if ms < 1000 => format!("{ms} ms"),
        Some(ms) if ms < 60_000 => format!("{:.1} s", ms as f64 / 1000.0),
        Some(ms) => format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

fn minutes(m: i64) -> String {
    if m < 60 {
        format!("{m} min")
    } else if m < 1440 {
        format!("{}h {}m", m / 60, m % 60)
    } else {
        format!("{}d {}h", m / 1440, (m % 1440) / 60)
    }
}

fn bytes(n: i64) -> String {
    let n = n as f64;
    if n >= 1e9 {
        format!("{:.1} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.0} MB", n / 1e6)
    } else {
        format!("{:.0} KB", n / 1e3)
    }
}

fn pct(p: Option<f64>) -> String {
    match p {
        None => "-".into(),
        Some(p) if p >= 99.995 => "100%".into(),
        Some(p) => format!("{p:.2}%"),
    }
}

fn pct_color(p: Option<f64>) -> Rgb {
    match p {
        None => DIM,
        Some(p) if p >= 99.0 => OK,
        Some(p) if p >= 95.0 => WARN,
        Some(_) => ERR,
    }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn trigger(t: &str) -> &str {
    match t {
        "MANUAL" => "Manual",
        "SCHEDULE" => "Schedule",
        "GIT_AUTO" => "Auto-deploy",
        "WEBHOOK" => "Webhook",
        "AUTO_RESTART" => "Crash restart",
        "RESTART" => "Restart",
        "ROLLBACK" => "Rollback",
        "COMMAND" => "Command",
        "SYSTEM_AUTO" => "Autostart",
        other => other,
    }
}

fn kind_name(k: &str) -> &str {
    match k {
        "failure" => "Failed run",
        "crash_loop" => "Crash loop",
        "health_down" => "Health check down",
        "deploy_failed" => "Deployment failed",
        "error_burst" => "Burst of errors",
        "new_error" => "New error",
        "attack" => "Attack",
        "brute_force" => "Password guessing",
        "flood" => "Flood of requests",
        other => other,
    }
}

fn severity_color(sev: &str) -> Rgb {
    match sev {
        "critical" => ERR,
        "warning" => WARN,
        _ => INFO,
    }
}

fn run_result(r: &RunRecord) -> Cell {
    match r.status.as_str() {
        "SUCCESS" => dotted("Succeeded", OK),
        "RUNNING" => dotted("Running", INFO),
        _ => {
            dotted(r.exit_code.map(|c| format!("Failed (exit {c})")).unwrap_or_else(|| "Failed".into()), ERR)
        }
    }
}

/// What needs a person: failures, unhealthy services, updates, findings.
/// Each item is "name: text".
fn attention(targets: &[Value]) -> Vec<(Rgb, String)> {
    let mut out = Vec::new();
    for t in targets {
        let name = s(t, "name");
        let (st, color) = status(t);
        if color == ERR {
            out.push((ERR, format!("{name}: {}.", st.to_lowercase())));
        } else if st == "Running (unhealthy)" {
            out.push((WARN, format!("{name}: running but its health check does not answer.")));
        }
        if let Some((c, v)) = scan_counts(t).filter(|(c, v)| c + v > 0) {
            out.push((
                if c > 0 { ERR } else { WARN },
                format!("{name}: {c} critical security issue(s) and {v} vulnerable package(s)."),
            ));
        }
        if let Some(g) = t.get("git").filter(|g| !g.is_null()) {
            match s(g, "status") {
                "UPDATES_AVAILABLE" => out.push((
                    INFO,
                    format!("{name}: {} new commit(s) on origin, ready to pull.", i(g, "behind")),
                )),
                "DIRTY" => out.push((
                    WARN,
                    format!("{name}: {} uncommitted change(s) in the working tree.", i(g, "dirty_files")),
                )),
                _ => {}
            }
        }
        if let Some(p) = t.get("uptime_pct").and_then(Value::as_f64).filter(|p| *p < 90.0) {
            out.push((WARN, format!("{name}: reachable only {p:.1}% of recent health probes.")));
        }
    }
    out
}

/// Attention items plus open incidents, most urgent first.
fn attention_rows(input: &Input) -> Vec<(Rgb, String)> {
    let mut items = attention(&input.targets);
    let names: HashMap<i64, &str> = input.targets.iter().map(|t| (i(t, "id"), s(t, "name"))).collect();
    let mut open: HashMap<i64, (usize, bool)> = HashMap::new();
    for inc in input.incidents.iter().filter(|x| x.status == "open") {
        if let Some(id) = inc.target_id.filter(|id| names.contains_key(id)) {
            let e = open.entry(id).or_default();
            e.0 += 1;
            e.1 |= inc.severity == "critical";
        }
    }
    let mut ids: Vec<_> = open.into_iter().collect();
    ids.sort();
    for (id, (n, critical)) in ids {
        items.push((
            if critical { ERR } else { WARN },
            format!("{}: {} open incident(s) on the Incidents page.", names[&id], n),
        ));
    }
    let rank = |c: &Rgb| {
        if *c == ERR {
            0
        } else if *c == WARN {
            1
        } else {
            2
        }
    };
    items.sort_by_key(|(c, _)| rank(c));
    items
}

fn attention_table(l: &mut Layout, items: &[(Rgb, String)]) {
    if items.is_empty() {
        l.paragraph(
            "Nothing needs attention: everything is running, healthy, up to date and clean.",
            OK,
            8.6,
        );
        l.gap(6.0);
        return;
    }
    let shown = items.len().min(14);
    let rows: Vec<Vec<Cell>> = items[..shown]
        .iter()
        .map(|(color, text)| {
            let (name, what) = text.split_once(": ").unwrap_or(("", text));
            let word = if *color == ERR {
                "Critical"
            } else if *color == WARN {
                "Warning"
            } else {
                "Notice"
            };
            vec![dotted(word, *color), strong(name), cell(what)]
        })
        .collect();
    l.table(&[("Priority", 0.14), ("Application", 0.26), ("Item", 0.60)], &rows, 2);
    if items.len() > shown {
        l.paragraph(&format!("{} more item(s) are listed on the dashboard.", items.len() - shown), DIM, 8.0);
    }
}

/// The verdict of a list of attention items.
fn verdict_of(items: &[(Rgb, String)]) -> (&'static str, Rgb, String) {
    let critical = items.iter().filter(|(c, _)| *c == ERR).count();
    let other = items.len() - critical;
    if items.is_empty() {
        ("All systems operational", OK, "Every application is running, healthy, up to date and clean.".into())
    } else if critical > 0 {
        ("Action required", ERR, format!("{critical} critical and {other} other item(s) need attention."))
    } else {
        ("Attention advised", WARN, format!("{other} item(s) need attention; nothing is critical."))
    }
}

/// The analytics row of one target, if there is one.
fn stats(input: &Input, id: i64) -> Option<&Value> {
    input.analytics.get("targets").and_then(Value::as_array)?.iter().find(|t| i(t, "id") == id)
}

/// Report number and seal: the SHA-256 of the facts the report states.
fn seal(prefix: &str, input: &Input, extra: &str) -> (String, String) {
    let mut h = Sha256::new();
    h.update(prefix.as_bytes());
    h.update(input.generated.as_bytes());
    h.update(input.host.as_bytes());
    for t in &input.targets {
        h.update(format!(
            "{}|{}|{}|{}|{:?}|{}",
            i(t, "id"),
            s(t, "name"),
            status(t).0,
            t.get("git").and_then(|g| g.get("head")).and_then(Value::as_str).unwrap_or(""),
            scan_counts(t),
            t.get("uptime_pct").map(Value::to_string).unwrap_or_default()
        ));
    }
    h.update(input.analytics.get("summary").map(Value::to_string).unwrap_or_default());
    h.update(extra.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let stamp: String = input.generated.chars().filter(char::is_ascii_digit).collect();
    let stamp = if stamp.len() >= 12 { format!("{}-{}", &stamp[..8], &stamp[8..12]) } else { stamp };
    (format!("{prefix}-{stamp}-{}", hex[..4].to_uppercase()), hex)
}

fn footer(input: &Input) -> String {
    format!(
        "Printed {} \u{00b7} {} \u{00b7} {} \u{00b7} PortalControl {}",
        long_date(&input.generated),
        input.brand.org(),
        input.host,
        input.version
    )
}

/// The figures of a set of targets for the grid. `project` is the site's
/// row of the analytics; without it the summary of all targets is used.
fn fleet_figures(input: &Input, targets: &[&Value], project: Option<&Value>) -> Vec<Figure> {
    let total = targets.len() as i64;
    let running = targets.iter().filter(|t| t.get("running").is_some_and(|r| !r.is_null())).count() as i64;
    let services = targets.iter().filter(|t| s(t, "run_mode") == "SERVICE").count() as i64;
    let probed: Vec<&&Value> = targets.iter().filter(|t| !s(t, "health_url").is_empty()).collect();
    let live = probed
        .iter()
        .filter(|t| {
            matches!(
                t.get("health").and_then(|h| h.get("status")).and_then(Value::as_str),
                Some("LIVE" | "SLOW")
            )
        })
        .count();
    let updates = targets
        .iter()
        .filter(|t| {
            t.get("git").and_then(|g| g.get("status")).and_then(Value::as_str) == Some("UPDATES_AVAILABLE")
        })
        .count() as i64;
    let (crit, vuln) =
        targets.iter().filter_map(|t| scan_counts(t)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    let scanned = targets.iter().filter(|t| scan_counts(t).is_some()).count() as i64;

    let sum = project.or_else(|| input.analytics.get("summary")).cloned().unwrap_or(Value::Null);
    let avail = f(&sum, "availability_pct");
    let downtime = i(&sum, "downtime_minutes");
    let runs = i(&sum, "runs");
    let failed = i(&sum, "runs_failed");
    let crashes = i(&sum, "crash_restarts");
    let ids: Vec<i64> = targets.iter().map(|t| i(t, "id")).collect();
    let incidents: Vec<&Incident> =
        input.incidents.iter().filter(|x| x.target_id.is_some_and(|id| ids.contains(&id))).collect();
    let open = incidents.iter().filter(|x| x.status == "open").count();

    vec![
        fig(
            "Applications running",
            format!("{running} of {total}"),
            format!(
                "{} \u{00b7} {}",
                plural(services, "service", "services"),
                plural(total - services, "task", "tasks")
            ),
            if running < services { WARN } else { INK },
        ),
        fig(
            "Health checks answering",
            if probed.is_empty() { "-".into() } else { format!("{live} of {}", probed.len()) },
            if probed.is_empty() { "no health URLs configured" } else { "probed every 15 seconds" },
            if live < probed.len() { ERR } else { INK },
        ),
        fig(
            "Availability \u{00b7} 7 days",
            pct(avail),
            if avail.is_none() {
                "no health probes recorded".into()
            } else if downtime > 0 {
                format!("{} unreachable in total", minutes(downtime))
            } else {
                "no outages".into()
            },
            pct_color(avail),
        ),
        fig(
            "Deployments \u{00b7} 7 days",
            if runs > 0 {
                format!("{runs} runs, {}% succeeded", ((runs - failed) * 100) / runs)
            } else {
                "none".into()
            },
            format!("{failed} failed \u{00b7} {}", plural(crashes, "crash restart", "crash restarts")),
            if failed > 0 { WARN } else { INK },
        ),
        fig(
            "Incidents \u{00b7} 7 days",
            format!("{} \u{00b7} {open} open", incidents.len()),
            "found by the log monitor",
            if open > 0 { ERR } else { INK },
        ),
        fig(
            "Updates waiting",
            plural(updates, "application", "applications"),
            "new commits on origin, not deployed",
            if updates > 0 { INFO } else { INK },
        ),
        fig(
            "Security",
            if scanned == 0 {
                "not scanned".into()
            } else {
                format!("{crit} critical \u{00b7} {vuln} vulnerable pkg")
            },
            format!("{scanned} of {total} scanned \u{00b7} details in the security assessment"),
            if crit > 0 {
                ERR
            } else if vuln > 0 {
                WARN
            } else {
                INK
            },
        ),
        fig("Host", input.host.clone(), format!("PortalControl {}", input.version), INK),
    ]
}

fn day_slots(input: &Input) -> Vec<(String, u32, u32, u32)> {
    input
        .analytics
        .get("timeline")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| {
                    let label = chrono::DateTime::from_timestamp(i(x, "t"), 0)
                        .map(|d| d.with_timezone(&chrono::Local).format("%a %-d").to_string())
                        .unwrap_or_default();
                    (label, i(x, "ok") as u32, i(x, "failed") as u32, i(x, "incidents") as u32)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn version_text(t: &Value) -> String {
    t.get("git")
        .filter(|g| !g.is_null())
        .and_then(|g| {
            g.get("head").and_then(Value::as_str).map(|h| {
                format!(
                    "{} {}",
                    &h[..h.len().min(8)],
                    g.get("head_summary").and_then(Value::as_str).unwrap_or("")
                )
            })
        })
        .unwrap_or_else(|| "-".into())
}

fn last_deploy(t: &Value) -> Cell {
    match t.get("last_run").filter(|r| !r.is_null()) {
        None => colored("never", DIM),
        Some(r) => {
            let color = match s(r, "status") {
                "SUCCESS" => OK,
                "RUNNING" => INFO,
                _ => ERR,
            };
            dotted(local_time(s(r, "executed_at")), color)
        }
    }
}

fn applications_table(l: &mut Layout, input: &Input, targets: &[&Value], with_site: bool) {
    let rows: Vec<Vec<Cell>> = targets
        .iter()
        .map(|t| {
            let (st, sc) = status(t);
            let avail = stats(input, i(t, "id")).and_then(|x| f(x, "availability_pct"));
            let mut row = vec![strong(s(t, "name"))];
            if with_site {
                row.push(cell(s(t, "project")));
            }
            row.extend([
                dotted(st, sc),
                colored(
                    if s(t, "health_url").is_empty() { "-".into() } else { pct(avail) },
                    pct_color(avail),
                ),
                mono(version_text(t)),
                last_deploy(t),
            ]);
            row
        })
        .collect();
    if with_site {
        l.table(
            &[
                ("Application", 0.2),
                ("Site", 0.13),
                ("Status", 0.19),
                ("Avail. 7 d", 0.1),
                ("Version", 0.21),
                ("Last deployment", 0.17),
            ],
            &rows,
            1,
        );
    } else {
        l.table(
            &[
                ("Application", 0.24),
                ("Status", 0.21),
                ("Avail. 7 d", 0.11),
                ("Version", 0.26),
                ("Last deployment", 0.18),
            ],
            &rows,
            1,
        );
    }
}

const NOTICE: &str = "Generated automatically from PortalControl's live state and stored history at the time shown. \
     It contains no secrets: environment variables are counted, never listed, and tokens and passwords are left out. \
     The seal code and the SHA-256 in the footer identify the data it was made from.";

/// The status report: everything PortalControl supervises.
pub fn overall(input: &Input) -> Vec<u8> {
    let (number, hash) = seal("OPS", input, "");
    let mut l =
        Layout::new("System status report", "Status report no.", number, &input.brand, input.logo.as_ref());
    l.masthead(&input.brand, &long_date(&input.generated), &hash);
    let mut sites: Vec<&str> = input.targets.iter().map(|t| s(t, "project")).collect();
    sites.sort_unstable();
    sites.dedup();
    l.title(
        "System status report",
        None,
        &format!(
            "The operational state of the {} supervised by PortalControl on {}, in {}, as recorded on {}. \
             Availability, deployments and incidents cover the last 7 days.",
            plural(input.targets.len() as i64, "application", "applications"),
            input.host,
            plural(sites.len() as i64, "site", "sites"),
            long_date(&input.generated)
        ),
    );
    let items = attention_rows(input);
    let (word, color, note) = verdict_of(&items);
    let summary = input.analytics.get("summary");
    l.verdict(
        "Overall status",
        word,
        color,
        &note,
        &[
            ("Availability 7 d", pct(summary.and_then(|x| f(x, "availability_pct")))),
            (
                "Reliability",
                summary.and_then(|x| f(x, "score")).map(|s| format!("{s:.0} / 100")).unwrap_or("-".into()),
            ),
        ],
    );
    let all: Vec<&Value> = input.targets.iter().collect();
    l.grid(&fleet_figures(input, &all, None));

    l.section("Items requiring attention");
    attention_table(&mut l, &items);

    let slots = day_slots(input);
    if !slots.is_empty() {
        l.section("Deployment activity \u{00b7} last 7 days");
        l.bars(&slots);
    }

    l.section("Sites");
    let rows: Vec<Vec<Cell>> = sites
        .iter()
        .map(|p| {
            let ts: Vec<&Value> = input.targets.iter().filter(|t| s(t, "project") == *p).collect();
            let running = ts.iter().filter(|t| t.get("running").is_some_and(|r| !r.is_null())).count();
            let failing = ts.iter().filter(|t| status(t).1 == ERR).count();
            let avail = input
                .analytics
                .get("projects")
                .and_then(Value::as_array)
                .and_then(|a| a.iter().find(|x| s(x, "name") == *p))
                .and_then(|x| f(x, "availability_pct"));
            vec![
                strong(*p),
                cell(ts.len().to_string()),
                cell(format!("{running} running")),
                colored(pct(avail), pct_color(avail)),
                if failing > 0 {
                    dotted(format!("{failing} failing"), ERR)
                } else {
                    dotted("Operational", OK)
                },
            ]
        })
        .collect();
    l.table(
        &[("Site", 0.34), ("Apps", 0.1), ("Running", 0.16), ("Avail. 7 d", 0.14), ("Status", 0.26)],
        &rows,
        1,
    );

    l.section("Applications");
    applications_table(&mut l, input, &all, true);

    l.section("Notable events \u{00b7} last 7 days");
    notable_events(&mut l, input, &all);

    l.signoff(&input.brand, NOTICE);
    l.finish(&footer(input), &hash)
}

/// Failed runs, rollbacks, crash restarts and critical incidents of the
/// period: the routine successful runs are left out.
fn notable_events(l: &mut Layout, input: &Input, targets: &[&Value]) {
    let names: HashMap<i64, &str> = targets.iter().map(|t| (i(t, "id"), s(t, "name"))).collect();
    let since = utc_secs(&input.generated_utc()).map(|now| now - 7 * 86_400).unwrap_or(0);
    let mut events: Vec<(i64, Vec<Cell>)> = Vec::new();
    for r in &input.recent_runs {
        let Some(name) = names.get(&r.target_id) else { continue };
        let at = utc_secs(&r.executed_at).unwrap_or(0);
        if at < since || r.label.as_deref().is_some_and(|l| !l.is_empty()) {
            continue;
        }
        let event = match (r.status.as_str(), r.triggered_by.as_str()) {
            ("FAILED", _) => dotted(format!("{} failed", trigger(&r.triggered_by)), ERR),
            (_, "ROLLBACK") => dotted("Rolled back", WARN),
            (_, "AUTO_RESTART") => dotted("Crash restart", WARN),
            _ => continue,
        };
        let detail = match (&r.commit_sha, r.exit_code) {
            (Some(sha), _) => format!(
                "{} {}",
                &sha[..sha.len().min(8)],
                r.commit_message.as_deref().unwrap_or("").lines().next().unwrap_or("")
            ),
            (None, Some(c)) => format!("exit code {c}"),
            _ => String::new(),
        };
        events.push((at, vec![cell(local_time(&r.executed_at)), strong(*name), event, colored(detail, SUB)]));
    }
    for inc in input.incidents.iter().filter(|x| x.severity == "critical") {
        let Some(name) = inc.target_id.and_then(|id| names.get(&id)) else { continue };
        events.push((
            utc_secs(&inc.created_at).unwrap_or(0),
            vec![
                cell(local_time(&inc.created_at)),
                strong(*name),
                dotted(format!("Incident: {}", kind_name(&inc.trigger)), ERR),
                colored(inc.title.clone(), SUB),
            ],
        ));
    }
    if events.is_empty() {
        l.paragraph(
            "No failed runs, rollbacks, crash restarts or critical incidents in the last 7 days.",
            OK,
            8.6,
        );
        return;
    }
    events.sort_by_key(|e| std::cmp::Reverse(e.0));
    let rows: Vec<Vec<Cell>> = events.into_iter().take(16).map(|(_, row)| row).collect();
    l.table(&[("When", 0.17), ("Application", 0.2), ("Event", 0.25), ("Detail", 0.38)], &rows, 1);
}

/// The site report: one project, a section per application.
pub fn project(input: &Input) -> Vec<u8> {
    let name = input.project.clone().unwrap_or_default();
    let (number, hash) = seal("SITE", input, &name);
    let mut l = Layout::new(
        &format!("{name} - site report"),
        "Site report no.",
        number,
        &input.brand,
        input.logo.as_ref(),
    );
    l.masthead(&input.brand, &long_date(&input.generated), &hash);
    l.title(
        "Site report",
        Some(&name),
        &format!(
            "The state of site {name}: its {} on {}, as recorded on {}. Availability, deployments and incidents \
             cover the last 7 days.",
            plural(input.targets.len() as i64, "application", "applications"),
            input.host,
            long_date(&input.generated)
        ),
    );
    let items = attention_rows(input);
    let (word, color, note) = verdict_of(&items);
    let site = input
        .analytics
        .get("projects")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|x| s(x, "name") == name));
    l.verdict(
        "Site status",
        word,
        color,
        &note,
        &[
            ("Availability 7 d", pct(site.and_then(|x| f(x, "availability_pct")))),
            (
                "Reliability",
                site.and_then(|x| f(x, "score")).map(|s| format!("{s:.0} / 100")).unwrap_or("-".into()),
            ),
        ],
    );
    let all: Vec<&Value> = input.targets.iter().collect();
    l.grid(&fleet_figures(input, &all, site));

    l.section("Items requiring attention");
    attention_table(&mut l, &items);

    l.section("Applications");
    applications_table(&mut l, input, &all, false);

    for t in &input.targets {
        application_section(&mut l, input, t);
    }

    l.signoff(&input.brand, NOTICE);
    l.finish(&footer(input), &hash)
}

fn application_section(l: &mut Layout, input: &Input, t: &Value) {
    let id = i(t, "id");
    // Keep the heading, the path line and the grid of figures on one page.
    l.need(330.0);
    l.section(s(t, "name"));
    let mode = if s(t, "run_mode") == "SERVICE" { "service" } else { "task" };
    let mut line = format!("{} \u{00b7} {} \u{00b7} {mode}", s(t, "working_directory"), s(t, "script_name"));
    let desc = s(t, "description");
    if !desc.is_empty() {
        line = format!("{desc} \u{2014} {line}");
    }
    l.paragraph(&line, SUB, 7.6);
    l.gap(4.0);

    let (st, sc) = status(t);
    let row = stats(input, id);
    let avail = row.and_then(|x| f(x, "availability_pct"));
    let p50 = row.and_then(|x| f(x, "latency_p50"));
    let health = if s(t, "health_url").is_empty() {
        fig("Availability \u{00b7} 7 days", "no health check", "", DIM)
    } else {
        fig(
            "Availability \u{00b7} 7 days",
            pct(avail),
            match p50 {
                Some(ms) => format!("typical response {ms:.0} ms \u{00b7} {}", s(t, "health_url")),
                None => s(t, "health_url").to_string(),
            },
            pct_color(avail),
        )
    };
    let (git, gc) = git_text(t);
    let last = t.get("last_run").filter(|r| !r.is_null());
    let (usage, usage_sub) = match input.usage_24h.get(&id) {
        Some((cpu, mem, peak)) if cpu.is_some() || mem.is_some() => (
            format!("CPU {}", cpu.map(|c| format!("{c:.1}%")).unwrap_or("-".into())),
            format!(
                "memory {} average, {} peak",
                mem.map(bytes).unwrap_or("-".into()),
                peak.map(bytes).unwrap_or("-".into())
            ),
        ),
        _ => ("no samples".into(), "nothing ran in the last 24 hours".into()),
    };
    let incidents: Vec<&Incident> = input.incidents.iter().filter(|x| x.target_id == Some(id)).collect();
    let open = incidents.iter().filter(|x| x.status == "open").count();
    let security = match scan_counts(t) {
        None => fig("Security", "not scanned", "run a scan from the Security page", WARN),
        Some((c, v)) => fig(
            "Security",
            if c + v == 0 {
                "clean".to_string()
            } else {
                format!("{c} critical \u{00b7} {v} vulnerable pkg")
            },
            t.get("scan").map(|x| format!("scanned {}", local_time(s(x, "scanned_at")))).unwrap_or_default(),
            if c > 0 {
                ERR
            } else if v > 0 {
                WARN
            } else {
                OK
            },
        ),
    };
    let last_fig = match last {
        None => fig("Last deployment", "never", "", DIM),
        Some(r) => {
            let ok = s(r, "status") == "SUCCESS";
            fig(
                "Last deployment",
                local_time(s(r, "executed_at")),
                format!(
                    "{} \u{00b7} {}",
                    if ok { "succeeded" } else { "failed" },
                    trigger(s(r, "triggered_by"))
                ),
                if ok { INK } else { ERR },
            )
        }
    };
    let next = t
        .get("next_run")
        .and_then(Value::as_i64)
        .and_then(|n| chrono::DateTime::from_timestamp(n, 0))
        .map(|d| format!("next run {}", d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")))
        .unwrap_or_default();
    l.grid(&[
        fig(
            "Status",
            st,
            if s(t, "run_mode") == "SERVICE" { "long-running service" } else { "runs to completion" },
            sc,
        ),
        health,
        fig(
            "Version",
            version_text(t),
            format!("branch {} \u{00b7} {git}", s(t, "git_branch")),
            if gc == OK { INK } else { gc },
        ),
        last_fig,
        fig("Resources \u{00b7} 24 hours", usage, usage_sub, INK),
        fig(
            "Incidents \u{00b7} 7 days",
            format!("{} \u{00b7} {open} open", incidents.len()),
            incidents.first().map(|x| format!("latest: {}", x.title)).unwrap_or_else(|| "none".into()),
            if open > 0 { ERR } else { INK },
        ),
        security,
        fig(
            "Schedule",
            t.get("schedule").and_then(Value::as_str).filter(|c| !c.is_empty()).unwrap_or("none"),
            next,
            INK,
        ),
    ]);

    let runs: Vec<&RunRecord> = input
        .runs_by_target
        .get(&id)
        .map(|r| r.iter().filter(|r| r.label.as_deref().is_none_or(str::is_empty)).take(5).collect())
        .unwrap_or_default();
    if !runs.is_empty() {
        l.subheading("Recent deployments", "newest first; quick commands left out");
        let rows: Vec<Vec<Cell>> = runs
            .iter()
            .map(|r| {
                let what = r
                    .commit_sha
                    .as_ref()
                    .map(|sha| {
                        format!(
                            "{} {}",
                            &sha[..sha.len().min(8)],
                            r.commit_message.as_deref().unwrap_or("").lines().next().unwrap_or("")
                        )
                    })
                    .unwrap_or_else(|| "-".into());
                vec![
                    cell(local_time(&r.executed_at)),
                    cell(trigger(&r.triggered_by)),
                    run_result(r),
                    cell(duration(r.duration_ms)),
                    mono(what),
                ]
            })
            .collect();
        l.table(
            &[("When", 0.18), ("Trigger", 0.14), ("Result", 0.18), ("Took", 0.1), ("Commit", 0.4)],
            &rows,
            1,
        );
    }
}

// --------------------------------------------------------------- security --

fn level_color(l: Level) -> Rgb {
    match l {
        Level::High => ERR,
        Level::Medium => WARN,
        Level::Low => INFO,
    }
}

/// The first line of a tool error, shortened: the full text is on the
/// target's Security tab.
fn brief(err: &str) -> String {
    let first = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = first.chars().take(220).collect();
    if out.len() < first.len() {
        out.push('\u{2026}');
    }
    format!("{out} (details on the Security tab)")
}

/// The security assessment: score, priority actions, PortalControl's own
/// configuration, attacks, and the findings of each application.
pub fn security(input: &Input) -> Vec<u8> {
    let posture = input.posture.clone().unwrap_or_else(|| crate::posture::assess(&Default::default(), &[]));
    let (number, hash) = seal("SEC", input, &format!("{}|{}", posture.score, posture.findings.len()));
    let mut l = Layout::new(
        "Security assessment report",
        "Assessment no.",
        number,
        &input.brand,
        input.logo.as_ref(),
    );
    l.masthead(&input.brand, &long_date(&input.generated), &hash);
    l.title(
        "Security assessment",
        None,
        &format!(
            "PortalControl's own configuration and the {} it supervises on {}, assessed on {}: code analysis \
             (bandit), known-vulnerable dependencies (pip-audit, with CVE references), configuration checks and the \
             attacks detected in the last 30 days.",
            plural(input.targets.len() as i64, "application", "applications"),
            input.host,
            long_date(&input.generated)
        ),
    );
    let grade_color = match posture.grade {
        "A" | "B" => OK,
        "C" => WARN,
        _ => ERR,
    };
    let scanned = input.scan_facts.len();
    let attacks: Vec<&Incident> = input
        .incidents
        .iter()
        .filter(|x| matches!(x.trigger.as_str(), "attack" | "brute_force" | "flood"))
        .collect();
    let note = if posture.findings.is_empty() {
        "No findings: nothing to improve was found.".to_string()
    } else {
        format!(
            "{} high, {} medium and {} low-priority finding(s), each with what to do.",
            posture.high, posture.medium, posture.low
        )
    };
    l.verdict(
        "Security posture",
        &format!("Score {} / 100 \u{00b7} grade {}", posture.score, posture.grade),
        grade_color,
        &note,
        &[
            ("Scanned", format!("{scanned} of {}", input.targets.len())),
            ("Attacks 30 d", attacks.len().to_string()),
        ],
    );

    let facts: Vec<&ScanFacts> = input.scan_facts.values().collect();
    let high: i64 = facts.iter().map(|f| f.high).sum();
    let medium: i64 = facts.iter().map(|f| f.medium).sum();
    let packages: usize = facts.iter().map(|f| f.packages.len()).sum();
    let advisories: usize = facts.iter().flat_map(|f| &f.packages).map(|p| p.advisories.len()).sum();
    let cves = facts
        .iter()
        .flat_map(|f| &f.packages)
        .flat_map(|p| &p.advisories)
        .filter(|a| a.starts_with("CVE-"))
        .count();
    let stale = facts.iter().filter(|f| f.age_days > crate::posture::STALE_DAYS).count();
    let open_attacks = attacks.iter().filter(|x| x.status == "open").count();
    let count_kind = |k: &str| attacks.iter().filter(|x| x.trigger == k).count();
    let config = posture
        .findings
        .iter()
        .filter(|f| {
            ["Debug mode", "Weak secret", "ALLOWED_HOSTS", "Secrets are committed", "Deploy webhook"]
                .iter()
                .any(|w| f.title.starts_with(w))
        })
        .count();
    let access = posture.checks.iter().find(|c| c.name == "Dashboard password");
    let exposure = posture.checks.iter().find(|c| c.name == "Network exposure");
    l.grid(&[
        fig(
            "Code issues",
            format!("{high} high \u{00b7} {medium} medium"),
            "static analysis of the applications' own code (bandit)",
            if high > 0 {
                ERR
            } else if medium > 0 {
                WARN
            } else {
                INK
            },
        ),
        fig(
            "Vulnerable packages",
            format!("{packages} packages \u{00b7} {advisories} advisories"),
            format!("{cves} with a CVE reference \u{00b7} pip-audit"),
            if packages > 0 { ERR } else { INK },
        ),
        fig(
            "Scan coverage",
            format!("{scanned} of {} scanned", input.targets.len()),
            format!("{} never scanned \u{00b7} {stale} older than 30 days", input.targets.len() - scanned),
            if scanned < input.targets.len() || stale > 0 { WARN } else { INK },
        ),
        fig(
            "Attacks \u{00b7} 30 days",
            format!("{} \u{00b7} {open_attacks} open", attacks.len()),
            format!(
                "{} intrusion attempts, {} password guessing, {} floods",
                count_kind("attack"),
                count_kind("brute_force"),
                count_kind("flood")
            ),
            if open_attacks > 0 {
                ERR
            } else if !attacks.is_empty() {
                WARN
            } else {
                INK
            },
        ),
        fig(
            "Dashboard access",
            if access.is_some_and(|c| c.ok) { "password set" } else { "no password" },
            exposure.map(|c| c.detail.clone()).unwrap_or_default(),
            if access.is_some_and(|c| c.ok) { INK } else { WARN },
        ),
        fig(
            "Configuration",
            plural(config as i64, "finding", "findings"),
            "debug mode, secret keys, allowed hosts, committed secrets, webhooks",
            if config > 0 { WARN } else { INK },
        ),
    ]);

    l.section("Priority actions");
    if posture.findings.is_empty() {
        l.paragraph("Nothing to improve was found.", OK, 8.6);
    } else {
        let rows: Vec<Vec<Cell>> = posture
            .findings
            .iter()
            .map(|f| {
                vec![
                    dotted(f.level.word(), level_color(f.level)),
                    strong(f.area.clone()),
                    strong(f.title.clone()).with_sub(f.detail.clone()),
                    cell(f.fix.clone()),
                ]
            })
            .collect();
        l.table(&[("Priority", 0.12), ("Area", 0.17), ("Finding", 0.39), ("What to do", 0.32)], &rows, 4);
    }

    l.section("PortalControl configuration");
    let rows: Vec<Vec<Cell>> = posture
        .checks
        .iter()
        .map(|c| {
            vec![
                strong(c.name),
                if c.ok { dotted("Pass", OK) } else { dotted("Attention", WARN) },
                colored(c.detail.clone(), SUB),
            ]
        })
        .collect();
    l.table(&[("Check", 0.36), ("Result", 0.16), ("Detail", 0.48)], &rows, 2);

    l.section("Threat activity \u{00b7} last 30 days");
    if attacks.is_empty() {
        l.paragraph(
            "No attacks were detected. Attack detection reads the requests in each application's output; a Django \
             project names the senders once PortalControl's request log is installed (Settings > AI Monitor).",
            SUB,
            8.2,
        );
    } else {
        let names: HashMap<i64, &str> = input.targets.iter().map(|t| (i(t, "id"), s(t, "name"))).collect();
        let rows: Vec<Vec<Cell>> = attacks
            .iter()
            .take(20)
            .map(|x| {
                vec![
                    cell(local_time(&x.created_at)),
                    strong(x.target_id.and_then(|id| names.get(&id).copied()).unwrap_or("removed")),
                    dotted(kind_name(&x.trigger), severity_color(&x.severity)),
                    cell(x.status.clone()),
                    cell(x.title.clone()),
                ]
            })
            .collect();
        l.table(
            &[
                ("When", 0.16),
                ("Application", 0.17),
                ("Kind", 0.19),
                ("Status", 0.1),
                ("What happened", 0.38),
            ],
            &rows,
            2,
        );
        if attacks.len() > 20 {
            l.paragraph(&format!("{} more on the Incidents page.", attacks.len() - 20), DIM, 8.0);
        }
    }

    l.section("Findings per application");
    for t in &input.targets {
        let id = i(t, "id");
        match input.scans.get(&id) {
            None => {
                l.subheading(s(t, "name"), "not scanned");
                l.paragraph("No scan yet. Run one from the Security page.", DIM, 8.2);
                l.gap(6.0);
            }
            Some(scan) => {
                l.subheading(
                    s(t, "name"),
                    &format!(
                        "scanned {}: {} critical issue(s), {} vulnerable package(s)",
                        local_time(&scan.scanned_at),
                        scan.critical_issues_found,
                        scan.vulnerable_packages_found
                    ),
                );
                scan_findings(&mut l, scan, input.scan_facts.get(&id), s(t, "working_directory"));
            }
        }
    }

    l.section("Method and scope");
    l.paragraph(
        "Code analysis runs bandit over each application's folder, leaving out virtual environments, node_modules and \
         generated files. Dependencies are checked with pip-audit against the Python Packaging Advisory Database and \
         OSV; advisories are shown by their CVE number where one exists. Configuration checks read the stored \
         environment variables and the project's .env file (values are checked, never printed), the repository index \
         for committed env files, and the webhook settings. Attacks are those PortalControl's monitor detected in the \
         applications' output. Score: 100 minus 15 per high, 6 per medium and 2 per low-priority finding; grade A from \
         90, B from 80, C from 70, D from 60.",
        SUB,
        7.8,
    );

    l.signoff(
        &input.brand,
        &format!("{NOTICE} A scan shows the problems known when it ran; it does not prove that an application is secure."),
    );
    l.finish(&footer(input), &hash)
}

fn scan_findings(l: &mut Layout, scan: &ScanReport, facts: Option<&ScanFacts>, folder: &str) {
    let payload = scan.raw_payload.clone().unwrap_or(Value::Null);
    let rank = |sev: &str| match sev {
        "HIGH" => 0,
        "MEDIUM" => 1,
        _ => 2,
    };
    let mut issues: Vec<&Value> = payload
        .get("bandit")
        .and_then(|b| b.get("results"))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    issues.sort_by_key(|r| rank(s(r, "issue_severity")));
    if let Some(err) = payload.get("bandit_error").and_then(Value::as_str) {
        l.paragraph(&format!("Code analysis did not run: {}", brief(err)), WARN, 8.0);
    } else if issues.is_empty() {
        l.paragraph("Code analysis (bandit): no issues found.", OK, 8.2);
    } else {
        let shown = issues.len().min(10);
        let rows: Vec<Vec<Cell>> = issues[..shown]
            .iter()
            .map(|r| {
                let sev = s(r, "issue_severity");
                let color = match sev {
                    "HIGH" => ERR,
                    "MEDIUM" => WARN,
                    _ => DIM,
                };
                // Paths relative to the project folder read better and fit.
                let full = s(r, "filename");
                let file = full
                    .strip_prefix(folder)
                    .map(|rest| rest.trim_start_matches(['/', '\\']))
                    .unwrap_or(full)
                    .trim_start_matches("./");
                vec![
                    dotted(sev.to_string(), color),
                    mono(s(r, "test_id")),
                    mono(format!("{file}:{}", i(r, "line_number"))),
                    cell(s(r, "issue_text")),
                ]
            })
            .collect();
        l.table(&[("Severity", 0.13), ("Rule", 0.09), ("Location", 0.3), ("Issue", 0.48)], &rows, 2);
        if issues.len() > shown {
            l.paragraph(&format!("{} more issue(s) on the Security tab.", issues.len() - shown), DIM, 8.0);
        }
    }
    if let Some(err) = payload.get("pip_audit_error").and_then(Value::as_str) {
        l.paragraph(&format!("Dependency audit did not run: {}", brief(err)), WARN, 8.0);
    } else {
        match facts.map(|f| &f.packages).filter(|p| !p.is_empty()) {
            None => l.paragraph("Dependency audit (pip-audit): no known-vulnerable packages.", OK, 8.2),
            Some(packages) => {
                let rows: Vec<Vec<Cell>> = packages
                    .iter()
                    .map(|p| {
                        vec![
                            strong(p.name.clone()),
                            mono(p.version.clone()),
                            mono(p.advisories.join(", ")),
                            match &p.fix {
                                Some(fx) => colored(format!("{fx} or later"), OK),
                                None => colored("no fix yet", WARN),
                            },
                        ]
                    })
                    .collect();
                l.table(
                    &[("Package", 0.2), ("Installed", 0.13), ("Advisories", 0.47), ("Fixed in", 0.2)],
                    &rows,
                    3,
                );
            }
        }
    }
    l.gap(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(id: i64, name: &str, project: &str, mode: &str, running: bool, last: Option<&str>) -> Value {
        json!({
            "id": id, "name": name, "project": project, "run_mode": mode,
            "running": if running { json!({"pid": 1}) } else { Value::Null },
            "last_run": last.map(|s| json!({"status": s, "executed_at": "2026-11-12 10:00:00", "triggered_by": "MANUAL"})),
            "health_url": if mode == "SERVICE" { "http://127.0.0.1:8000/" } else { "" },
            "health": {"status": "LIVE", "latency_ms": 3}, "uptime_pct": 99.5,
            "git": {"is_repo": true, "status": "UPDATES_AVAILABLE", "behind": 2, "head": "abcdef1234", "head_summary": "Fix"},
            "scan": {"critical_issues_found": 2, "vulnerable_packages_found": 5, "scanned_at": "2026-11-10 09:00:00"},
            "working_directory": "/srv/app", "script_name": "deploy.sh", "git_branch": "main",
            "env_count": 3, "env_vars": "SECRET=hunter2", "webhook_token": "tok123", "description": "Shop"
        })
    }

    fn input(project: Option<&str>) -> Input {
        Input {
            project: project.map(str::to_string),
            host: "host".into(),
            version: "0.14.0".into(),
            generated: "2026-11-12 20:00".into(),
            targets: vec![
                target(1, "web", "shop", "SERVICE", true, Some("SUCCESS")),
                target(2, "sync", "shop", "TASK", false, Some("FAILED")),
            ],
            recent_runs: vec![],
            runs_by_target: HashMap::new(),
            scans: HashMap::new(),
            usage_24h: HashMap::new(),
            brand: Brand::default(),
            logo: None,
            analytics: json!({
                "summary": {"availability_pct": 99.9, "runs": 10, "runs_failed": 1, "score": 93.0},
                "timeline": [
                    {"t": 1_762_900_000, "ok": 3, "failed": 1, "incidents": 2},
                    {"t": 1_762_986_400, "ok": 0, "failed": 0, "incidents": 0}
                ],
                "projects": [{"name": "shop", "availability_pct": 99.9}],
                "targets": [{"id": 1, "availability_pct": 99.9, "latency_p50": 12.0}]
            }),
            incidents: vec![],
            posture: None,
            scan_facts: HashMap::new(),
        }
    }

    fn page_count(bytes: &[u8]) -> usize {
        let text = String::from_utf8_lossy(bytes);
        text.split("/Count ").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap()
    }

    #[test]
    fn status_matches_the_dashboard() {
        assert_eq!(status(&target(1, "a", "p", "SERVICE", true, None)).0, "Running (healthy)");
        assert_eq!(status(&target(1, "a", "p", "TASK", false, Some("FAILED"))).0, "Last deploy failed");
        assert_eq!(status(&target(1, "a", "p", "TASK", false, None)).0, "Never deployed");
    }

    #[test]
    fn attention_lists_failures_findings_updates_and_open_incidents() {
        let mut inp = input(None);
        inp.incidents = vec![Incident {
            target_id: Some(1),
            status: "open".into(),
            severity: "critical".into(),
            ..Default::default()
        }];
        let items = attention_rows(&inp);
        let text: Vec<&str> = items.iter().map(|(_, t)| t.as_str()).collect();
        assert!(text.iter().any(|t| t.contains("sync: last deploy failed")));
        assert!(text.iter().any(|t| t.contains("2 critical")));
        assert!(text.iter().any(|t| t.contains("2 new commit")));
        assert!(text.iter().any(|t| t.contains("web: 1 open incident")));
        assert_eq!(items[0].0, ERR, "critical first");
        assert_eq!(verdict_of(&items).0, "Action required");
        assert_eq!(verdict_of(&[]).0, "All systems operational");
    }

    #[test]
    fn reports_are_pdfs_without_secrets() {
        assert!(
            String::from_utf8_lossy(&overall(&input(None))).contains("INSIGHTFUSIONTECH"),
            "the default organisation"
        );
        let mut sec = input(None);
        sec.posture = Some(crate::posture::assess(&Default::default(), &[]));
        for bytes in [overall(&input(None)), project(&input(Some("shop"))), security(&sec)] {
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.starts_with("%PDF-1.4") && text.trim_end().ends_with("%%EOF"));
            assert!(!text.contains("hunter2") && !text.contains("tok123"), "no secrets in a report");
            assert!(text.contains("SHA-256"), "the seal is in the footer");
        }
    }

    #[test]
    fn the_brand_and_logo_appear_and_the_number_is_stable() {
        let mut inp = input(None);
        inp.brand =
            Brand { org_name: "Example Org".into(), prepared_by: "A. Reviewer".into(), ..Default::default() };
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x40, 0x03, 0x01, 0x11, 0x00, 0xFF,
            0xD9,
        ];
        inp.logo = Some(Jpeg::parse(&jpeg).unwrap());
        let text = String::from_utf8_lossy(&overall(&inp)).into_owned();
        assert!(text.contains("/Subtype /Image"));
        assert!(text.contains("EXAMPLE ORG") && text.contains("A. Reviewer"));
        let (a, _) = seal("OPS", &inp, "");
        let (b, _) = seal("OPS", &inp, "");
        assert_eq!(a, b);
        assert!(a.starts_with("OPS-20261112-2000-"), "{a}");
        assert!(Brand { note: "x".repeat(401), ..Default::default() }.clean().is_err());
    }

    #[test]
    fn long_tables_break_onto_new_pages() {
        let mut inp = input(None);
        inp.targets =
            (0..120).map(|n| target(n, &format!("t{n}"), "p", "TASK", false, Some("SUCCESS"))).collect();
        assert!(page_count(&overall(&inp)) >= 4);
    }

    #[test]
    fn security_findings_list_cves() {
        let mut inp = input(None);
        let payload = json!({"pip_audit": {"dependencies": [
            {"name": "django", "version": "4.2.0", "vulns": [{"id": "PYSEC-1", "aliases": ["CVE-2024-0001"], "fix_versions": ["4.2.9"]}]}
        ]}});
        inp.scans.insert(
            1,
            ScanReport {
                id: 1,
                target_id: 1,
                critical_issues_found: 0,
                vulnerable_packages_found: 1,
                scanned_at: "2026-11-10 09:00:00".into(),
                raw_payload: Some(payload.clone()),
            },
        );
        inp.scan_facts.insert(1, crate::posture::scan_facts(&payload, "2026-11-10 09:00:00", 0));
        inp.posture = Some(crate::posture::assess(&Default::default(), &[]));
        let text = String::from_utf8_lossy(&security(&inp)).into_owned();
        assert!(text.contains("CVE-2024-0001") && text.contains("4.2.9 or later"));
    }
}
