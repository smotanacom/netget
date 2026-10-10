//! The X11 core protocol as a client speaks it (X Window System Protocol, X11R6): connection
//! setup, the requests NetGet issues, and the replies, errors and events it reads. NetGet always
//! asks for little-endian byte order (`l`), so every multi-byte field here is little-endian in
//! both directions; the server converts.
use anyhow::{bail, ensure, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The most a single reply or generic event may carry. NetGet asks for at most
/// [`MAX_PROPERTY_BYTES`] of a property, and a QueryTree of 250 000 windows still fits; a reply
/// announcing more is refused before it is read, and the connection ends.
pub const MAX_REPLY_BYTES: usize = 1 << 20;
/// The most of a property NetGet reads (GetProperty's long-length) or writes in one request.
pub const MAX_PROPERTY_BYTES: usize = 64 * 1024;

pub const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

pub const CREATE_WINDOW: u8 = 1;
pub const DESTROY_WINDOW: u8 = 4;
pub const MAP_WINDOW: u8 = 8;
pub const UNMAP_WINDOW: u8 = 10;
pub const CONFIGURE_WINDOW: u8 = 12;
pub const GET_GEOMETRY: u8 = 14;
pub const QUERY_TREE: u8 = 15;
pub const INTERN_ATOM: u8 = 16;
pub const GET_ATOM_NAME: u8 = 17;
pub const CHANGE_PROPERTY: u8 = 18;
pub const DELETE_PROPERTY: u8 = 19;
pub const GET_PROPERTY: u8 = 20;
pub const LIST_PROPERTIES: u8 = 21;
pub const GET_INPUT_FOCUS: u8 = 43;
pub const LIST_EXTENSIONS: u8 = 99;
pub const BELL: u8 = 104;

/// Event-mask bits a window can be created watching (core protocol, section 8).
pub const EVENT_MASKS: &[(&str, u32)] = &[
    ("keyboard", 0x0000_0003),  // KeyPress | KeyRelease
    ("pointer", 0x0000_004C),   // ButtonPress | ButtonRelease | PointerMotion
    ("exposure", 0x0000_8000),  // Exposure
    ("structure", 0x0002_0000), // StructureNotify: Map/Unmap/Configure/Destroy
    ("focus", 0x0020_0000),     // FocusChange
    ("property", 0x0040_0000),  // PropertyChange
];

/// The 68 atoms every server predefines (core protocol, appendix B), by value.
pub const PREDEFINED_ATOMS: [&str; 68] = [
    "PRIMARY",
    "SECONDARY",
    "ARC",
    "ATOM",
    "BITMAP",
    "CARDINAL",
    "COLORMAP",
    "CURSOR",
    "CUT_BUFFER0",
    "CUT_BUFFER1",
    "CUT_BUFFER2",
    "CUT_BUFFER3",
    "CUT_BUFFER4",
    "CUT_BUFFER5",
    "CUT_BUFFER6",
    "CUT_BUFFER7",
    "DRAWABLE",
    "FONT",
    "INTEGER",
    "PIXMAP",
    "POINT",
    "RECTANGLE",
    "RESOURCE_MANAGER",
    "RGB_COLOR_MAP",
    "RGB_BEST_MAP",
    "RGB_BLUE_MAP",
    "RGB_DEFAULT_MAP",
    "RGB_GRAY_MAP",
    "RGB_GREEN_MAP",
    "RGB_RED_MAP",
    "STRING",
    "VISUALID",
    "WINDOW",
    "WM_COMMAND",
    "WM_HINTS",
    "WM_CLIENT_MACHINE",
    "WM_ICON_NAME",
    "WM_ICON_SIZE",
    "WM_NAME",
    "WM_NORMAL_HINTS",
    "WM_SIZE_HINTS",
    "WM_ZOOM_HINTS",
    "MIN_SPACE",
    "NORM_SPACE",
    "MAX_SPACE",
    "END_SPACE",
    "SUPERSCRIPT_X",
    "SUPERSCRIPT_Y",
    "SUBSCRIPT_X",
    "SUBSCRIPT_Y",
    "UNDERLINE_POSITION",
    "UNDERLINE_THICKNESS",
    "STRIKEOUT_ASCENT",
    "STRIKEOUT_DESCENT",
    "ITALIC_ANGLE",
    "X_HEIGHT",
    "QUAD_WIDTH",
    "WEIGHT",
    "POINT_SIZE",
    "RESOLUTION",
    "COPYRIGHT",
    "NOTICE",
    "FONT_NAME",
    "FAMILY_NAME",
    "FULL_NAME",
    "CAP_HEIGHT",
    "WM_CLASS",
    "WM_TRANSIENT_FOR",
];

pub fn predefined_atom(name: &str) -> Option<u32> {
    PREDEFINED_ATOMS
        .iter()
        .position(|a| *a == name)
        .map(|i| i as u32 + 1)
}

pub fn predefined_name(atom: u32) -> Option<&'static str> {
    PREDEFINED_ATOMS
        .get((atom as usize).checked_sub(1)?)
        .copied()
}

