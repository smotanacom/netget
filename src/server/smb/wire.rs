//! SMB2 wire format: the Direct TCP transport frame, the 64-byte header, and every response
//! body this server writes.
//!
//! Everything here is a pure function of its arguments, so the layout can be checked without a
//! socket (`tests/server/smb/header_layout_test.rs` walks every builder). The session loop in
//! `mod.rs` owns the state and the model; this module owns the bytes.
//!
//! **Every response header is built by [`ResponseHeader::encode`] and nothing else.** A client
//! matches a reply to its outstanding request by MessageId (MS-SMB2 3.2.5.1.2), so one builder
//! that lays the header out differently from the others breaks every exchange that goes through
//! it while the rest of the session looks healthy. The offsets are written once, below.

/// Length of the SMB2 sync header (MS-SMB2 2.2.1.2).
pub const HEADER_LEN: usize = 64;

/// `ProtocolId` of an SMB2 message.
pub const PROTOCOL_ID: [u8; 4] = *b"\xFESMB";

// Header field offsets (MS-SMB2 2.2.1.2, SYNC form). Kept as named constants so the one place
// that writes them and the one place that reads them cannot drift apart.
pub const OFF_PROTOCOL_ID: usize = 0;
pub const OFF_STRUCTURE_SIZE: usize = 4;
pub const OFF_CREDIT_CHARGE: usize = 6;
pub const OFF_STATUS: usize = 8;
pub const OFF_COMMAND: usize = 12;
pub const OFF_CREDITS: usize = 14;
pub const OFF_FLAGS: usize = 16;
pub const OFF_NEXT_COMMAND: usize = 20;
pub const OFF_MESSAGE_ID: usize = 24;
pub const OFF_RESERVED: usize = 32;
pub const OFF_TREE_ID: usize = 36;
pub const OFF_SESSION_ID: usize = 40;
pub const OFF_SIGNATURE: usize = 48;

/// Header `Flags` bits (MS-SMB2 2.2.1.2).
pub const FLAGS_SERVER_TO_REDIR: u32 = 0x0000_0001;
pub const FLAGS_ASYNC_COMMAND: u32 = 0x0000_0002;
pub const FLAGS_RELATED_OPERATIONS: u32 = 0x0000_0004;
pub const FLAGS_SIGNED: u32 = 0x0000_0008;

/// SMB2 command codes (MS-SMB2 2.2.1.2).
pub mod command {
    pub const NEGOTIATE: u16 = 0x0000;
    pub const SESSION_SETUP: u16 = 0x0001;
    pub const LOGOFF: u16 = 0x0002;
    pub const TREE_CONNECT: u16 = 0x0003;
    pub const TREE_DISCONNECT: u16 = 0x0004;
    pub const CREATE: u16 = 0x0005;
    pub const CLOSE: u16 = 0x0006;
    pub const FLUSH: u16 = 0x0007;
    pub const READ: u16 = 0x0008;
    pub const WRITE: u16 = 0x0009;
    pub const LOCK: u16 = 0x000A;
    pub const IOCTL: u16 = 0x000B;
    pub const CANCEL: u16 = 0x000C;
    pub const ECHO: u16 = 0x000D;
    pub const QUERY_DIRECTORY: u16 = 0x000E;
    pub const CHANGE_NOTIFY: u16 = 0x000F;
    pub const QUERY_INFO: u16 = 0x0010;
    pub const SET_INFO: u16 = 0x0011;
    pub const OPLOCK_BREAK: u16 = 0x0012;
}

/// NTSTATUS values this server sends (MS-ERREF 2.3.1).
pub mod status {
    pub const SUCCESS: u32 = 0x0000_0000;
    pub const INVALID_INFO_CLASS: u32 = 0xC000_0003;
    pub const INVALID_PARAMETER: u32 = 0xC000_000D;
    pub const INVALID_DEVICE_REQUEST: u32 = 0xC000_0010;
    pub const END_OF_FILE: u32 = 0xC000_0011;
    pub const MORE_PROCESSING_REQUIRED: u32 = 0xC000_0016;
    pub const ACCESS_DENIED: u32 = 0xC000_0022;
    pub const DATA_ERROR: u32 = 0xC000_003E;
    pub const LOGON_FAILURE: u32 = 0xC000_006D;
    pub const INSUFFICIENT_RESOURCES: u32 = 0xC000_009A;
    pub const NOT_SUPPORTED: u32 = 0xC000_00BB;
    pub const NETWORK_NAME_DELETED: u32 = 0xC000_00C9;
    pub const INTERNAL_ERROR: u32 = 0xC000_00E5;
    pub const FILE_CLOSED: u32 = 0xC000_0128;
    pub const USER_SESSION_DELETED: u32 = 0xC000_0203;
    pub const NO_MORE_FILES: u32 = 0x8000_0006;
}

