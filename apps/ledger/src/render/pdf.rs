//! The smallest PDF writer the customer documents need (D39): US Letter
//! pages of text, rules, filled boxes, one raster logo and link
//! annotations, in the fourteen standard fonts so nothing is embedded.
//!
//! Text is Helvetica in three faces, encoded WinAnsi. Widths come from
//! Adobe's published Helvetica metrics ([`char_width`]), which is what
//! right-aligned money columns and word wrap need; a viewer substitutes its
//! own Helvetica (Arial on most systems) with the same widths. A character
//! outside WinAnsi prints as `?` rather than as something the viewer would
//! garble.
//!
//! Content streams are written uncompressed so [`extract_text`] can read a
//! rendered document back without a PDF library — the tests prove totals,
//! numbers and VOID from the bytes, not from the layout's own bookkeeping.
//! Only the logo is deflated, with `miniz_oxide`, which also inflates the
//! PNG it comes from ([`decode_png`]).

use std::fmt::Write as _;

pub const PAGE_WIDTH: f32 = 612.0;
pub const PAGE_HEIGHT: f32 = 792.0;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Color(pub f32, pub f32, pub f32);

impl Color {
    pub const BLACK: Color = Color(0.0, 0.0, 0.0);

    /// `0x1B3A5B` → navy.
    pub const fn hex(rgb: u32) -> Color {
        Color(
            ((rgb >> 16) & 0xff) as f32 / 255.0,
            ((rgb >> 8) & 0xff) as f32 / 255.0,
            (rgb & 0xff) as f32 / 255.0,
        )
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Font {
    Regular,
    Bold,
    Oblique,
}

impl Font {
    const fn resource(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
            Font::Oblique => "F3",
        }
    }

    const fn base_font(self) -> &'static str {
        match self {
            Font::Regular => "Helvetica",
            Font::Bold => "Helvetica-Bold",
            Font::Oblique => "Helvetica-Oblique",
        }
    }
}

const FONTS: [Font; 3] = [Font::Regular, Font::Bold, Font::Oblique];

/// Helvetica advance widths, 1/1000 em, for ASCII 32..=126 (Adobe AFM).
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278,
    278, // space../
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // 0..9
    278, 278, 584, 584, 584, 556, 1015, // :..@
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, // A..M
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, // N..Z
    278, 278, 278, 469, 556, 333, // [..`
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, // a..m
    556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, // n..z
    334, 260, 334, 584, // {..~
];

/// Helvetica-Bold, same range.
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, //
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, //
    333, 333, 584, 584, 584, 611, 975, //
    722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, //
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, //
    333, 278, 333, 584, 556, 333, //
    556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, //
    611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, //
    389, 280, 389, 584,
];

/// The WinAnsi byte for `c`, or `None` when the encoding has no such glyph.
fn win_ansi(c: char) -> Option<u8> {
    let code = c as u32;
    match code {
        32..=126 | 0xA0..=0xFF => Some(code as u8),
        _ => Some(match c {
            '€' => 0x80,
            '‚' => 0x82,
            '„' => 0x84,
            '…' => 0x85,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            '–' => 0x96,
            '—' => 0x97,
            '™' => 0x99,
            _ => return None,
        }),
    }
}

/// The character a WinAnsi byte stands for; the inverse of [`win_ansi`].
fn from_win_ansi(byte: u8) -> char {
    match byte {
        0x80 => '€',
        0x82 => '‚',
        0x84 => '„',
        0x85 => '…',
        0x91 => '‘',
        0x92 => '’',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x99 => '™',
        other => other as char,
    }
}

/// What a string becomes on the page: control characters as spaces, and
/// anything WinAnsi cannot carry as `?`.
fn encode(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| {
            if c.is_control() {
                b' '
            } else {
                win_ansi(c).unwrap_or(b'?')
            }
        })
        .collect()
}