pub fn request_name(major: u8) -> &'static str {
    match major {
        CREATE_WINDOW => "CreateWindow",
        DESTROY_WINDOW => "DestroyWindow",
        MAP_WINDOW => "MapWindow",
        UNMAP_WINDOW => "UnmapWindow",
        CONFIGURE_WINDOW => "ConfigureWindow",
        GET_GEOMETRY => "GetGeometry",
        QUERY_TREE => "QueryTree",
        INTERN_ATOM => "InternAtom",
        GET_ATOM_NAME => "GetAtomName",
        CHANGE_PROPERTY => "ChangeProperty",
        DELETE_PROPERTY => "DeleteProperty",
        GET_PROPERTY => "GetProperty",
        LIST_PROPERTIES => "ListProperties",
        GET_INPUT_FOCUS => "GetInputFocus",
        LIST_EXTENSIONS => "ListExtensions",
        BELL => "Bell",
        _ => "unknown",
    }
}

pub fn error_name(code: u8) -> &'static str {
    match code {
        1 => "BadRequest",
        2 => "BadValue",
        3 => "BadWindow",
        4 => "BadPixmap",
        5 => "BadAtom",
        6 => "BadCursor",
        7 => "BadFont",
        8 => "BadMatch",
        9 => "BadDrawable",
        10 => "BadAccess",
        11 => "BadAlloc",
        12 => "BadColormap",
        13 => "BadGContext",
        14 => "BadIDChoice",
        15 => "BadName",
        16 => "BadLength",
        17 => "BadImplementation",
        _ => "extension error",
    }
}

pub fn event_name(code: u8) -> &'static str {
    match code {
        2 => "KeyPress",
        3 => "KeyRelease",
        4 => "ButtonPress",
        5 => "ButtonRelease",
        6 => "MotionNotify",
        7 => "EnterNotify",
        8 => "LeaveNotify",
        9 => "FocusIn",
        10 => "FocusOut",
        11 => "KeymapNotify",
        12 => "Expose",
        13 => "GraphicsExposure",
        14 => "NoExposure",
        15 => "VisibilityNotify",
        16 => "CreateNotify",
        17 => "DestroyNotify",
        18 => "UnmapNotify",
        19 => "MapNotify",
        20 => "MapRequest",
        21 => "ReparentNotify",
        22 => "ConfigureNotify",
        23 => "ConfigureRequest",
        24 => "GravityNotify",
        25 => "ResizeRequest",
        26 => "CirculateNotify",
        27 => "CirculateRequest",
        28 => "PropertyNotify",
        29 => "SelectionClear",
        30 => "SelectionRequest",
        31 => "SelectionNotify",
        32 => "ColormapNotify",
        33 => "ClientMessage",
        34 => "MappingNotify",
        35 => "GenericEvent",
        _ => "extension event",
    }
}

pub fn pad(n: usize) -> usize {
    (4 - n % 4) % 4
}