// ---------------------------------------------------------------------------------------------
// Direct TCP transport (MS-SMB2 2.1)
// ---------------------------------------------------------------------------------------------

/// Direct TCP transport header type byte for a session message. Every SMB2 message on port 445
/// is preceded by `00` and a 24-bit big-endian length; this is the RFC 1002 session-service
/// header with the length widened, and it is what every real client writes.
pub const NBSS_SESSION_MESSAGE: u8 = 0x00;
/// RFC 1002 session request, sent by a client that dialled port 139 before any SMB traffic.
pub const NBSS_SESSION_REQUEST: u8 = 0x81;
/// RFC 1002 positive session response.
pub const NBSS_POSITIVE_RESPONSE: u8 = 0x82;
/// RFC 1002 session keep-alive; carries no payload and gets no answer.
pub const NBSS_KEEPALIVE: u8 = 0x85;

/// Largest length a Direct TCP header can carry (24 bits).
pub const NBSS_MAX_LEN: usize = 0x00FF_FFFF;

/// Prefix one SMB2 message (or compound chain) with its Direct TCP transport header.
pub fn frame(message: &[u8]) -> Vec<u8> {
    assert!(
        message.len() <= NBSS_MAX_LEN,
        "an SMB2 message longer than 24 bits cannot be framed"
    );
    let len = message.len() as u32;
    let mut out = Vec::with_capacity(4 + message.len());
    out.push(NBSS_SESSION_MESSAGE);
    out.extend_from_slice(&len.to_be_bytes()[1..4]);
    out.extend_from_slice(message);
    out
}

/// Split a transport header into its type byte and 24-bit length.
pub fn parse_frame_header(header: [u8; 4]) -> (u8, usize) {
    let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
    (header[0], len)
}

// ---------------------------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------------------------

/// The fields of a request header this server reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestHeader {
    pub credit_charge: u16,
    pub command: u16,
    pub credit_request: u16,
    pub flags: u32,
    pub next_command: u32,
    pub message_id: u64,
    pub tree_id: u32,
    pub session_id: u64,
}

impl RequestHeader {
    /// Parse the 64-byte header at the start of `msg`. `None` if it is short or is not SMB2.
    pub fn parse(msg: &[u8]) -> Option<Self> {
        if msg.len() < HEADER_LEN || msg[OFF_PROTOCOL_ID..OFF_PROTOCOL_ID + 4] != PROTOCOL_ID {
            return None;
        }
        Some(Self {
            credit_charge: le16(msg, OFF_CREDIT_CHARGE),
            command: le16(msg, OFF_COMMAND),
            credit_request: le16(msg, OFF_CREDITS),
            flags: le32(msg, OFF_FLAGS),
            next_command: le32(msg, OFF_NEXT_COMMAND),
            message_id: le64(msg, OFF_MESSAGE_ID),
            tree_id: le32(msg, OFF_TREE_ID),
            session_id: le64(msg, OFF_SESSION_ID),
        })
    }

    pub fn is_related(&self) -> bool {
        self.flags & FLAGS_RELATED_OPERATIONS != 0
    }
}

/// The most credits one response grants. A client asks for what it wants in `CreditRequest`;
/// granting up to this many keeps a pipelining client supplied without letting one request
/// inflate its window without limit.
pub const MAX_CREDIT_GRANT: u16 = 64;

/// Every field of a response header. [`ResponseHeader::encode`] is the only code in this server
/// that lays a header out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseHeader {
    pub command: u16,
    pub status: u32,
    pub message_id: u64,
    pub credit_charge: u16,
    pub credits: u16,
    /// Flags beyond `SERVER_TO_REDIR`, which `encode` always sets.
    pub flags: u32,
    pub next_command: u32,
    pub tree_id: u32,
    pub session_id: u64,
}

impl ResponseHeader {
    /// A response to `req`: its command, MessageId, CreditCharge, TreeId and SessionId echoed,
    /// `RELATED_OPERATIONS` carried over for a compound, and credits granted.
    ///
    /// The TreeId and SessionId are the request's; a caller that has just allocated one
    /// (SESSION_SETUP, TREE_CONNECT) overwrites the field it allocated.
    pub fn for_request(req: &RequestHeader, status: u32) -> Self {
        Self {
            command: req.command,
            status,
            message_id: req.message_id,
            credit_charge: req.credit_charge,
            credits: req.credit_request.clamp(1, MAX_CREDIT_GRANT),
            flags: req.flags & FLAGS_RELATED_OPERATIONS,
            next_command: 0,
            tree_id: req.tree_id,
            session_id: req.session_id,
        }
    }

