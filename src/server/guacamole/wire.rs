//! The Guacamole protocol's text framing, shared by both roles: `LEN.VALUE,…;` instructions
//! whose lengths count Unicode code points, read with guacd's own bounds; X11 keysyms for
//! the keys a handler names; and text rendered to PNG, so a handler draws words rather than
//! writing image bytes.
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt};

/// guacd's `GUAC_INSTRUCTION_MAX_LENGTH`: one instruction, in bytes.
pub const MAX_INSTRUCTION: usize = 8192;
/// guacd's `GUAC_INSTRUCTION_MAX_ELEMENTS`.
pub const MAX_ELEMENTS: usize = 128;
/// The protocol version both roles speak.
pub const VERSION: &str = "VERSION_1_3_0";
/// Base64 carried in one `blob`: what guacd itself sends at most.
pub const BLOB_CHUNK: usize = 6048;
/// A clipboard stream is at most this long, decoded.
pub const MAX_CLIPBOARD: usize = 64 * 1024;
pub const PORT: u16 = 4822;

#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub opcode: String,
    pub args: Vec<String>,
}

impl Instruction {
    pub fn new(opcode: &str, args: &[&str]) -> Self {
        Self {
            opcode: opcode.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }
    pub fn arg(&self, i: usize) -> &str {
        self.args.get(i).map(String::as_str).unwrap_or("")
    }
}

fn element(out: &mut String, v: &str) {
    out.push_str(&v.chars().count().to_string());
    out.push('.');
    out.push_str(v);
}

pub fn encode(opcode: &str, args: &[&str]) -> String {
    let mut out = String::new();
    element(&mut out, opcode);
    for a in args {
        out.push(',');
        element(&mut out, a);
    }
    out.push(';');
    out
}

pub fn encode_ins(i: &Instruction) -> String {
    let args: Vec<&str> = i.args.iter().map(String::as_str).collect();
    encode(&i.opcode, &args)
}

/// Reads instructions from a byte stream, refusing one past guacd's bounds.
pub struct Reader<R> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    /// The next instruction; None at a clean end of stream.
    pub async fn next(&mut self) -> Result<Option<Instruction>> {
        loop {
            if let Some((ins, used)) = parse(&self.buf)? {
                self.buf.drain(..used);
                return Ok(Some(ins));
            }
            ensure!(
                self.buf.len() <= MAX_INSTRUCTION,
                "instruction longer than {MAX_INSTRUCTION} bytes"
            );
            let mut chunk = [0u8; 4096];
            let n = self.inner.read(&mut chunk).await?;
            if n == 0 {
                ensure!(self.buf.is_empty(), "connection closed mid-instruction");
                return Ok(None);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// One complete instruction at the start of `b` and the bytes it used, or None when more
/// bytes are needed.
pub fn parse(b: &[u8]) -> Result<Option<(Instruction, usize)>> {
    let mut pos = 0;
    let mut elements: Vec<String> = Vec::new();
    loop {
        // LENGTH '.'
        let dot = match b[pos..].iter().take(12).position(|&c| c == b'.') {
            Some(d) => d,
            None if b.len() - pos < 12 => return Ok(None),
            None => bail!("element length is not a number followed by '.'"),
        };
        let len_text = std::str::from_utf8(&b[pos..pos + dot])?;
        ensure!(
            !len_text.is_empty() && len_text.bytes().all(|c| c.is_ascii_digit()),
            "element length {len_text:?} is not a number"
        );
        let chars: usize = len_text.parse()?;
        ensure!(chars <= MAX_INSTRUCTION, "element of {chars} characters");
        pos += dot + 1;
        // `chars` code points of UTF-8.
        let start = pos;
        let mut seen = 0;
        while seen < chars {
            let Some(&lead) = b.get(pos) else {
                return Ok(None);
            };
            let width = match lead {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => bail!("invalid UTF-8 in an element"),
            };
            pos += width;
            seen += 1;
        }
        if pos >= b.len() {
            return Ok(None);
        }
        let value = std::str::from_utf8(&b[start..pos]).context("element is not UTF-8")?;
        elements.push(value.to_string());
        ensure!(
            elements.len() <= MAX_ELEMENTS,
            "more than {MAX_ELEMENTS} elements"
        );
        match b[pos] {
            b',' => pos += 1,
            b';' => {
                pos += 1;
                let mut it = elements.into_iter();
                let opcode = it.next().unwrap_or_default();
                return Ok(Some((
                    Instruction {
                        opcode,
                        args: it.collect(),
                    },
                    pos,
                )));
            }
            c => bail!("expected ',' or ';' after an element, got {:?}", c as char),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Keysyms

const NAMED: &[(&str, u32)] = &[
    ("Return", 0xff0d),
    ("Enter", 0xff0d),
    ("BackSpace", 0xff08),
    ("Tab", 0xff09),
    ("Escape", 0xff1b),
    ("Delete", 0xffff),
    ("Home", 0xff50),
    ("Left", 0xff51),
    ("Up", 0xff52),
    ("Right", 0xff53),
    ("Down", 0xff54),
    ("Page_Up", 0xff55),
    ("Page_Down", 0xff56),
    ("End", 0xff57),
    ("Insert", 0xff63),
    ("Shift_L", 0xffe1),
    ("Control_L", 0xffe3),
    ("Alt_L", 0xffe9),
    ("Super_L", 0xffeb),
    ("space", 0x20),
];

/// The keysym a character types: Latin-1 directly, everything else in the Unicode range.
pub fn keysym_of_char(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        c if (c as u32) < 0x100 => c as u32,
        c => 0x0100_0000 + c as u32,
    }
}

/// A key a handler names: `Return`, `Escape`, `F1`…`F12`, a single character, or a keysym.
pub fn keysym_of_name(name: &str) -> Result<u32> {
    if let Some((_, k)) = NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Ok(*k);
    }
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        ensure!((1..=35).contains(&n), "function keys are F1-F35");
        return Ok(0xffbe + n - 1);
    }
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.clone().next()) {
        return Ok(keysym_of_char(c));
    }
    if let Some(hex) = name.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).context("keysym is not hex");
    }
    bail!("unknown key {name:?}: use Return, Escape, Tab, BackSpace, an arrow, F1-F35, a character or 0x<keysym>")
}

/// What a pressed keysym is, as the handler is told it: Some(char) for one that types text.
pub fn char_of_keysym(k: u32) -> Option<char> {
    match k {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(k),
        0x0100_0100..=0x0110_ffff => char::from_u32(k - 0x0100_0000),
        _ => None,
    }
}

pub fn name_of_keysym(k: u32) -> String {
    if let Some((n, _)) = NAMED.iter().find(|(_, v)| *v == k) {
        return n.to_string();
    }
    if (0xffbe..0xffbe + 35).contains(&k) {
        return format!("F{}", k - 0xffbe + 1);
    }
    match char_of_keysym(k) {
        Some(c) => c.to_string(),
        None => format!("0x{k:x}"),
    }
}

// ---------------------------------------------------------------------------------------
// Colours and text

/// `#rrggbb` (or `rrggbb`) to channels.
pub fn color(s: &str) -> Result<[u8; 3]> {
    let h = s.trim_start_matches('#');
    ensure!(h.len() == 6, "colour must be #rrggbb");
    let v = u32::from_str_radix(h, 16).context("colour must be #rrggbb")?;
    Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// The largest text image NetGet renders, in pixels per side.
pub const MAX_TEXT_SIDE: u32 = 4096;

/// `text` in an 8×8 bitmap font, `scale` times enlarged, as a PNG; its width and height.
pub fn render_text(
    text: &str,
    fg: [u8; 3],
    bg: Option<[u8; 3]>,
    scale: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    use font8x8::UnicodeFonts;
    let lines: Vec<&str> = text.split('\n').collect();
    let cols = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(1) as u32;
    let w = cols * 8 * scale;
    let h = lines.len() as u32 * 8 * scale;
    ensure!(
        w <= MAX_TEXT_SIDE && h <= MAX_TEXT_SIDE,
        "text renders larger than {MAX_TEXT_SIDE} pixels; shorten it or lower scale"
    );
    let mut img = image::RgbaImage::from_pixel(
        w,
        h,
        match bg {
            Some([r, g, b]) => image::Rgba([r, g, b, 255]),
            None => image::Rgba([0, 0, 0, 0]),
        },
    );
    for (row, line) in lines.iter().enumerate() {
        for (col, c) in line.chars().enumerate() {
            let glyph = font8x8::BASIC_FONTS
                .get(c)
                .or_else(|| font8x8::LATIN_FONTS.get(c))
                .or_else(|| font8x8::BASIC_FONTS.get('?'))
                .unwrap_or([0; 8]);
            for (gy, bits) in glyph.iter().enumerate() {
                for gx in 0..8 {
                    if bits & (1 << gx) == 0 {
                        continue;
                    }
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let x = (col as u32 * 8 + gx) * scale + sx;
                            let y = (row as u32 * 8 + gy as u32) * scale + sy;
                            img.put_pixel(x, y, image::Rgba([fg[0], fg[1], fg[2], 255]));
                        }
                    }
                }
            }
        }
    }
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok((png, w, h))
}

/// Bytes as the `blob` instructions of one stream.
pub fn blobs(stream: &str, data: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = String::new();
    for chunk in b64.as_bytes().chunks(BLOB_CHUNK) {
        out.push_str(&encode(
            "blob",
            &[stream, std::str::from_utf8(chunk).unwrap_or("")],
        ));
    }
    out
}

pub fn unbase64(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("blob is not base64")
}

/// Guacamole status codes used here.
pub const STATUS_SERVER_ERROR: u32 = 0x0200;
pub const STATUS_CLIENT_UNAUTHORIZED: u32 = 0x0301;
pub const STATUS_CLIENT_FORBIDDEN: u32 = 0x0303;
