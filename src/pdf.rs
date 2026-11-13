//! A small PDF writer for the reports: pages, text in Helvetica and
//! Helvetica-Bold, filled rectangles and lines. Those two fonts are among the
//! 14 every PDF reader has built in, so nothing is embedded and no font or PDF
//! library is linked: the output is plain PDF 1.4 text.
//!
//! Coordinates are in points (1/72 inch) measured from the **top-left** of an
//! A4 page; they are flipped to PDF's bottom-left origin when written.

use std::fmt::Write as _;

pub const PAGE_W: f64 = 595.28;
pub const PAGE_H: f64 = 841.89;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn hex(s: &str) -> Rgb {
        let v = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0);
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
    fn ops(self) -> String {
        format!("{:.3} {:.3} {:.3}", self.0 as f64 / 255.0, self.1 as f64 / 255.0, self.2 as f64 / 255.0)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Font {
    Regular,
    Bold,
}

impl Font {
    fn res(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
        }
    }
}

/// Advance widths (per 1000 em) of printable ASCII, 32..=126, from the
/// standard Adobe font metrics.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556,
    556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278,
    500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469,
    556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500,
    278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556,
    556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667, 611, 778, 722, 278,
    556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584,
    556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611, 611, 389, 556,
    333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

/// Map a character to its WinAnsi byte (the encoding the fonts are set to);
/// characters that encoding lacks become `?`.
fn win_ansi(c: char) -> u8 {
    match c {
        ' '..='~' => c as u8,
        '\u{a0}'..='\u{ff}' => c as u32 as u8,
        '\u{2022}' => 0x95, // bullet
        '\u{2013}' => 0x96, // en dash
        '\u{2014}' => 0x97, // em dash
        '\u{2026}' => 0x85, // ellipsis
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201c}' => 0x93,
        '\u{201d}' => 0x94,
        '\u{2192}' => b'>', // arrow
        '\u{20ac}' => 0x80,
        _ => b'?',
    }
}

/// Width of `text` in points at `size`.
pub fn text_width(text: &str, font: Font, size: f64) -> f64 {
    let table = match font {
        Font::Regular => &HELVETICA,
        Font::Bold => &HELVETICA_BOLD,
    };
    let units: u32 = text
        .chars()
        .map(|c| match win_ansi(c) {
            b @ 32..=126 => table[(b - 32) as usize] as u32,
            _ => 556,
        })
        .sum();
    units as f64 * size / 1000.0
}

/// Shorten `text` with an ellipsis so it fits in `max` points.
pub fn fit(text: &str, font: Font, size: f64, max: f64) -> String {
    if text_width(text, font, size) <= max {
        return text.to_string();
    }
    let mut out: String = text.to_string();
    while !out.is_empty() && text_width(&format!("{out}\u{2026}"), font, size) > max {
        out.pop();
    }
    format!("{}\u{2026}", out.trim_end())
}

/// Break `text` into lines no wider than `max` points.
pub fn wrap(text: &str, font: Font, size: f64, max: f64) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if text_width(&candidate, font, size) <= max || line.is_empty() {
                line = candidate;
            } else {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            }
            while text_width(&line, font, size) > max && line.chars().count() > 1 {
                // A single word longer than the line: cut it.
                let mut head = String::new();
                for c in line.chars() {
                    if text_width(&format!("{head}{c}"), font, size) > max {
                        break;
                    }
                    head.push(c);
                }
                let rest: String = line.chars().skip(head.chars().count().max(1)).collect();
                lines.push(head);
                line = rest;
            }
        }
        lines.push(line);
    }
    lines
}

fn escape(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        match win_ansi(c) {
            b @ (b'(' | b')' | b'\\') => out.extend([b'\\', b]),
            b => out.push(b),
        }
    }
    out
}

pub struct Page {
    ops: Vec<u8>,
}

impl Page {
    fn push(&mut self, s: &str) {
        self.ops.extend_from_slice(s.as_bytes());
    }