fn byte_width(font: Font, byte: u8) -> u16 {
    let table = match font {
        Font::Bold => &HELVETICA_BOLD,
        Font::Regular | Font::Oblique => &HELVETICA,
    };
    match byte {
        32..=126 => table[(byte - 32) as usize],
        0x95 => 350,
        0x97 => 1000,
        0x85 => 1000,
        0x80 | 0x96 => 556,
        0x91 | 0x92 | 0x82 => 222,
        0x93 | 0x94 | 0x84 => 333,
        0x99 => 1000,
        // Accented Latin-1 letters are within a few units of the base letter.
        _ => 556,
    }
}

/// The advance width of one character, 1/1000 em.
pub fn char_width(font: Font, c: char) -> u16 {
    byte_width(font, win_ansi(c).unwrap_or(b'?'))
}

/// Width of `text` set in `font` at `size` points.
pub fn text_width(font: Font, size: f32, text: &str) -> f32 {
    let units: u32 = encode(text)
        .iter()
        .map(|&b| u32::from(byte_width(font, b)))
        .sum();
    units as f32 * size / 1000.0
}

/// Greedy word wrap to `max_width`; a word longer than the line is broken
/// by character. Explicit newlines start a new line. Never returns an empty
/// vector.
pub fn wrap(font: Font, size: f32, text: &str, max_width: f32) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if text_width(font, size, &candidate) <= max_width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            // The word alone; break it if it still does not fit.
            let mut piece = String::new();
            for c in word.chars() {
                piece.push(c);
                if text_width(font, size, &piece) > max_width && piece.chars().count() > 1 {
                    piece.pop();
                    out.push(std::mem::take(&mut piece));
                    piece.push(c);
                }
            }
            line = piece;
        }
        out.push(line);
    }
    out
}

fn num(value: f32) -> String {
    let mut s = format!("{value:.2}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s == "-0" {
        s = "0".to_string();
    }
    s
}

fn literal(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('(');
    for &b in bytes {
        match b {
            b'(' | b')' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            32..=126 => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{b:03o}");
            }
        }
    }
    out.push(')');
    out
}

/// A clickable rectangle that opens a URI.
#[derive(Clone, Debug, PartialEq)]
struct Link {
    rect: [f32; 4],
    uri: String,
}

/// One page's drawing operations, in PDF user space (points, origin at the
/// bottom left).
#[derive(Clone, Debug, Default)]
pub struct Page {
    ops: String,
    links: Vec<Link>,
    images: Vec<usize>,
}

