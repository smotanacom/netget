//! The Gearman wire formats: the binary packet protocol and the text admin protocol.
//!
//! Everything here is a pure function of its arguments, shared by the session loop, the action
//! executor, the tests and the `gearman_packet` fuzz target. **The model never writes a
//! packet**: it supplies a result, a pair of progress numbers or an error text, and this module
//! decides the magic, the type, the length and the NUL separators.
//!
//! Binary packet (protocol.txt, gearmand 1.1):
//!
//! ```text
//! "\0REQ" | "\0RES"   type (u32 BE)   size (u32 BE)   args, NUL-separated
//! ```
//!
//! Every packet type has a fixed number of arguments; the last one runs to the end of the data
//! and may itself contain NULs (a workload or a result is binary). [`split_args`] splits on the
//! first `n - 1` NULs only.
//!
//! The admin protocol is text: a line (`status`, `workers`, `version`, …) answered by lines,
//! a multi-line answer ending in `.`. A connection may interleave the two; a leading `\0` byte
//! says which one comes next.

/// Packet magics.
pub const MAGIC_REQ: &[u8; 4] = b"\0REQ";
pub const MAGIC_RES: &[u8; 4] = b"\0RES";
pub const HEADER_LEN: usize = 12;

/// The largest packet body NetGet reads: 1 MiB, judged from the declared size before anything
/// is allocated. gearmand itself has no small limit, because a real server queues workloads for
/// workers; here the workload goes into a model prompt, and a megabyte is already far past
/// anything a prompt should carry.
pub const MAX_PACKET_BYTES: usize = 1024 * 1024;

/// The longest admin-protocol line, CRLF included.
pub const MAX_ADMIN_LINE: usize = 1024;

/// libgearman's own limits: `GEARMAN_FUNCTION_MAX_SIZE` and `GEARMAN_MAX_UNIQUE_SIZE`.
pub const MAX_FUNCTION_NAME: usize = 512;
pub const MAX_UNIQUE: usize = 64;

// Packet types this server reads or writes. The numbers are the protocol's.
pub const CAN_DO: u32 = 1;
pub const CANT_DO: u32 = 2;
pub const RESET_ABILITIES: u32 = 3;
pub const PRE_SLEEP: u32 = 4;
pub const SUBMIT_JOB: u32 = 7;
pub const JOB_CREATED: u32 = 8;
pub const GRAB_JOB: u32 = 9;
pub const WORK_STATUS: u32 = 12;
pub const WORK_COMPLETE: u32 = 13;
pub const WORK_FAIL: u32 = 14;
pub const GET_STATUS: u32 = 15;
pub const ECHO_REQ: u32 = 16;
pub const ECHO_RES: u32 = 17;
pub const SUBMIT_JOB_BG: u32 = 18;
pub const ERROR: u32 = 19;
pub const STATUS_RES: u32 = 20;
pub const SUBMIT_JOB_HIGH: u32 = 21;
pub const SET_CLIENT_ID: u32 = 22;
pub const CAN_DO_TIMEOUT: u32 = 23;
pub const ALL_YOURS: u32 = 24;
pub const WORK_EXCEPTION: u32 = 25;
pub const OPTION_REQ: u32 = 26;
pub const OPTION_RES: u32 = 27;
pub const WORK_DATA: u32 = 28;
pub const WORK_WARNING: u32 = 29;
pub const GRAB_JOB_UNIQ: u32 = 30;
pub const SUBMIT_JOB_HIGH_BG: u32 = 32;
pub const SUBMIT_JOB_LOW: u32 = 33;
pub const SUBMIT_JOB_LOW_BG: u32 = 34;
pub const GRAB_JOB_ALL: u32 = 39;

/// A parsed packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// `\0REQ` (true) or `\0RES`.
    pub request: bool,
    pub packet_type: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    BadMagic,
    /// The declared size is past [`MAX_PACKET_BYTES`].
    TooLarge(u32),
}

/// Parse a 12-byte header. The size is judged here, before the caller allocates for the body.
pub fn parse_header(bytes: &[u8]) -> Result<Header, HeaderError> {
    if bytes.len() < HEADER_LEN {
        return Err(HeaderError::BadMagic);
    }
    let request = match &bytes[..4] {
        m if m == MAGIC_REQ => true,
        m if m == MAGIC_RES => false,
        _ => return Err(HeaderError::BadMagic),
    };
    let packet_type = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    let size = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    if size as usize > MAX_PACKET_BYTES {
        return Err(HeaderError::TooLarge(size));
    }
    Ok(Header {
        request,
        packet_type,
        size,
    })
}

