//! A small PDF writer for the reports: pages, text in Helvetica,
//! Helvetica-Bold and Courier, filled and outlined rectangles, lines, circles
//! and JPEG images (the report logo). Those fonts are among the 14 every PDF
//! reader has built in, so nothing is embedded and no font or PDF library is
//! linked: the output is plain PDF 1.4. A JPEG is stored as it is (DCTDecode),
//! so no image library is needed either.
//!
//! Coordinates are in points (1/72 inch) measured from the **top-left** of an
//! A4 page; they are flipped to PDF's bottom-left origin when written.

use std::fmt::Write as _;

pub const PAGE_W: f64 = 595.28;
pub const PAGE_H: f64 = 841.89;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    fn ops(self) -> String {
        format!("{:.3} {:.3} {:.3}", self.0 as f64 / 255.0, self.1 as f64 / 255.0, self.2 as f64 / 255.0)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Font {
    Regular,
    Bold,
    /// Courier: every character is 0.6 em wide. For codes and hashes.
    Mono,
}

impl Font {
    fn res(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
            Font::Mono => "F3",
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
        Font::Mono => return text.chars().count() as f64 * 0.6 * size,
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

/// Width of `text` drawn with `spacing` points between characters.
pub fn spaced_width(text: &str, font: Font, size: f64, spacing: f64) -> f64 {
    text_width(text, font, size) + spacing * text.chars().count().saturating_sub(1) as f64
}

/// A JPEG image as it is stored in the PDF. Only baseline and progressive
/// JPEGs with 1 (grey) or 3 (colour) components are accepted; a browser's
/// canvas writes those.
#[derive(Clone, Debug)]
pub struct Jpeg {
    data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    components: u8,
}

impl Jpeg {
    /// Read the size and colour components from the frame header.
    pub fn parse(bytes: &[u8]) -> Result<Jpeg, String> {
        if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
            return Err("not a JPEG file".into());
        }
        let mut i = 2;
        while i + 4 <= bytes.len() {
            if bytes[i] != 0xFF {
                return Err("damaged JPEG file".into());
            }
            let marker = bytes[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            let is_frame = matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF);
            if is_frame {
                if i + 10 > bytes.len() {
                    break;
                }
                let height = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                let width = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                let components = bytes[i + 9];
                if !matches!(marker, 0xC0..=0xC2) {
                    return Err("this kind of JPEG (lossless or arithmetic coded) is not supported".into());
                }
                if components != 1 && components != 3 {
                    return Err("only grey or RGB JPEGs are supported (not CMYK)".into());
                }
                if width == 0 || height == 0 {
                    return Err("the JPEG has no size".into());
                }
                return Ok(Jpeg { data: bytes.to_vec(), width, height, components });
            }
            if marker == 0xDA || marker == 0xD9 {
                break;
            }
            i += 2 + len;
        }
        Err("no image size found in the JPEG".into())
    }
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
        self.text_spaced(x, y, text, font, size, color, 0.0);
    }

    /// Text with `spacing` points added after each character (letter spacing),
    /// for the small capital labels.
    #[allow(clippy::too_many_arguments)]
    pub fn text_spaced(
        &mut self,
        x: f64,
        y: f64,
        text: &str,
        font: Font,
        size: f64,
        color: Rgb,
        spacing: f64,
    ) {
        // `y` is the top of the text box; the baseline sits about 0.8 em lower.
        let base = PAGE_H - y - size * 0.8;
        self.push(&format!(
            "BT {} rg /{} {size:.2} Tf {spacing:.2} Tc {x:.2} {base:.2} Td (",
            color.ops(),
            font.res()
        ));
        let esc = escape(text);
        self.ops.extend_from_slice(&esc);
        self.push(") Tj ET\n");
    }

    pub fn text_center(&mut self, center: f64, y: f64, text: &str, font: Font, size: f64, color: Rgb) {
        let w = text_width(text, font, size);
        self.text(center - w / 2.0, y, text, font, size, color);
    }

    /// The outline of a rectangle.
    pub fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, width: f64, color: Rgb) {
        self.push(&format!(
            "{} RG {width:.2} w {x:.2} {:.2} {w:.2} {h:.2} re S\n",
            color.ops(),
            PAGE_H - y - h
        ));
    }

    /// Image number `index` (from `Document::add_jpeg`) in the box at x, y.
    pub fn image(&mut self, index: usize, x: f64, y: f64, w: f64, h: f64) {
        self.push(&format!("q {w:.2} 0 0 {h:.2} {x:.2} {:.2} cm /Im{index} Do Q\n", PAGE_H - y - h));
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
    images: Vec<Jpeg>,
}

impl Document {
    pub fn new(title: &str) -> Self {
        Self { pages: Vec::new(), title: title.to_string(), images: Vec::new() }
    }

    /// Add an image that pages can draw with `Page::image`; returns its number.
    pub fn add_jpeg(&mut self, image: Jpeg) -> usize {
        self.images.push(image);
        self.images.len() - 1
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
        let images = self.images.len();
        // Object numbers: 1 catalog, 2 pages, 3-5 fonts, 6 info, then the
        // images, then (page, content) pairs.
        let image_obj = |i: usize| 7 + i;
        let page_obj = |i: usize| 7 + images + i * 2;
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
        for base in ["Helvetica", "Helvetica-Bold", "Courier"] {
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
        for img in &self.images {
            let space = if img.components == 1 { "DeviceGray" } else { "DeviceRGB" };
            let mut body = format!(
                "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /{space} \
                 /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
                img.width,
                img.height,
                img.data.len()
            )
            .into_bytes();
            body.extend_from_slice(&img.data);
            body.extend_from_slice(b"\nendstream");
            obj(&mut out, &mut offsets, &body);
        }
        let xobjects: String = (0..images).map(|i| format!("/Im{i} {} 0 R ", image_obj(i))).collect();
        let xobjects = if images > 0 { format!(" /XObject << {xobjects}>>") } else { String::new() };
        for (i, page) in self.pages.into_iter().enumerate() {
            let page_body = format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R /F3 5 0 R >>{xobjects} >> /Contents {} 0 R >>",
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
            "trailer\n<< /Size {} /Root 1 0 R /Info 6 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
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

    /// The smallest JPEG header that names a size: SOI, an APP0 segment, SOF0.
    fn tiny_jpeg(w: u16, h: u16, components: u8) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08];
        b.extend(h.to_be_bytes());
        b.extend(w.to_be_bytes());
        b.extend([components, 0x01, 0x11, 0x00, 0xFF, 0xD9]);
        b
    }

    #[test]
    fn reads_jpeg_sizes_and_refuses_other_files() {
        let j = Jpeg::parse(&tiny_jpeg(320, 80, 3)).unwrap();
        assert_eq!((j.width, j.height, j.components), (320, 80, 3));
        assert!(Jpeg::parse(b"\x89PNG\r\n").is_err());
        assert!(Jpeg::parse(&tiny_jpeg(10, 10, 4)).unwrap_err().contains("CMYK"));
    }

    #[test]
    fn images_and_the_mono_font_are_referenced_from_every_page() {
        let mut doc = Document::new("Logo");
        let img = doc.add_jpeg(Jpeg::parse(&tiny_jpeg(40, 20, 3)).unwrap());
        doc.add_page().image(img, 40.0, 40.0, 80.0, 40.0);
        doc.add_page().text(40.0, 40.0, "ABC123", Font::Mono, 9.0, Rgb(0, 0, 0));
        let text = String::from_utf8_lossy(&doc.finish()).into_owned();
        assert!(text.contains("/Filter /DCTDecode") && text.contains("/Width 40 /Height 20"));
        assert_eq!(text.matches("/XObject << /Im0 7 0 R >>").count(), 2);
        assert!(text.contains("/BaseFont /Courier"));
        assert!((text_width("ABCD", Font::Mono, 10.0) - 24.0).abs() < 1e-9);
    }

    #[test]
    fn writes_a_valid_document_structure() {
        let mut doc = Document::new("Test (report)");
        let p = doc.add_page();
        p.text(40.0, 40.0, "Hello", Font::Bold, 12.0, Rgb(0x7c, 0x3a, 0xed));
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
        assert_eq!(entries.len(), 10, "catalog, pages, 3 fonts, info, 2 x (page, content)");
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