impl Page {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&mut self, x: f32, y: f32, font: Font, size: f32, color: Color, text: &str) {
        self.text_matrix([1.0, 0.0, 0.0, 1.0, x, y], font, size, color, text);
    }

    pub fn text_right(
        &mut self,
        right: f32,
        y: f32,
        font: Font,
        size: f32,
        color: Color,
        text: &str,
    ) {
        let x = right - text_width(font, size, text);
        self.text(x, y, font, size, color, text);
    }

    pub fn text_center(
        &mut self,
        center: f32,
        y: f32,
        font: Font,
        size: f32,
        color: Color,
        text: &str,
    ) {
        let x = center - text_width(font, size, text) / 2.0;
        self.text(x, y, font, size, color, text);
    }

    /// Text whose baseline runs at `degrees` counter-clockwise, centred on
    /// `center` — the VOID watermark.
    pub fn text_rotated_center(
        &mut self,
        center: (f32, f32),
        degrees: f32,
        font: Font,
        size: f32,
        color: Color,
        text: &str,
    ) {
        let (cx, cy) = center;
        let (sin, cos) = degrees.to_radians().sin_cos();
        let w = text_width(font, size, text);
        // Shift back by half the width along the baseline and by a third of
        // the size across it, so the word's middle lands on the point.
        let (dx, dy) = (w / 2.0, size * 0.35);
        let x = cx - dx * cos + dy * sin;
        let y = cy - dx * sin - dy * cos;
        self.text_matrix([cos, sin, -sin, cos, x, y], font, size, color, text);
    }

    fn text_matrix(&mut self, m: [f32; 6], font: Font, size: f32, color: Color, text: &str) {
        let Color(r, g, b) = color;
        let _ = writeln!(
            self.ops,
            "BT /{} {} Tf {} {} {} rg {} {} {} {} {} {} Tm {} Tj ET",
            font.resource(),
            num(size),
            num(r),
            num(g),
            num(b),
            num(m[0]),
            num(m[1]),
            num(m[2]),
            num(m[3]),
            num(m[4]),
            num(m[5]),
            literal(&encode(text)),
        );
    }

    pub fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Color) {
        let Color(r, g, b) = color;
        let _ = writeln!(
            self.ops,
            "{} {} {} RG {} w {} {} m {} {} l S",
            num(r),
            num(g),
            num(b),
            num(width),
            num(x1),
            num(y1),
            num(x2),
            num(y2),
        );
    }

    pub fn rect_fill(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let Color(r, g, b) = color;
        let _ = writeln!(
            self.ops,
            "{} {} {} rg {} {} {} {} re f",
            num(r),
            num(g),
            num(b),
            num(x),
            num(y),
            num(w),
            num(h),
        );
    }

    pub fn rect_stroke(&mut self, x: f32, y: f32, w: f32, h: f32, width: f32, color: Color) {
        let Color(r, g, b) = color;
        let _ = writeln!(
            self.ops,
            "{} {} {} RG {} w {} {} {} {} re S",
            num(r),
            num(g),
            num(b),
            num(width),
            num(x),
            num(y),
            num(w),
            num(h),
        );
    }

    /// Draws image `index` of the document (see [`PdfDocument::add_image`])
    /// with its bottom-left corner at (`x`, `y`).
    pub fn image(&mut self, index: usize, x: f32, y: f32, w: f32, h: f32) {
        if !self.images.contains(&index) {
            self.images.push(index);
        }
        let _ = writeln!(
            self.ops,
            "q {} 0 0 {} {} {} cm /Im{index} Do Q",
            num(w),
            num(h),
            num(x),
            num(y),
        );
    }

    /// Makes the rectangle open `uri` when clicked.
    pub fn link(&mut self, x: f32, y: f32, w: f32, h: f32, uri: &str) {
        self.links.push(Link {
            rect: [x, y, x + w, y + h],
            uri: uri.to_string(),
        });
    }
}