pub fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub fn i16_at(b: &[u8], at: usize) -> i16 {
    u16_at(b, at) as i16
}

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The connection setup request, with MIT-MAGIC-COOKIE-1 when a cookie is given.
pub fn setup_request(cookie: Option<&[u8]>) -> Vec<u8> {
    let (name, data): (&[u8], &[u8]) = match cookie {
        Some(c) => (MIT_MAGIC_COOKIE.as_bytes(), c),
        None => (b"", b""),
    };
    let mut out = vec![b'l', 0];
    out.extend(11u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((name.len() as u16).to_le_bytes());
    out.extend((data.len() as u16).to_le_bytes());
    out.extend([0, 0]);
    out.extend(name);
    out.extend(vec![0; pad(name.len())]);
    out.extend(data);
    out.extend(vec![0; pad(data.len())]);
    out
}

#[derive(Debug, Clone)]
pub struct Screen {
    pub root: u32,
    pub white_pixel: u32,
    pub black_pixel: u32,
    pub width: u16,
    pub height: u16,
    pub width_mm: u16,
    pub height_mm: u16,
    pub root_visual: u32,
    pub root_depth: u8,
}

#[derive(Debug, Clone)]
pub struct Setup {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub release: u32,
    pub vendor: String,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    /// Bytes, from the setup's maximum-request-length (in four-byte units).
    pub max_request_bytes: usize,
    pub screens: Vec<Screen>,
}

/// Read the server's answer to the setup request: `Ok(setup)`, or `Err` with the server's own
/// reason when it refused (status 0) or demands more authentication (status 2).
pub async fn read_setup<R: AsyncRead + Unpin>(r: &mut R) -> Result<Setup> {
    let mut head = [0u8; 8];
    r.read_exact(&mut head)
        .await
        .context("the X server closed the connection during setup")?;
    let len = u16_at(&head, 6) as usize * 4;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .context("the X server closed the connection during setup")?;
    match head[0] {
        1 => parse_setup(u16_at(&head, 2), u16_at(&head, 4), &body),
        0 => {
            let reason = &body[..(head[1] as usize).min(body.len())];
            bail!(
                "the X server refused the connection: {}",
                crate::utils::sanitize::line_field(String::from_utf8_lossy(reason).trim())
            )
        }
        2 => {
            let reason = String::from_utf8_lossy(&body);
            bail!(
                "the X server asks for further authentication: {}",
                crate::utils::sanitize::line_field(reason.trim_end_matches('\0').trim())
            )
        }
        other => bail!("the X server answered setup with status {other}"),
    }
}

fn parse_setup(major: u16, minor: u16, b: &[u8]) -> Result<Setup> {
    ensure!(
        b.len() >= 32,
        "setup reply of {} bytes is too short",
        b.len()
    );
    let vendor_len = u16_at(b, 16) as usize;
    let screens_n = b[20] as usize;
    let formats_n = b[21] as usize;
    let mut at = 32 + vendor_len + pad(vendor_len);
    ensure!(at <= b.len(), "setup vendor overruns the reply");
    let vendor =
        crate::utils::sanitize::line_field(&String::from_utf8_lossy(&b[32..32 + vendor_len]));
    at += 8 * formats_n;
    let mut screens = Vec::with_capacity(screens_n);
    for _ in 0..screens_n {
        ensure!(at + 40 <= b.len(), "setup screen overruns the reply");
        let s = &b[at..];
        screens.push(Screen {
            root: u32_at(s, 0),
            white_pixel: u32_at(s, 8),
            black_pixel: u32_at(s, 12),
            width: u16_at(s, 20),
            height: u16_at(s, 22),
            width_mm: u16_at(s, 24),
            height_mm: u16_at(s, 26),
            root_visual: u32_at(s, 32),
            root_depth: s[38],
        });
        let depths = s[39] as usize;
        at += 40;
        for _ in 0..depths {
            ensure!(at + 8 <= b.len(), "setup depth overruns the reply");
            let visuals = u16_at(b, at + 2) as usize;
            at += 8 + 24 * visuals;
        }
    }
    ensure!(at <= b.len(), "setup visuals overrun the reply");
    Ok(Setup {
        protocol_major: major,
        protocol_minor: minor,
        release: u32_at(b, 0),
        vendor,
        resource_id_base: u32_at(b, 4),
        resource_id_mask: u32_at(b, 8),
        max_request_bytes: u16_at(b, 18) as usize * 4,
        screens,
    })
}

#[derive(Debug, Clone)]
pub enum Packet {
    /// A whole reply, header included (`body[0] == 1`).
    Reply { seq: u16, body: Vec<u8> },
    Error {
        seq: u16,
        code: u8,
        bad_value: u32,
        major: u8,
        minor: u16,
    },
    /// An event, the high "sent by SendEvent" bit removed from `code`.
    Event {
        code: u8,
        synthetic: bool,
        body: Vec<u8>,
    },
}

/// Read one reply, error or event. A reply or generic event announcing more than
/// [`MAX_REPLY_BYTES`] is refused before its body is read.
pub async fn read_packet<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Packet>> {
    let mut head = [0u8; 32];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let seq = u16_at(&head, 2);
    let code = head[0] & 0x7F;
    let extended = head[0] == 1 || code == 35;
    let mut body = head.to_vec();
    if extended {
        let extra = (u32_at(&head, 4) as usize).saturating_mul(4);
        ensure!(
            32usize.saturating_add(extra) <= MAX_REPLY_BYTES,
            "the X server announced a {}-byte packet; the bound is {MAX_REPLY_BYTES}",
            32usize.saturating_add(extra)
        );
        body.resize(32 + extra, 0);
        r.read_exact(&mut body[32..]).await?;
    }
    Ok(Some(match head[0] {
        0 => Packet::Error {
            seq,
            code: head[1],
            bad_value: u32_at(&head, 4),
            minor: u16_at(&head, 8),
            major: head[10],
        },
        1 => Packet::Reply { seq, body },
        _ => Packet::Event {
            code,
            synthetic: head[0] & 0x80 != 0,
            body,
        },
    }))
}

/// One request: opcode, the header's data byte, and the body, padded and length-prefixed.
pub fn request(opcode: u8, data: u8, body: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let total = 4 + body.len() + pad(body.len());
    ensure!(
        total <= max_bytes && total / 4 <= u16::MAX as usize,
        "a {total}-byte {} request exceeds the server's {max_bytes}-byte maximum",
        request_name(opcode)
    );
    let mut out = Vec::with_capacity(total);
    out.push(opcode);
    out.push(data);
    out.extend(((total / 4) as u16).to_le_bytes());
    out.extend(body);
    out.resize(total, 0);
    Ok(out)
}

pub fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[allow(clippy::too_many_arguments)]
pub fn create_window(
    wid: u32,
    parent: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border: u16,
    background: u32,
    event_mask: u32,
) -> Vec<u8> {
    let mut b = u32s(&[wid, parent]);
    b.extend(x.to_le_bytes());
    b.extend(y.to_le_bytes());
    b.extend(width.to_le_bytes());
    b.extend(height.to_le_bytes());
    b.extend(border.to_le_bytes());
    b.extend(1u16.to_le_bytes()); // InputOutput
                                  // visual CopyFromParent; values: background-pixel (0x2), event-mask (0x800).
    b.extend(u32s(&[0, 0x0000_0802, background, event_mask]));
    b
}

pub fn configure_window(wid: u32, values: &[(u16, u32)]) -> Vec<u8> {
    let mask = values.iter().fold(0u16, |m, (bit, _)| m | bit);
    let mut b = u32s(&[wid]);
    b.extend(mask.to_le_bytes());
    b.extend([0, 0]);
    let mut sorted = values.to_vec();
    sorted.sort_by_key(|(bit, _)| *bit);
    b.extend(u32s(&sorted.iter().map(|(_, v)| *v).collect::<Vec<_>>()));
    b
}

pub fn intern_atom(name: &str) -> Vec<u8> {
    let mut b = (name.len() as u16).to_le_bytes().to_vec();
    b.extend([0, 0]);
    b.extend(name.as_bytes());
    b
}

pub fn change_property(wid: u32, property: u32, kind: u32, format: u8, data: &[u8]) -> Vec<u8> {
    let mut b = u32s(&[wid, property, kind]);
    b.push(format);
    b.extend([0, 0, 0]);
    let units = data.len() / (format as usize / 8);
    b.extend((units as u32).to_le_bytes());
    b.extend(data);
    b
}

pub fn get_property(wid: u32, property: u32) -> Vec<u8> {
    // AnyPropertyType, from offset 0, at most MAX_PROPERTY_BYTES (in 32-bit units).
    u32s(&[wid, property, 0, 0, (MAX_PROPERTY_BYTES / 4) as u32])
}

/// A reply's variable part: what follows the 32-byte header.
pub fn tail(body: &[u8]) -> &[u8] {
    &body[32.min(body.len())..]
}

/// The strings of a ListExtensions reply (count in the data byte, each length-prefixed).
pub fn strs(count: usize, b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    for _ in 0..count {
        let Some(&len) = b.get(at) else { break };
        let Some(s) = b.get(at + 1..at + 1 + len as usize) else {
            break;
        };
        out.push(String::from_utf8_lossy(s).into_owned());
        at += 1 + len as usize;
    }
    out
}