/// A complete packet.
pub fn encode(request: bool, packet_type: u32, args: &[&[u8]]) -> Vec<u8> {
    let size: usize = args.iter().map(|a| a.len()).sum::<usize>() + args.len().saturating_sub(1);
    let mut out = Vec::with_capacity(HEADER_LEN + size);
    out.extend_from_slice(if request { MAGIC_REQ } else { MAGIC_RES });
    out.extend_from_slice(&packet_type.to_be_bytes());
    out.extend_from_slice(&(size as u32).to_be_bytes());
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(0);
        }
        out.extend_from_slice(arg);
    }
    out
}

/// A response packet (`\0RES`).
pub fn response(packet_type: u32, args: &[&[u8]]) -> Vec<u8> {
    encode(false, packet_type, args)
}

/// Split `data` into exactly `n` arguments on its first `n - 1` NULs; the last argument keeps
/// any NULs of its own. `None` when there are too few separators.
pub fn split_args(data: &[u8], n: usize) -> Option<Vec<&[u8]>> {
    if n == 0 {
        return data.is_empty().then(Vec::new);
    }
    let mut out = Vec::with_capacity(n);
    let mut rest = data;
    for _ in 0..n - 1 {
        let at = rest.iter().position(|b| *b == 0)?;
        out.push(&rest[..at]);
        rest = &rest[at + 1..];
    }
    out.push(rest);
    Some(out)
}

/// How many arguments a request of this type carries, for the types this server reads.
pub fn request_arg_count(packet_type: u32) -> Option<usize> {
    Some(match packet_type {
        SUBMIT_JOB | SUBMIT_JOB_BG | SUBMIT_JOB_HIGH | SUBMIT_JOB_HIGH_BG | SUBMIT_JOB_LOW
        | SUBMIT_JOB_LOW_BG => 3,
        GET_STATUS | ECHO_REQ | OPTION_REQ | SET_CLIENT_ID => 1,
        _ => return None,
    })
}

/// Worker-side request types. NetGet runs jobs itself — the model is the worker — so these are
/// refused rather than half-served.
pub fn is_worker_packet(packet_type: u32) -> bool {
    matches!(
        packet_type,
        CAN_DO
            | CANT_DO
            | RESET_ABILITIES
            | PRE_SLEEP
            | GRAB_JOB
            | WORK_STATUS
            | WORK_COMPLETE
            | WORK_FAIL
            | CAN_DO_TIMEOUT
            | ALL_YOURS
            | WORK_EXCEPTION
            | WORK_DATA
            | WORK_WARNING
            | GRAB_JOB_UNIQ
            | GRAB_JOB_ALL
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Normal,
    High,
    Low,
}

impl Priority {
    pub fn name(self) -> &'static str {
        match self {
            Priority::Normal => "normal",
            Priority::High => "high",
            Priority::Low => "low",
        }
    }
}

/// One client request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Submit {
        function: String,
        unique: String,
        workload: Vec<u8>,
        priority: Priority,
        background: bool,
    },
    GetStatus(Vec<u8>),
    Echo(Vec<u8>),
    Option(Vec<u8>),
    SetClientId,
    /// A worker packet (`CAN_DO`, `GRAB_JOB`, …).
    Worker(u32),
    /// A request type this server does not implement (`SUBMIT_JOB_SCHED`, `SUBMIT_REDUCE_JOB`,
    /// an unknown number, a `\0RES` sent as a request).
    Unsupported(u32),
}

/// Why a request's arguments were refused. The codes go to the peer in an `ERROR` packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgError {
    /// Fewer NUL separators than the type's argument count.
    WrongArgCount,
    /// Function name empty or past [`MAX_FUNCTION_NAME`], or unique id past [`MAX_UNIQUE`].
    BadName,
}

/// Parse a request packet's body.
pub fn parse_request(header: &Header, data: &[u8]) -> Result<Request, ArgError> {
    let t = header.packet_type;
    if !header.request {
        return Ok(Request::Unsupported(t));
    }
    if is_worker_packet(t) {
        return Ok(Request::Worker(t));
    }
    let Some(n) = request_arg_count(t) else {
        return Ok(Request::Unsupported(t));
    };
    let args = split_args(data, n).ok_or(ArgError::WrongArgCount)?;
    Ok(match t {
        SUBMIT_JOB | SUBMIT_JOB_BG | SUBMIT_JOB_HIGH | SUBMIT_JOB_HIGH_BG | SUBMIT_JOB_LOW
        | SUBMIT_JOB_LOW_BG => {
            let (function, unique) = (args[0], args[1]);
            if function.is_empty()
                || function.len() > MAX_FUNCTION_NAME
                || unique.len() > MAX_UNIQUE
            {
                return Err(ArgError::BadName);
            }
            Request::Submit {
                function: String::from_utf8_lossy(function).into_owned(),
                unique: String::from_utf8_lossy(unique).into_owned(),
                workload: args[2].to_vec(),
                priority: match t {
                    SUBMIT_JOB_HIGH | SUBMIT_JOB_HIGH_BG => Priority::High,
                    SUBMIT_JOB_LOW | SUBMIT_JOB_LOW_BG => Priority::Low,
                    _ => Priority::Normal,
                },
                background: matches!(t, SUBMIT_JOB_BG | SUBMIT_JOB_HIGH_BG | SUBMIT_JOB_LOW_BG),
            }
        }
        GET_STATUS => Request::GetStatus(args[0].to_vec()),
        ECHO_REQ => Request::Echo(args[0].to_vec()),
        OPTION_REQ => Request::Option(args[0].to_vec()),
        _ => Request::SetClientId,
    })
}