    pub fn with_status(mut self, status: u32) -> Self {
        self.status = status;
        self
    }

    pub fn with_session_id(mut self, session_id: u64) -> Self {
        self.session_id = session_id;
        self
    }

    pub fn with_tree_id(mut self, tree_id: u32) -> Self {
        self.tree_id = tree_id;
        self
    }

    /// The 64-byte header (MS-SMB2 2.2.1.2, SYNC form). Signature is zero: this server signs
    /// nothing, and says so in NEGOTIATE (signing enabled, not required).
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[OFF_PROTOCOL_ID..OFF_PROTOCOL_ID + 4].copy_from_slice(&PROTOCOL_ID);
        h[OFF_STRUCTURE_SIZE..OFF_STRUCTURE_SIZE + 2].copy_from_slice(&64u16.to_le_bytes());
        h[OFF_CREDIT_CHARGE..OFF_CREDIT_CHARGE + 2]
            .copy_from_slice(&self.credit_charge.to_le_bytes());
        h[OFF_STATUS..OFF_STATUS + 4].copy_from_slice(&self.status.to_le_bytes());
        h[OFF_COMMAND..OFF_COMMAND + 2].copy_from_slice(&self.command.to_le_bytes());
        h[OFF_CREDITS..OFF_CREDITS + 2].copy_from_slice(&self.credits.to_le_bytes());
        let flags = self.flags | FLAGS_SERVER_TO_REDIR;
        h[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        h[OFF_NEXT_COMMAND..OFF_NEXT_COMMAND + 4].copy_from_slice(&self.next_command.to_le_bytes());
        h[OFF_MESSAGE_ID..OFF_MESSAGE_ID + 8].copy_from_slice(&self.message_id.to_le_bytes());
        // OFF_RESERVED (ProcessId in the SYNC form) stays zero.
        h[OFF_TREE_ID..OFF_TREE_ID + 4].copy_from_slice(&self.tree_id.to_le_bytes());
        h[OFF_SESSION_ID..OFF_SESSION_ID + 8].copy_from_slice(&self.session_id.to_le_bytes());
        // OFF_SIGNATURE..64 stays zero.
        h
    }

    fn message(&self, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(&self.encode());
        out.extend_from_slice(body);
        out
    }
}