    pub fn text(&mut self, x: f64, y: f64, text: &str, font: Font, size: f64, color: Rgb) {
        // `y` is the top of the text box; the baseline sits about 0.8 em lower.
        let base = PAGE_H - y - size * 0.8;
        self.push(&format!("BT {} rg /{} {size:.2} Tf {x:.2} {base:.2} Td (", color.ops(), font.res()));
        let esc = escape(text);
        self.ops.extend_from_slice(&esc);
        self.push(") Tj ET\n");
    }

    pub fn text_right(&mut self, right: f64, y: f64, text: &str, font: Font, size: f64, color: Rgb) {
        let w = text_width(text, font, size);
        self.text(right - w, y, text, font, size, color);
    }

    pub fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, fill: Rgb) {
        self.push(&format!("{} rg {x:.2} {:.2} {w:.2} {h:.2} re f\n", fill.ops(), PAGE_H - y - h));
    }

    /// The path of a circle, as four Bezier curves.
    fn circle_path(&mut self, cx: f64, cy: f64, r: f64) {
        let k = 0.5523 * r;
        let cy = PAGE_H - cy;
        self.push(&format!(
            "{:.2} {cy:.2} m {:.2} {:.2} {:.2} {:.2} {cx:.2} {:.2} c {:.2} {:.2} {:.2} {:.2} {:.2} {cy:.2} c \
             {:.2} {:.2} {:.2} {:.2} {cx:.2} {:.2} c {:.2} {:.2} {:.2} {:.2} {:.2} {cy:.2} c ",
            cx + r,
            cx + r, cy + k, cx + k, cy + r, cy + r,
            cx - k, cy + r, cx - r, cy + k, cx - r,
            cx - r, cy - k, cx - k, cy - r, cy - r,
            cx + k, cy - r, cx + r, cy - k, cx + r,
        ));
    }

    pub fn circle(&mut self, cx: f64, cy: f64, r: f64, fill: Rgb) {
        self.push(&format!("{} rg ", fill.ops()));
        self.circle_path(cx, cy, r);
        self.push("f\n");
    }

    pub fn ring(&mut self, cx: f64, cy: f64, r: f64, width: f64, color: Rgb) {
        self.push(&format!("{} RG {width:.2} w ", color.ops()));
        self.circle_path(cx, cy, r);
        self.push("S\n");
    }

    pub fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, width: f64, color: Rgb) {
        self.push(&format!(
            "{} RG {width:.2} w {x1:.2} {:.2} m {x2:.2} {:.2} l S\n",
            color.ops(),
            PAGE_H - y1,
            PAGE_H - y2
        ));
    }
}

#[derive(Default)]
pub struct Document {
    pages: Vec<Page>,
    title: String,
}

impl Document {
    pub fn new(title: &str) -> Self {
        Self { pages: Vec::new(), title: title.to_string() }
    }

    pub fn add_page(&mut self) -> &mut Page {
        self.pages.push(Page { ops: Vec::new() });
        self.pages.last_mut().expect("just pushed")
    }