/// A number the model gave for `WORK_STATUS`, as the decimal digits the protocol carries.
fn digits(n: u64) -> Vec<u8> {
    n.to_string().into_bytes()
}

pub fn job_created(handle: &[u8]) -> Vec<u8> {
    response(JOB_CREATED, &[handle])
}
pub fn work_status(handle: &[u8], numerator: u64, denominator: u64) -> Vec<u8> {
    response(
        WORK_STATUS,
        &[handle, &digits(numerator), &digits(denominator)],
    )
}
pub fn work_data(handle: &[u8], data: &[u8]) -> Vec<u8> {
    response(WORK_DATA, &[handle, data])
}
pub fn work_complete(handle: &[u8], result: &[u8]) -> Vec<u8> {
    response(WORK_COMPLETE, &[handle, result])
}
pub fn work_fail(handle: &[u8]) -> Vec<u8> {
    response(WORK_FAIL, &[handle])
}
pub fn work_exception(handle: &[u8], text: &[u8]) -> Vec<u8> {
    response(WORK_EXCEPTION, &[handle, text])
}
pub fn status_res(handle: &[u8], known: bool, running: bool, num: u64, den: u64) -> Vec<u8> {
    response(
        STATUS_RES,
        &[
            handle,
            if known { b"1" } else { b"0" },
            if running { b"1" } else { b"0" },
            &digits(num),
            &digits(den),
        ],
    )
}

/// An `ERROR` packet. The code is an identifier (letters, digits, `_`) and the text one line
/// without NULs, so neither can add an argument or a packet.
pub fn error(code: &str, text: &str) -> Result<Vec<u8>, String> {
    if code.is_empty()
        || code.len() > 64
        || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(format!(
            "error code must be 1-64 letters, digits or '_', got {code:?}"
        ));
    }
    if text.len() > 1024 || text.contains('\0') {
        return Err("error text must be at most 1024 bytes and contain no NUL".to_string());
    }
    Ok(response(ERROR, &[code.as_bytes(), text.as_bytes()]))
}

/// Read a response packet back: `(type, args)` split by the type's argument count, or `None`
/// for anything this module would not have written. The session loop uses it to check a
/// rendered reply against the job it answers.
pub fn read_response(packet: &[u8]) -> Option<(u32, Vec<Vec<u8>>)> {
    let header = parse_header(packet).ok()?;
    if header.request || packet.len() != HEADER_LEN + header.size as usize {
        return None;
    }
    let n = match header.packet_type {
        JOB_CREATED | WORK_FAIL | ECHO_RES | OPTION_RES => 1,
        WORK_COMPLETE | WORK_DATA | WORK_WARNING | WORK_EXCEPTION | ERROR => 2,
        WORK_STATUS => 3,
        STATUS_RES => 5,
        _ => return None,
    };
    let args = split_args(&packet[HEADER_LEN..], n)?;
    Some((
        header.packet_type,
        args.into_iter().map(<[u8]>::to_vec).collect(),
    ))
}

/// One admin-protocol command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminCommand {
    Status,
    Workers,
    Version,
    /// `maxqueue` changes server state; refused.
    MaxQueue,
    /// `shutdown`; refused.
    Shutdown,
    Unknown,
}

pub fn parse_admin(line: &str) -> AdminCommand {
    match line.split_whitespace().next().unwrap_or("") {
        "status" => AdminCommand::Status,
        "workers" => AdminCommand::Workers,
        "version" => AdminCommand::Version,
        "maxqueue" => AdminCommand::MaxQueue,
        "shutdown" => AdminCommand::Shutdown,
        _ => AdminCommand::Unknown,
    }
}

/// gearmand's admin error line: `ERR <CODE> <text with spaces as +>`.
pub fn admin_error(code: &str, text: &str) -> String {
    format!("ERR {code} {}\n", text.replace(' ', "+"))
}

/// The `status` answer: `FUNCTION\tTOTAL\tRUNNING\tAVAILABLE_WORKERS` per line, then `.`.
pub fn admin_status(rows: &[(String, u64, u64, u64)]) -> String {
    let mut out = String::new();
    for (function, total, running, workers) in rows {
        let name = crate::utils::sanitize::line_field(function);
        out.push_str(&format!("{name}\t{total}\t{running}\t{workers}\n"));
    }
    out.push_str(".\n");
    out
}