/// Join the responses to a compound request into one chain (MS-SMB2 3.3.4.1.3): every response
/// but the last is padded to an 8-byte boundary and its `NextCommand` names that padded length.
pub fn chain(responses: Vec<Vec<u8>>) -> Vec<u8> {
    let count = responses.len();
    let mut out = Vec::new();
    for (i, mut response) in responses.into_iter().enumerate() {
        if i + 1 < count {
            while response.len() % 8 != 0 {
                response.push(0);
            }
            let next = response.len() as u32;
            response[OFF_NEXT_COMMAND..OFF_NEXT_COMMAND + 4].copy_from_slice(&next.to_le_bytes());
        }
        out.extend_from_slice(&response);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Response bodies. Each builder returns a whole message: header then body.
// ---------------------------------------------------------------------------------------------

/// SMB2 ERROR response (MS-SMB2 2.2.2): the status is in the header, the body is empty.
pub fn error_response(hdr: &ResponseHeader) -> Vec<u8> {
    let mut body = Vec::with_capacity(9);
    body.extend_from_slice(&9u16.to_le_bytes()); // StructureSize
    body.push(0); // ErrorContextCount
    body.push(0); // Reserved
    body.extend_from_slice(&0u32.to_le_bytes()); // ByteCount
    body.push(0); // ErrorData: one byte when ByteCount is zero
    hdr.message(&body)
}

/// The four-byte body shared by LOGOFF, TREE_DISCONNECT, FLUSH and ECHO responses.
pub fn empty_response(hdr: &ResponseHeader) -> Vec<u8> {
    hdr.message(&[4, 0, 0, 0])
}

/// What NEGOTIATE advertises. Grouped so the builder is a function of values, not of globals.
#[derive(Debug, Clone)]
pub struct NegotiateParams {
    pub dialect: u16,
    pub server_guid: [u8; 16],
    pub capabilities: u32,
    pub max_transact_size: u32,
    pub max_read_size: u32,
    pub max_write_size: u32,
    pub system_time: u64,
    pub security_blob: Vec<u8>,
}

/// NEGOTIATE response (MS-SMB2 2.2.4).
pub fn negotiate_response(hdr: &ResponseHeader, p: &NegotiateParams) -> Vec<u8> {
    let mut body = Vec::with_capacity(64 + p.security_blob.len());
    body.extend_from_slice(&65u16.to_le_bytes()); // StructureSize
    body.extend_from_slice(&0x0001u16.to_le_bytes()); // SecurityMode: SIGNING_ENABLED
    body.extend_from_slice(&p.dialect.to_le_bytes()); // DialectRevision
    body.extend_from_slice(&0u16.to_le_bytes()); // NegotiateContextCount / Reserved
    body.extend_from_slice(&p.server_guid); // ServerGuid
    body.extend_from_slice(&p.capabilities.to_le_bytes());
    body.extend_from_slice(&p.max_transact_size.to_le_bytes());
    body.extend_from_slice(&p.max_read_size.to_le_bytes());
    body.extend_from_slice(&p.max_write_size.to_le_bytes());
    body.extend_from_slice(&p.system_time.to_le_bytes()); // SystemTime
    body.extend_from_slice(&0u64.to_le_bytes()); // ServerStartTime (MUST be 0 for 2.x)
    let (offset, len) = if p.security_blob.is_empty() {
        (0u16, 0u16)
    } else {
        ((HEADER_LEN + 64) as u16, p.security_blob.len() as u16)
    };
    body.extend_from_slice(&offset.to_le_bytes()); // SecurityBufferOffset
    body.extend_from_slice(&len.to_le_bytes()); // SecurityBufferLength
    body.extend_from_slice(&0u32.to_le_bytes()); // NegotiateContextOffset / Reserved2
    debug_assert_eq!(body.len(), 64);
    if p.security_blob.is_empty() {
        body.push(0); // StructureSize 65 counts one byte of Buffer
    } else {
        body.extend_from_slice(&p.security_blob);
    }
    hdr.message(&body)
}

/// SESSION_SETUP `SessionFlags` (MS-SMB2 2.2.6).
pub const SESSION_FLAG_IS_GUEST: u16 = 0x0001;
pub const SESSION_FLAG_IS_NULL: u16 = 0x0002;

/// SESSION_SETUP response (MS-SMB2 2.2.6).
pub fn session_setup_response(hdr: &ResponseHeader, session_flags: u16, blob: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + blob.len().max(1));
    body.extend_from_slice(&9u16.to_le_bytes()); // StructureSize
    body.extend_from_slice(&session_flags.to_le_bytes());
    let offset = if blob.is_empty() {
        0u16
    } else {
        (HEADER_LEN + 8) as u16
    };
    body.extend_from_slice(&offset.to_le_bytes()); // SecurityBufferOffset
    body.extend_from_slice(&(blob.len() as u16).to_le_bytes()); // SecurityBufferLength
    if blob.is_empty() {
        body.push(0);
    } else {
        body.extend_from_slice(blob);
    }
    hdr.message(&body)
}

/// `ShareType` values in a TREE_CONNECT response.
pub const SHARE_TYPE_DISK: u8 = 0x01;
pub const SHARE_TYPE_PIPE: u8 = 0x02;

/// TREE_CONNECT response (MS-SMB2 2.2.10).
pub fn tree_connect_response(hdr: &ResponseHeader, share_type: u8, maximal_access: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&16u16.to_le_bytes()); // StructureSize
    body.push(share_type);
    body.push(0); // Reserved
    body.extend_from_slice(&0u32.to_le_bytes()); // ShareFlags: manual caching, no DFS
    body.extend_from_slice(&0u32.to_le_bytes()); // Capabilities
    body.extend_from_slice(&maximal_access.to_le_bytes()); // MaximalAccess
    hdr.message(&body)
}

/// A file's attributes and times as the info classes and CREATE/CLOSE responses carry them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileMeta {
    pub is_directory: bool,
    pub size: u64,
    /// FILETIME (100ns since 1601) used for all four timestamps; 0 when unknown.
    pub time: u64,
    /// A stable per-file number for `FileInternalInformation` and the `Id` directory classes.
    pub file_index: u64,
}

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

impl FileMeta {
    pub fn attributes(&self) -> u32 {
        if self.is_directory {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        }
    }