/// A decoded raster: 8-bit RGB, plus 8-bit alpha when any pixel is not
/// opaque.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    pub alpha: Option<Vec<u8>>,
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let pa = (p - i16::from(a)).abs();
    let pb = (p - i16::from(b)).abs();
    let pc = (p - i16::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Decodes an 8-bit, non-interlaced PNG (grey, grey+alpha, RGB, RGBA or
/// palette). Anything else is refused with a reason, and the caller falls
/// back to a text wordmark.
pub fn decode_png(bytes: &[u8]) -> Result<Image, String> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIGNATURE) {
        return Err("not a PNG file".into());
    }
    let mut at = SIGNATURE.len();
    let mut header = None;
    let mut palette: Vec<u8> = Vec::new();
    let mut data = Vec::new();
    while at + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let kind = &bytes[at + 4..at + 8];
        let body = bytes
            .get(at + 8..at + 8 + len)
            .ok_or("truncated PNG chunk")?;
        match kind {
            b"IHDR" if len >= 13 => header = Some(body[..13].to_vec()),
            b"PLTE" => palette = body.to_vec(),
            b"IDAT" => data.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + len;
    }
    let header = header.ok_or("PNG has no IHDR")?;
    let width = u32::from_be_bytes(header[0..4].try_into().unwrap());
    let height = u32::from_be_bytes(header[4..8].try_into().unwrap());
    let (depth, color_type, interlace) = (header[8], header[9], header[12]);
    if depth != 8 {
        return Err(format!("PNG bit depth {depth} is not supported (8 only)"));
    }
    if interlace != 0 {
        return Err("interlaced PNGs are not supported".into());
    }
    let channels: usize = match color_type {
        0 | 3 => 1,
        4 => 2,
        2 => 3,
        6 => 4,
        other => return Err(format!("PNG colour type {other} is not supported")),
    };
    if width == 0 || height == 0 || width > 8000 || height > 8000 {
        return Err(format!("PNG size {width}x{height} is out of range"));
    }
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&data)
        .map_err(|e| format!("PNG data does not inflate: {e:?}"))?;
    let (w, h) = (width as usize, height as usize);
    let stride = w * channels;
    if raw.len() < h * (stride + 1) {
        return Err("PNG data is shorter than its size".into());
    }
    let mut pixels = vec![0u8; h * stride];
    for y in 0..h {
        let filter = raw[y * (stride + 1)];
        let src = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        for x in 0..stride {
            let a = if x >= channels {
                pixels[y * stride + x - channels]
            } else {
                0
            };
            let b = if y > 0 {
                pixels[(y - 1) * stride + x]
            } else {
                0
            };
            let c = if x >= channels && y > 0 {
                pixels[(y - 1) * stride + x - channels]
            } else {
                0
            };
            let value = match filter {
                0 => src[x],
                1 => src[x].wrapping_add(a),
                2 => src[x].wrapping_add(b),
                3 => src[x].wrapping_add(((u16::from(a) + u16::from(b)) / 2) as u8),
                4 => src[x].wrapping_add(paeth(a, b, c)),
                other => return Err(format!("unknown PNG row filter {other}")),
            };
            pixels[y * stride + x] = value;
        }
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut alpha = Vec::with_capacity(w * h);
    for px in pixels.chunks_exact(channels) {
        match color_type {
            0 => {
                rgb.extend_from_slice(&[px[0]; 3]);
                alpha.push(255);
            }
            4 => {
                rgb.extend_from_slice(&[px[0]; 3]);
                alpha.push(px[1]);
            }
            2 => {
                rgb.extend_from_slice(px);
                alpha.push(255);
            }
            6 => {
                rgb.extend_from_slice(&px[..3]);
                alpha.push(px[3]);
            }
            _ => {
                let i = usize::from(px[0]) * 3;
                let entry = palette
                    .get(i..i + 3)
                    .ok_or("PNG palette index out of range")?;
                rgb.extend_from_slice(entry);
                alpha.push(255);
            }
        }
    }
    let alpha = if alpha.iter().all(|&a| a == 255) {
        None
    } else {
        Some(alpha)
    };
    Ok(Image {
        width,
        height,
        rgb,
        alpha,
    })
}

impl Image {
    /// Box-filter downsample so the image is at most `max_width` pixels
    /// wide; a narrower image is returned unchanged. Keeps a printed logo
    /// sharp while the PDF stays small enough to attach to an email.
    pub fn downsampled(self, max_width: u32) -> Image {
        if self.width <= max_width {
            return self;
        }
        let (sw, sh) = (self.width as usize, self.height as usize);
        let dw = max_width as usize;
        let dh = ((sh * dw) / sw).max(1);
        let average = |src: &[u8], channels: usize| -> Vec<u8> {
            let mut out = Vec::with_capacity(dw * dh * channels);
            for dy in 0..dh {
                let (y0, y1) = (dy * sh / dh, ((dy + 1) * sh / dh).max(dy * sh / dh + 1));
                for dx in 0..dw {
                    let (x0, x1) = (dx * sw / dw, ((dx + 1) * sw / dw).max(dx * sw / dw + 1));
                    for ch in 0..channels {
                        let mut sum = 0u32;
                        for y in y0..y1 {
                            for x in x0..x1 {
                                sum += u32::from(src[(y * sw + x) * channels + ch]);
                            }
                        }
                        let count = ((y1 - y0) * (x1 - x0)) as u32;
                        out.push((sum / count) as u8);
                    }
                }
            }
            out
        };
        Image {
            width: dw as u32,
            height: dh as u32,
            rgb: average(&self.rgb, 3),
            alpha: self.alpha.as_ref().map(|a| average(a, 1)),
        }
    }
}

/// Pages plus the images they draw, serialised by [`PdfDocument::to_bytes`].
#[derive(Clone, Debug, Default)]
pub struct PdfDocument {
    pub title: String,
    pub pages: Vec<Page>,
    images: Vec<Image>,
}