    pub fn page(&mut self, i: usize) -> &mut Page {
        &mut self.pages[i]
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Serialize to PDF bytes.
    pub fn finish(self) -> Vec<u8> {
        let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets: Vec<usize> = Vec::new();
        let n = self.pages.len();
        // Object numbers: 1 catalog, 2 pages, 3-4 fonts, 5 info, then (page, content) pairs.
        let page_obj = |i: usize| 6 + i * 2;
        let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]| {
            offsets.push(out.len());
            let num = offsets.len();
            out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        };
        obj(&mut out, &mut offsets, b"<< /Type /Catalog /Pages 2 0 R >>");
        let kids: String = (0..n).map(|i| format!("{} 0 R ", page_obj(i))).collect();
        obj(&mut out, &mut offsets, format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").as_bytes());
        for base in ["Helvetica", "Helvetica-Bold"] {
            obj(
                &mut out,
                &mut offsets,
                format!("<< /Type /Font /Subtype /Type1 /BaseFont /{base} /Encoding /WinAnsiEncoding >>")
                    .as_bytes(),
            );
        }
        let mut info = b"<< /Producer (PortalControl) /Title (".to_vec();
        info.extend(escape(&self.title));
        info.extend_from_slice(b") >>");
        obj(&mut out, &mut offsets, &info);
        for (i, page) in self.pages.into_iter().enumerate() {
            let page_body = format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
                page_obj(i) + 1
            );
            obj(&mut out, &mut offsets, page_body.as_bytes());
            let mut stream = format!("<< /Length {} >>\nstream\n", page.ops.len()).into_bytes();
            stream.extend_from_slice(&page.ops);
            stream.extend_from_slice(b"\nendstream");
            obj(&mut out, &mut offsets, &stream);
        }
        let xref_at = out.len();
        let mut xref = format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1);
        for off in &offsets {
            let _ = writeln!(xref, "{off:010} 00000 n ");
        }
        let _ = write!(
            xref,
            "trailer\n<< /Size {} /Root 1 0 R /Info 5 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            offsets.len() + 1
        );
        out.extend_from_slice(xref.as_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_like_helvetica() {
        // "Hello": H 722 + e 556 + l 222 + l 222 + o 556 = 2278 units.
        assert!((text_width("Hello", Font::Regular, 10.0) - 22.78).abs() < 1e-9);
        assert!(text_width("Hello", Font::Bold, 10.0) > text_width("Hello", Font::Regular, 10.0));
    }

    #[test]
    fn wraps_and_fits() {
        let lines = wrap("the quick brown fox jumps over the lazy dog", Font::Regular, 10.0, 80.0);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| text_width(l, Font::Regular, 10.0) <= 80.0));
        let long = wrap("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", Font::Regular, 10.0, 50.0);
        assert!(long.iter().all(|l| text_width(l, Font::Regular, 10.0) <= 50.0));
        let short = fit("a very long target name indeed", Font::Bold, 9.0, 60.0);
        assert!(short.ends_with('\u{2026}') && text_width(&short, Font::Bold, 9.0) <= 60.0);
    }

    #[test]
    fn escapes_and_encodes() {
        assert_eq!(escape("a(b)\\c"), b"a\\(b\\)\\\\c".to_vec());
        assert_eq!(escape("\u{2014}\u{4e2d}"), vec![0x97, b'?']);
    }

    #[test]
    fn writes_a_valid_document_structure() {
        let mut doc = Document::new("Test (report)");
        let p = doc.add_page();
        p.text(40.0, 40.0, "Hello", Font::Bold, 12.0, Rgb::hex("#7c3aed"));
        p.rect(40.0, 60.0, 100.0, 20.0, Rgb(240, 240, 240));
        doc.add_page().line(40.0, 40.0, 200.0, 40.0, 1.0, Rgb(0, 0, 0));
        let bytes = doc.finish();
        assert!(bytes.starts_with(b"%PDF-1.4"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        let find = |needle: &[u8]| bytes.windows(needle.len()).rposition(|w| w == needle).unwrap();
        assert!(find(b"/Count 2") > 0);
        // Every xref offset points at the start of its object (byte offsets).
        let sx = find(b"startxref\n") + b"startxref\n".len();
        let xref_at: usize =
            std::str::from_utf8(&bytes[sx..]).unwrap().lines().next().unwrap().parse().unwrap();
        let table = std::str::from_utf8(&bytes[xref_at..]).unwrap();
        let entries: Vec<&str> = table.lines().skip(3).take_while(|l| l.ends_with(" n ")).collect();
        assert_eq!(entries.len(), 9, "catalog, pages, 2 fonts, info, 2 x (page, content)");
        for (n, line) in entries.iter().enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(
                bytes[off..].starts_with(format!("{} 0 obj", n + 1).as_bytes()),
                "object {} offset",
                n + 1
            );
        }
    }
}