    /// Size rounded up to the 4 KiB allocation unit this server reports.
    pub fn allocation_size(&self) -> u64 {
        self.size.div_ceil(4096) * 4096
    }

    fn times(&self, out: &mut Vec<u8>) {
        for _ in 0..4 {
            out.extend_from_slice(&self.time.to_le_bytes());
        }
    }
}

/// `CreateAction` values (MS-SMB2 2.2.14).
pub const FILE_OPENED: u32 = 0x0000_0001;
pub const FILE_CREATED: u32 = 0x0000_0002;

/// CREATE response (MS-SMB2 2.2.14), with no create contexts.
pub fn create_response(
    hdr: &ResponseHeader,
    file_id: &[u8; 16],
    meta: &FileMeta,
    create_action: u32,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(89);
    body.extend_from_slice(&89u16.to_le_bytes()); // StructureSize
    body.push(0); // OplockLevel: none
    body.push(0); // Flags
    body.extend_from_slice(&create_action.to_le_bytes());
    meta.times(&mut body); // Creation, LastAccess, LastWrite, Change
    body.extend_from_slice(&meta.allocation_size().to_le_bytes());
    body.extend_from_slice(&meta.size.to_le_bytes()); // EndofFile
    body.extend_from_slice(&meta.attributes().to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes()); // Reserved2
    body.extend_from_slice(file_id);
    body.extend_from_slice(&0u32.to_le_bytes()); // CreateContextsOffset
    body.extend_from_slice(&0u32.to_le_bytes()); // CreateContextsLength
    body.push(0); // StructureSize 89 counts one byte of Buffer
    hdr.message(&body)
}

/// CLOSE `Flags`: the client asked for the attributes to be returned.
pub const CLOSE_FLAG_POSTQUERY_ATTRIB: u16 = 0x0001;

/// CLOSE response (MS-SMB2 2.2.16). With `meta`, the attributes are filled in and
/// `SMB2_CLOSE_FLAG_POSTQUERY_ATTRIB` is set; without it every field is zero, as the
/// specification requires when the client did not ask.
pub fn close_response(hdr: &ResponseHeader, meta: Option<&FileMeta>) -> Vec<u8> {
    let mut body = Vec::with_capacity(60);
    body.extend_from_slice(&60u16.to_le_bytes()); // StructureSize
    match meta {
        Some(meta) => {
            body.extend_from_slice(&CLOSE_FLAG_POSTQUERY_ATTRIB.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // Reserved
            meta.times(&mut body);
            body.extend_from_slice(&meta.allocation_size().to_le_bytes());
            body.extend_from_slice(&meta.size.to_le_bytes());
            body.extend_from_slice(&meta.attributes().to_le_bytes());
        }
        None => body.resize(60, 0),
    }
    hdr.message(&body)
}

/// Where READ puts the payload: straight after the 16-byte fixed body.
pub const READ_DATA_OFFSET: u8 = (HEADER_LEN + 16) as u8;

/// READ response (MS-SMB2 2.2.20).
pub fn read_response(hdr: &ResponseHeader, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + data.len());
    body.extend_from_slice(&17u16.to_le_bytes()); // StructureSize
    body.push(READ_DATA_OFFSET); // DataOffset, from the start of the header
    body.push(0); // Reserved
    body.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DataLength
    body.extend_from_slice(&0u32.to_le_bytes()); // DataRemaining
    body.extend_from_slice(&0u32.to_le_bytes()); // Reserved2 / Flags
    body.extend_from_slice(data);
    if data.is_empty() {
        body.push(0);
    }
    hdr.message(&body)
}

/// WRITE response (MS-SMB2 2.2.22).
pub fn write_response(hdr: &ResponseHeader, count: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(17);
    body.extend_from_slice(&17u16.to_le_bytes()); // StructureSize
    body.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    body.extend_from_slice(&count.to_le_bytes()); // Count
    body.extend_from_slice(&0u32.to_le_bytes()); // Remaining
    body.extend_from_slice(&0u16.to_le_bytes()); // WriteChannelInfoOffset
    body.extend_from_slice(&0u16.to_le_bytes()); // WriteChannelInfoLength
    body.push(0); // StructureSize 17 counts one byte of Buffer
    hdr.message(&body)
}