impl PdfDocument {
    pub fn new(title: impl Into<String>) -> Self {
        PdfDocument {
            title: title.into(),
            pages: Vec::new(),
            images: Vec::new(),
        }
    }

    /// Registers an image; pages draw it by the returned index.
    pub fn add_image(&mut self, image: Image) -> usize {
        self.images.push(image);
        self.images.len() - 1
    }

    /// The finished file. Deterministic: the same document always gives the
    /// same bytes (no creation date, no random id).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets: Vec<usize> = Vec::new();
        let mut object = |out: &mut Vec<u8>, body: &[u8]| {
            offsets.push(out.len());
            let n = offsets.len();
            out.extend_from_slice(format!("{n} 0 obj\n").as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        };
        let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
            let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            body.extend_from_slice(data);
            body.extend_from_slice(b"\nendstream");
            body
        };

        // Fixed objects: 1 catalog, 2 page tree, 3 info, 4..=6 fonts.
        let font_base = 4;
        let mut next = font_base + FONTS.len();
        let mut image_ids = Vec::new();
        for image in &self.images {
            let id = next;
            next += if image.alpha.is_some() { 2 } else { 1 };
            image_ids.push(id);
        }
        // Per page: page, content, then one object per link.
        let mut page_ids = Vec::new();
        for page in &self.pages {
            page_ids.push(next);
            next += 2 + page.links.len();
        }

        let kids: Vec<String> = page_ids.iter().map(|id| format!("{id} 0 R")).collect();
        object(&mut out, b"<< /Type /Catalog /Pages 2 0 R >>");
        object(
            &mut out,
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                kids.join(" "),
                self.pages.len()
            )
            .as_bytes(),
        );
        object(
            &mut out,
            format!(
                "<< /Title {} /Producer (ledger render) >>",
                literal(&encode(&self.title))
            )
            .as_bytes(),
        );
        for font in FONTS {
            object(
                &mut out,
                format!(
                    "<< /Type /Font /Subtype /Type1 /BaseFont /{} /Encoding /WinAnsiEncoding >>",
                    font.base_font()
                )
                .as_bytes(),
            );
        }
        for (image, &id) in self.images.iter().zip(&image_ids) {
            let rgb = miniz_oxide::deflate::compress_to_vec_zlib(&image.rgb, 9);
            let smask = if image.alpha.is_some() {
                format!(" /SMask {} 0 R", id + 1)
            } else {
                String::new()
            };
            object(
                &mut out,
                &stream(
                    &format!(
                        "/Type /XObject /Subtype /Image /Width {} /Height {} \
                         /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode{smask}",
                        image.width, image.height
                    ),
                    &rgb,
                ),
            );
            if let Some(alpha) = &image.alpha {
                let a = miniz_oxide::deflate::compress_to_vec_zlib(alpha, 9);
                object(
                    &mut out,
                    &stream(
                        &format!(
                            "/Type /XObject /Subtype /Image /Width {} /Height {} \
                             /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                            image.width, image.height
                        ),
                        &a,
                    ),
                );
            }
        }
        let fonts: Vec<String> = FONTS
            .iter()
            .enumerate()
            .map(|(i, f)| format!("/{} {} 0 R", f.resource(), font_base + i))
            .collect();
        for (page, &id) in self.pages.iter().zip(&page_ids) {
            let xobjects: Vec<String> = page
                .images
                .iter()
                .map(|&i| format!("/Im{i} {} 0 R", image_ids[i]))
                .collect();
            let annots: Vec<String> = (0..page.links.len())
                .map(|i| format!("{} 0 R", id + 2 + i))
                .collect();
            let mut dict = format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] \
                 /Resources << /Font << {} >>",
                num(PAGE_WIDTH),
                num(PAGE_HEIGHT),
                fonts.join(" ")
            );
            if !xobjects.is_empty() {
                let _ = write!(dict, " /XObject << {} >>", xobjects.join(" "));
            }
            let _ = write!(dict, " >> /Contents {} 0 R", id + 1);
            if !annots.is_empty() {
                let _ = write!(dict, " /Annots [{}]", annots.join(" "));
            }
            dict.push_str(" >>");
            object(&mut out, dict.as_bytes());
            object(&mut out, &stream("", page.ops.as_bytes()));
            for link in &page.links {
                let [x0, y0, x1, y1] = link.rect;
                object(
                    &mut out,
                    format!(
                        "<< /Type /Annot /Subtype /Link /Rect [{} {} {} {}] /Border [0 0 0] \
                         /A << /S /URI /URI {} >> >>",
                        num(x0),
                        num(y0),
                        num(x1),
                        num(y1),
                        literal(link.uri.as_bytes())
                    )
                    .as_bytes(),
                );
            }
        }

        let xref_at = out.len();
        let mut xref = format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1);
        for offset in &offsets {
            let _ = writeln!(xref, "{offset:010} 00000 n ");
        }
        let _ = write!(
            xref,
            "trailer\n<< /Size {} /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            offsets.len() + 1
        );
        out.extend_from_slice(xref.as_bytes());
        out
    }
}