/// The QUERY_INFO and QUERY_DIRECTORY responses share one shape (MS-SMB2 2.2.38 / 2.2.34):
/// StructureSize 9, an offset and a length, then the buffer.
fn output_buffer_response(hdr: &ResponseHeader, buffer: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + buffer.len().max(1));
    body.extend_from_slice(&9u16.to_le_bytes()); // StructureSize
    body.extend_from_slice(&((HEADER_LEN + 8) as u16).to_le_bytes()); // OutputBufferOffset
    body.extend_from_slice(&(buffer.len() as u32).to_le_bytes()); // OutputBufferLength
    if buffer.is_empty() {
        body.push(0);
    } else {
        body.extend_from_slice(buffer);
    }
    hdr.message(&body)
}

/// QUERY_INFO response (MS-SMB2 2.2.38).
pub fn query_info_response(hdr: &ResponseHeader, buffer: &[u8]) -> Vec<u8> {
    output_buffer_response(hdr, buffer)
}

/// QUERY_DIRECTORY response (MS-SMB2 2.2.34).
pub fn query_directory_response(hdr: &ResponseHeader, entries: &[u8]) -> Vec<u8> {
    output_buffer_response(hdr, entries)
}

// ---------------------------------------------------------------------------------------------
// Information classes (MS-FSCC 2.4 and 2.5)
// ---------------------------------------------------------------------------------------------

/// `InfoType` in a QUERY_INFO request.
pub const INFO_FILE: u8 = 0x01;
pub const INFO_FILESYSTEM: u8 = 0x02;
pub const INFO_SECURITY: u8 = 0x03;
pub const INFO_QUOTA: u8 = 0x04;

/// File information classes (MS-FSCC 2.4).
pub mod file_class {
    pub const DIRECTORY: u8 = 1;
    pub const FULL_DIRECTORY: u8 = 2;
    pub const BOTH_DIRECTORY: u8 = 3;
    pub const BASIC: u8 = 4;
    pub const STANDARD: u8 = 5;
    pub const INTERNAL: u8 = 6;
    pub const EA: u8 = 7;
    pub const ACCESS: u8 = 8;
    pub const NAMES: u8 = 12;
    pub const POSITION: u8 = 14;
    pub const MODE: u8 = 16;
    pub const ALIGNMENT: u8 = 17;
    pub const ALL: u8 = 18;
    pub const STREAM: u8 = 22;
    pub const NETWORK_OPEN: u8 = 34;
    pub const ATTRIBUTE_TAG: u8 = 35;
    pub const ID_BOTH_DIRECTORY: u8 = 37;
    pub const ID_FULL_DIRECTORY: u8 = 38;
}

/// File system information classes (MS-FSCC 2.5).
pub mod fs_class {
    pub const VOLUME: u8 = 1;
    pub const SIZE: u8 = 3;
    pub const DEVICE: u8 = 4;
    pub const ATTRIBUTE: u8 = 5;
    pub const FULL_SIZE: u8 = 7;
    pub const SECTOR_SIZE: u8 = 11;
}

/// Whether answering this file information class needs the file's size, which only the model
/// knows. The others are answered from the handle alone.
pub fn file_class_needs_size(class: u8) -> bool {
    matches!(
        class,
        file_class::STANDARD | file_class::ALL | file_class::NETWORK_OPEN | file_class::STREAM
    )
}