// ---------------------------------------------------------------------------
// Reading a rendered file back
// ---------------------------------------------------------------------------

/// The page count a file written by [`PdfDocument::to_bytes`] declares.
pub fn page_count(bytes: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(bytes);
    let at = text.find("/Type /Pages")?;
    let rest = &text[at..];
    let count = rest.find("/Count ")? + "/Count ".len();
    rest[count..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Every string shown with `Tj` in an uncompressed content stream, one page
/// per element, each string on its own line in drawing order. Enough to
/// read back what [`Page`] wrote; not a general PDF text extractor.
pub fn extract_text(bytes: &[u8]) -> Vec<String> {
    let mut pages = Vec::new();
    let mut at = 0;
    // Every stream this writer emits opens with `>>\nstream\n`; matching the
    // dictionary's close keeps `endstream\n` from reading as a new stream.
    while let Some(found) = find(&bytes[at..], b">>\nstream\n") {
        let start = at + found + b">>\nstream\n".len();
        let Some(len) = find(&bytes[start..], b"\nendstream") else {
            break;
        };
        let data = &bytes[start..start + len];
        at = start + len + b"\nendstream".len();
        // Content streams carry text operators; image streams are binary
        // and never contain `BT`.
        let dict_start = bytes[..start].iter().rposition(|&b| b == b'<').unwrap_or(0);
        if find(&bytes[dict_start..start], b"/Subtype /Image").is_some() {
            continue;
        }
        if find(data, b" Tj").is_none() && find(data, b"BT").is_none() {
            pages.push(String::new());
            continue;
        }
        pages.push(strings_shown(data).join("\n"));
    }
    pages
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn strings_shown(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data[i] != b'(' {
            i += 1;
            continue;
        }
        let mut depth = 1;
        let mut text = String::new();
        i += 1;
        while i < data.len() && depth > 0 {
            match data[i] {
                b'\\' if i + 1 < data.len() => {
                    let next = data[i + 1];
                    if next.is_ascii_digit() {
                        let digits: String = data[i + 1..]
                            .iter()
                            .take(3)
                            .take_while(|b| b.is_ascii_digit())
                            .map(|&b| b as char)
                            .collect();
                        let byte = u8::from_str_radix(&digits, 8).unwrap_or(b'?');
                        text.push(from_win_ansi(byte));
                        i += 1 + digits.len();
                        continue;
                    }
                    text.push(match next {
                        b'n' => '\n',
                        b't' => '\t',
                        other => other as char,
                    });
                    i += 2;
                    continue;
                }
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            if depth > 0 {
                text.push(from_win_ansi(data[i]));
            }
            i += 1;
        }
        if data[i..].starts_with(b" Tj") {
            out.push(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_match_the_afm() {
        assert_eq!(text_width(Font::Regular, 1000.0, "0"), 556.0);
        assert_eq!(text_width(Font::Bold, 1000.0, "W"), 944.0);
        assert_eq!(text_width(Font::Regular, 10.0, "il"), 4.44);
    }

    #[test]
    fn wrap_breaks_on_words_and_long_tokens() {
        let lines = wrap(Font::Regular, 10.0, "one two three four", 40.0);
        assert!(lines.len() > 1, "{lines:?}");
        assert!(lines
            .iter()
            .all(|l| text_width(Font::Regular, 10.0, l) <= 40.0));
        let long = wrap(Font::Regular, 10.0, "aaaaaaaaaaaaaaaaaaaa", 30.0);
        assert!(long.len() > 1);
        assert_eq!(long.concat(), "aaaaaaaaaaaaaaaaaaaa");
        assert_eq!(wrap(Font::Regular, 10.0, "", 30.0), vec![String::new()]);
    }

    #[test]
    fn text_round_trips_through_the_file() {
        let mut doc = PdfDocument::new("t");
        let mut page = Page::new();
        page.text(
            10.0,
            10.0,
            Font::Bold,
            12.0,
            Color::BLACK,
            "Invoice (1001) \\ café — €5",
        );
        page.text(10.0, 30.0, Font::Regular, 12.0, Color::BLACK, "日本");
        doc.pages.push(page);
        doc.pages.push(Page::new());
        let bytes = doc.to_bytes();
        assert_eq!(page_count(&bytes), Some(2));
        let text = extract_text(&bytes);
        assert_eq!(text.len(), 2);
        assert_eq!(text[0], "Invoice (1001) \\ café — €5\n??");
        assert_eq!(text[1], "");
        assert!(bytes.ends_with(b"%%EOF\n"));
    }

    #[test]
    fn xref_offsets_point_at_objects() {
        let mut doc = PdfDocument::new("t");
        let mut page = Page::new();
        page.link(0.0, 0.0, 10.0, 10.0, "https://example.com/pay");
        doc.pages.push(page);
        let bytes = doc.to_bytes();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let xref = text.rfind("xref\n").unwrap();
        for (n, line) in text[xref..]
            .lines()
            .skip(3)
            .take_while(|l| l.ends_with(" n "))
            .enumerate()
        {
            let offset: usize = line[..10].parse().unwrap();
            assert!(
                text[offset..].starts_with(&format!("{} 0 obj", n + 1)),
                "object {}",
                n + 1
            );
        }
        assert!(text.contains("/URI (https://example.com/pay)"));
    }

    fn png(width: u32, height: u32, color_type: u8, rows: &[Vec<u8>]) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8], body: &[u8]) {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out.extend_from_slice(&[0, 0, 0, 0]); // CRC, not checked here
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, color_type, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        let raw: Vec<u8> = rows.concat();
        chunk(
            &mut out,
            b"IDAT",
            &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6),
        );
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn png_filters_decode() {
        // 2x2 RGBA: row 0 with Sub filter, row 1 with Up filter.
        let rows = vec![
            vec![1, 10, 20, 30, 255, 5, 5, 5, 0],
            vec![2, 1, 1, 1, 0, 1, 1, 1, 1],
        ];
        let image = decode_png(&png(2, 2, 6, &rows)).unwrap();
        assert_eq!(
            image.rgb,
            vec![10, 20, 30, 15, 25, 35, 11, 21, 31, 16, 26, 36]
        );
        assert_eq!(image.alpha, Some(vec![255, 255, 255, 0]));
        let opaque = decode_png(&png(1, 1, 2, &[vec![0, 1, 2, 3]])).unwrap();
        assert_eq!(opaque.alpha, None);
        assert!(decode_png(b"GIF89a").is_err());
        let small = decode_png(&png(
            4,
            2,
            0,
            &[vec![0, 0, 100, 200, 255], vec![0, 0, 100, 200, 255]],
        ))
        .unwrap()
        .downsampled(2);
        assert_eq!((small.width, small.height), (2, 1));
        assert_eq!(small.rgb, vec![50, 50, 50, 227, 227, 227]);
    }
}