/// Encode a file information class for `meta`. `None` for a class this server does not answer.
pub fn file_info(class: u8, meta: &FileMeta) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match class {
        file_class::BASIC => basic_info(meta, &mut out),
        file_class::STANDARD => standard_info(meta, &mut out),
        file_class::INTERNAL => out.extend_from_slice(&meta.file_index.to_le_bytes()),
        file_class::EA => out.extend_from_slice(&0u32.to_le_bytes()),
        file_class::ACCESS => out.extend_from_slice(&0x001F_01FFu32.to_le_bytes()),
        file_class::POSITION => out.extend_from_slice(&0u64.to_le_bytes()),
        file_class::MODE => out.extend_from_slice(&0u32.to_le_bytes()),
        file_class::ALIGNMENT => out.extend_from_slice(&0u32.to_le_bytes()),
        file_class::ALL => {
            basic_info(meta, &mut out); // 40
            standard_info(meta, &mut out); // 24
            out.extend_from_slice(&meta.file_index.to_le_bytes()); // Internal
            out.extend_from_slice(&0u32.to_le_bytes()); // EaSize
            out.extend_from_slice(&0x001F_01FFu32.to_le_bytes()); // AccessFlags
            out.extend_from_slice(&0u64.to_le_bytes()); // CurrentByteOffset
            out.extend_from_slice(&0u32.to_le_bytes()); // Mode
            out.extend_from_slice(&0u32.to_le_bytes()); // AlignmentRequirement
            out.extend_from_slice(&0u32.to_le_bytes()); // FileNameLength, no name
        }
        file_class::STREAM => {
            if meta.is_directory {
                return Some(Vec::new());
            }
            let name: Vec<u8> = utf16le("::$DATA");
            out.extend_from_slice(&0u32.to_le_bytes()); // NextEntryOffset
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(&meta.size.to_le_bytes());
            out.extend_from_slice(&meta.allocation_size().to_le_bytes());
            out.extend_from_slice(&name);
        }
        file_class::NETWORK_OPEN => {
            meta.times(&mut out);
            out.extend_from_slice(&meta.allocation_size().to_le_bytes());
            out.extend_from_slice(&meta.size.to_le_bytes());
            out.extend_from_slice(&meta.attributes().to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // Reserved
        }
        file_class::ATTRIBUTE_TAG => {
            out.extend_from_slice(&meta.attributes().to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // ReparseTag
        }
        _ => return None,
    }
    Some(out)
}

fn basic_info(meta: &FileMeta, out: &mut Vec<u8>) {
    meta.times(out);
    out.extend_from_slice(&meta.attributes().to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // Reserved
}

fn standard_info(meta: &FileMeta, out: &mut Vec<u8>) {
    out.extend_from_slice(&meta.allocation_size().to_le_bytes());
    out.extend_from_slice(&meta.size.to_le_bytes()); // EndOfFile
    out.extend_from_slice(&1u32.to_le_bytes()); // NumberOfLinks
    out.push(0); // DeletePending
    out.push(meta.is_directory as u8); // Directory
    out.extend_from_slice(&0u16.to_le_bytes()); // Reserved
}

/// The volume this server reports. None of it is measured — there is no disk behind the share
/// — so the numbers are fixed and say so: a 1 GiB volume, half free, 4 KiB clusters.
const FS_TOTAL_UNITS: u64 = 262_144;
const FS_FREE_UNITS: u64 = 131_072;
const FS_SECTORS_PER_UNIT: u32 = 8;
const FS_BYTES_PER_SECTOR: u32 = 512;
const FS_SERIAL: u32 = 0x4E47_5342; // "NGSB"

/// Encode a file system information class. `None` for a class this server does not answer.
pub fn fs_info(class: u8, volume_label: &str, created: u64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match class {
        fs_class::VOLUME => {
            let label = utf16le(volume_label);
            out.extend_from_slice(&created.to_le_bytes()); // VolumeCreationTime
            out.extend_from_slice(&FS_SERIAL.to_le_bytes());
            out.extend_from_slice(&(label.len() as u32).to_le_bytes());
            out.push(0); // SupportsObjects
            out.push(0); // Reserved
            out.extend_from_slice(&label);
        }
        fs_class::SIZE => {
            out.extend_from_slice(&FS_TOTAL_UNITS.to_le_bytes());
            out.extend_from_slice(&FS_FREE_UNITS.to_le_bytes());
            out.extend_from_slice(&FS_SECTORS_PER_UNIT.to_le_bytes());
            out.extend_from_slice(&FS_BYTES_PER_SECTOR.to_le_bytes());
        }
        fs_class::DEVICE => {
            out.extend_from_slice(&0x0000_0007u32.to_le_bytes()); // FILE_DEVICE_DISK
            out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
        }
        fs_class::ATTRIBUTE => {
            let name = utf16le("NTFS");
            // FILE_CASE_SENSITIVE_SEARCH | FILE_CASE_PRESERVED_NAMES | FILE_UNICODE_ON_DISK
            out.extend_from_slice(&0x0000_0007u32.to_le_bytes());
            out.extend_from_slice(&255u32.to_le_bytes()); // MaximumComponentNameLength
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(&name);
        }
        fs_class::FULL_SIZE => {
            out.extend_from_slice(&FS_TOTAL_UNITS.to_le_bytes());
            out.extend_from_slice(&FS_FREE_UNITS.to_le_bytes()); // CallerAvailable
            out.extend_from_slice(&FS_FREE_UNITS.to_le_bytes()); // ActualAvailable
            out.extend_from_slice(&FS_SECTORS_PER_UNIT.to_le_bytes());
            out.extend_from_slice(&FS_BYTES_PER_SECTOR.to_le_bytes());
        }
        fs_class::SECTOR_SIZE => {
            let sector = FS_BYTES_PER_SECTOR;
            out.extend_from_slice(&sector.to_le_bytes()); // LogicalBytesPerSector
            out.extend_from_slice(&sector.to_le_bytes()); // PhysicalBytesPerSectorForAtomicity
            out.extend_from_slice(&sector.to_le_bytes()); // PhysicalBytesPerSectorForPerformance
            out.extend_from_slice(&sector.to_le_bytes()); // FileSystemEffective...Atomicity
            out.extend_from_slice(&0u32.to_le_bytes()); // Flags
            out.extend_from_slice(&0u32.to_le_bytes()); // ByteOffsetForSectorAlignment
            out.extend_from_slice(&0u32.to_le_bytes()); // ByteOffsetForPartitionAlignment
        }
        _ => return None,
    }
    Some(out)
}

/// Whether QUERY_DIRECTORY can answer this information class.
pub fn directory_class_supported(class: u8) -> bool {
    matches!(
        class,
        file_class::DIRECTORY
            | file_class::FULL_DIRECTORY
            | file_class::BOTH_DIRECTORY
            | file_class::NAMES
            | file_class::ID_BOTH_DIRECTORY
            | file_class::ID_FULL_DIRECTORY
    )
}

/// One directory entry in `class`, without its `NextEntryOffset` set and without padding.
pub fn directory_entry(class: u8, name: &str, meta: &FileMeta) -> Vec<u8> {
    let name = utf16le(name);
    let mut e = Vec::with_capacity(104 + name.len());
    e.extend_from_slice(&0u32.to_le_bytes()); // NextEntryOffset, set by `join_directory_entries`
    e.extend_from_slice(&0u32.to_le_bytes()); // FileIndex
    if class == file_class::NAMES {
        e.extend_from_slice(&(name.len() as u32).to_le_bytes());
        e.extend_from_slice(&name);
        return e;
    }
    meta.times(&mut e);
    e.extend_from_slice(&meta.size.to_le_bytes()); // EndOfFile
    e.extend_from_slice(&meta.allocation_size().to_le_bytes());
    e.extend_from_slice(&meta.attributes().to_le_bytes());
    e.extend_from_slice(&(name.len() as u32).to_le_bytes()); // FileNameLength
    match class {
        file_class::DIRECTORY => {}
        file_class::FULL_DIRECTORY => e.extend_from_slice(&0u32.to_le_bytes()), // EaSize
        file_class::BOTH_DIRECTORY => {
            e.extend_from_slice(&0u32.to_le_bytes()); // EaSize
            e.push(0); // ShortNameLength
            e.push(0); // Reserved
            e.extend_from_slice(&[0u8; 24]); // ShortName
        }
        file_class::ID_BOTH_DIRECTORY => {
            e.extend_from_slice(&0u32.to_le_bytes()); // EaSize
            e.push(0); // ShortNameLength
            e.push(0); // Reserved1
            e.extend_from_slice(&[0u8; 24]); // ShortName
            e.extend_from_slice(&0u16.to_le_bytes()); // Reserved2
            e.extend_from_slice(&meta.file_index.to_le_bytes()); // FileId
        }
        file_class::ID_FULL_DIRECTORY => {
            e.extend_from_slice(&0u32.to_le_bytes()); // EaSize
            e.extend_from_slice(&0u32.to_le_bytes()); // Reserved
            e.extend_from_slice(&meta.file_index.to_le_bytes()); // FileId
        }
        _ => {}
    }
    e.extend_from_slice(&name);
    e
}

/// Join directory entries into one buffer (MS-FSCC 2.4): each entry but the last is padded to
/// 8 bytes and its `NextEntryOffset` names the padded length.
pub fn join_directory_entries(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let start = out.len();
        out.extend_from_slice(entry);
        if i + 1 < entries.len() {
            while (out.len() - start) % 8 != 0 {
                out.push(0);
            }
            let next = (out.len() - start) as u32;
            out[start..start + 4].copy_from_slice(&next.to_le_bytes());
        }
    }
    out
}

/// Size of an entry once padded to 8 bytes, which is what it costs in a joined buffer when it
/// is not the last.
pub fn padded_len(entry: &[u8]) -> usize {
    entry.len().div_ceil(8) * 8
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

pub fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
}

/// Decode UTF-16LE, dropping a trailing NUL a client may include in the length.
pub fn from_utf16le(bytes: &[u8]) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16(&units).ok()
}

/// Convert Unix seconds to a Windows FILETIME.
pub fn filetime_from_unix(secs: i64, nanos: u32) -> u64 {
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    let secs = secs.saturating_add(EPOCH_DIFF_SECS).max(0) as u64;
    secs.saturating_mul(10_000_000) + (nanos / 100) as u64
}

pub fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

pub fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

pub fn le64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}
