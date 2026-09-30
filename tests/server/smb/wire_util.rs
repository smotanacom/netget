//! Hand-built SMB2 requests and the Direct TCP framing every one of them rides in.
//!
//! Written from MS-SMB2 rather than from `src/server/smb/wire.rs`, so a test that uses these
//! builders checks the server against the specification and not against itself. Every request
//! is returned *unframed*; [`nbss`] adds the 4-byte transport header (MS-SMB2 2.1) that a real
//! client puts in front of each message on port 445.

#![allow(dead_code)]

use std::io::Read;

pub const NEGOTIATE: u16 = 0x0000;
pub const SESSION_SETUP: u16 = 0x0001;
pub const LOGOFF: u16 = 0x0002;
pub const TREE_CONNECT: u16 = 0x0003;
pub const TREE_DISCONNECT: u16 = 0x0004;
pub const CREATE: u16 = 0x0005;
pub const CLOSE: u16 = 0x0006;
pub const FLUSH: u16 = 0x0007;
pub const LOCK: u16 = 0x000A;
pub const IOCTL: u16 = 0x000B;
pub const CANCEL: u16 = 0x000C;
pub const SET_INFO: u16 = 0x0011;
pub const READ: u16 = 0x0008;
pub const WRITE: u16 = 0x0009;
pub const ECHO: u16 = 0x000D;
pub const QUERY_DIRECTORY: u16 = 0x000E;
pub const QUERY_INFO: u16 = 0x0010;

pub const STATUS_SUCCESS: u32 = 0;
pub const STATUS_MORE_PROCESSING_REQUIRED: u32 = 0xC000_0016;
pub const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
pub const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
pub const STATUS_USER_SESSION_DELETED: u32 = 0xC000_0203;
pub const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;
pub const STATUS_END_OF_FILE: u32 = 0xC000_0011;
pub const STATUS_FILE_CLOSED: u32 = 0xC000_0128;
pub const STATUS_INVALID_DEVICE_REQUEST: u32 = 0xC000_0010;
pub const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;
pub const STATUS_INSUFFICIENT_RESOURCES: u32 = 0xC000_009A;
pub const STATUS_TOO_MANY_OPENED_FILES: u32 = 0xC000_011F;

/// A bare NTLMSSP NEGOTIATE (MS-NLMP 2.2.1.1): signature, MessageType 1, NegotiateFlags
/// (UNICODE | NTLM | ALWAYS_SIGN), and empty domain and workstation fields.
pub fn ntlmssp_negotiate() -> Vec<u8> {
    let mut t = b"NTLMSSP\0".to_vec();
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&0x0000_8201u32.to_le_bytes());
    t.extend_from_slice(&[0u8; 16]);
    t
}

/// An NTLMSSP AUTHENTICATE too short to carry its own fixed part (MS-NLMP 2.2.1.3 is 64 bytes
/// before any payload), so the server cannot read a user name out of it.
pub fn ntlmssp_truncated_authenticate() -> Vec<u8> {
    let mut t = b"NTLMSSP\0".to_vec();
    t.extend_from_slice(&3u32.to_le_bytes());
    t.extend_from_slice(&[0u8; 20]);
    t
}

/// Prefix a message with its Direct TCP transport header: a zero byte and a 24-bit length.
pub fn nbss(message: Vec<u8>) -> Vec<u8> {
    let len = message.len() as u32;
    assert!(len < 1 << 24, "message too long for a Direct TCP frame");
    let mut out = vec![0u8];
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.extend(message);
    out
}

/// A Direct TCP header announcing `len` bytes, for tests that lie about the length.
pub fn nbss_header(len: usize) -> [u8; 4] {
    let b = (len as u32).to_be_bytes();
    [0, b[1], b[2], b[3]]
}

/// A 64-byte SMB2 request header (MS-SMB2 2.2.1.2, SYNC).
pub fn header(command: u16, message_id: u64, tree_id: u32, session_id: u64) -> Vec<u8> {
    let mut h = Vec::with_capacity(64);
    h.extend_from_slice(b"\xFESMB"); // 0  ProtocolId
    h.extend_from_slice(&64u16.to_le_bytes()); // 4  StructureSize
    h.extend_from_slice(&0u16.to_le_bytes()); // 6  CreditCharge
    h.extend_from_slice(&0u32.to_le_bytes()); // 8  Status
    h.extend_from_slice(&command.to_le_bytes()); // 12 Command
    h.extend_from_slice(&8u16.to_le_bytes()); // 14 CreditRequest
    h.extend_from_slice(&0u32.to_le_bytes()); // 16 Flags
    h.extend_from_slice(&0u32.to_le_bytes()); // 20 NextCommand
    h.extend_from_slice(&message_id.to_le_bytes()); // 24 MessageId
    h.extend_from_slice(&0u32.to_le_bytes()); // 32 Reserved
    h.extend_from_slice(&tree_id.to_le_bytes()); // 36 TreeId
    h.extend_from_slice(&session_id.to_le_bytes()); // 40 SessionId
    h.extend_from_slice(&[0u8; 16]); // 48 Signature
    h
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
}

/// NEGOTIATE (MS-SMB2 2.2.3) offering 2.0.2 and 2.1.
pub fn negotiate(message_id: u64) -> Vec<u8> {
    let mut p = header(NEGOTIATE, message_id, 0, 0);
    p.extend_from_slice(&36u16.to_le_bytes()); // StructureSize
    p.extend_from_slice(&2u16.to_le_bytes()); // DialectCount
    p.extend_from_slice(&1u16.to_le_bytes()); // SecurityMode: signing enabled
    p.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    p.extend_from_slice(&0u32.to_le_bytes()); // Capabilities
    p.extend_from_slice(&[0x11; 16]); // ClientGuid
    p.extend_from_slice(&0u64.to_le_bytes()); // ClientStartTime
    p.extend_from_slice(&0x0202u16.to_le_bytes());
    p.extend_from_slice(&0x0210u16.to_le_bytes());
    p
}

/// SESSION_SETUP (MS-SMB2 2.2.5) with an empty security buffer: a one-step guest login.
pub fn session_setup(message_id: u64) -> Vec<u8> {
    session_setup_with(message_id, 0, &[])
}

/// SESSION_SETUP carrying `blob` on `session_id`.
pub fn session_setup_with(message_id: u64, session_id: u64, blob: &[u8]) -> Vec<u8> {
    let mut p = header(SESSION_SETUP, message_id, 0, session_id);
    p.extend_from_slice(&25u16.to_le_bytes()); // StructureSize
    p.push(0); // Flags
    p.push(1); // SecurityMode
    p.extend_from_slice(&0u32.to_le_bytes()); // Capabilities
    p.extend_from_slice(&0u32.to_le_bytes()); // Channel
    p.extend_from_slice(&88u16.to_le_bytes()); // SecurityBufferOffset (64 + 24)
    p.extend_from_slice(&(blob.len() as u16).to_le_bytes()); // SecurityBufferLength
    p.extend_from_slice(&0u64.to_le_bytes()); // PreviousSessionId
    p.extend_from_slice(blob);
    p
}

/// TREE_CONNECT (MS-SMB2 2.2.9) to `unc`, e.g. `\\127.0.0.1\share`.
pub fn tree_connect(message_id: u64, session_id: u64, unc: &str) -> Vec<u8> {
    let path = utf16(unc);
    let mut p = header(TREE_CONNECT, message_id, 0, session_id);
    p.extend_from_slice(&9u16.to_le_bytes()); // StructureSize
    p.extend_from_slice(&0u16.to_le_bytes()); // Flags
    p.extend_from_slice(&72u16.to_le_bytes()); // PathOffset (64 + 8)
    p.extend_from_slice(&(path.len() as u16).to_le_bytes()); // PathLength
    p.extend_from_slice(&path);
    p
}

/// CREATE (MS-SMB2 2.2.13) opening `name` (share-relative, `\`-separated) for read.
pub fn create(message_id: u64, tree_id: u32, session_id: u64, name: &str) -> Vec<u8> {
    create_with(message_id, tree_id, session_id, name, 0)
}

/// CREATE with explicit `CreateOptions` (1 = FILE_DIRECTORY_FILE, 0x40 = NON_DIRECTORY).
pub fn create_with(
    message_id: u64,
    tree_id: u32,
    session_id: u64,
    name: &str,
    options: u32,
) -> Vec<u8> {
    let name = utf16(name);
    let mut p = header(CREATE, message_id, tree_id, session_id);
    p.extend_from_slice(&57u16.to_le_bytes()); // StructureSize
    p.push(0); // SecurityFlags
    p.push(0); // RequestedOplockLevel
    p.extend_from_slice(&2u32.to_le_bytes()); // ImpersonationLevel
    p.extend_from_slice(&[0u8; 8]); // SmbCreateFlags
    p.extend_from_slice(&[0u8; 8]); // Reserved
    p.extend_from_slice(&0x0012_0089u32.to_le_bytes()); // DesiredAccess
    p.extend_from_slice(&0u32.to_le_bytes()); // FileAttributes
    p.extend_from_slice(&7u32.to_le_bytes()); // ShareAccess
    p.extend_from_slice(&1u32.to_le_bytes()); // CreateDisposition: FILE_OPEN
    p.extend_from_slice(&options.to_le_bytes()); // CreateOptions
    p.extend_from_slice(&120u16.to_le_bytes()); // NameOffset (64 + 56)
    p.extend_from_slice(&(name.len() as u16).to_le_bytes()); // NameLength
    p.extend_from_slice(&0u32.to_le_bytes()); // CreateContextsOffset
    p.extend_from_slice(&0u32.to_le_bytes()); // CreateContextsLength
    if name.is_empty() {
        p.push(0); // StructureSize 57 counts one byte of Buffer
    } else {
        p.extend_from_slice(&name);
    }
    p
}

/// CLOSE (MS-SMB2 2.2.15).
pub fn close(message_id: u64, tree_id: u32, session_id: u64, file_id: &[u8]) -> Vec<u8> {
    let mut p = header(CLOSE, message_id, tree_id, session_id);
    p.extend_from_slice(&24u16.to_le_bytes()); // StructureSize
    p.extend_from_slice(&0u16.to_le_bytes()); // Flags
    p.extend_from_slice(&0u32.to_le_bytes()); // Reserved
    p.extend_from_slice(file_id);
    p
}

/// READ (MS-SMB2 2.2.19).
pub fn read(
    message_id: u64,
    tree_id: u32,
    session_id: u64,
    file_id: &[u8],
    offset: u64,
    length: u32,
) -> Vec<u8> {
    let mut p = header(READ, message_id, tree_id, session_id);
    p.extend_from_slice(&49u16.to_le_bytes()); // StructureSize
    p.push(0x50); // Padding
    p.push(0); // Flags
    p.extend_from_slice(&length.to_le_bytes()); // Length
    p.extend_from_slice(&offset.to_le_bytes()); // Offset
    p.extend_from_slice(file_id); // FileId
    p.extend_from_slice(&0u32.to_le_bytes()); // MinimumCount
    p.extend_from_slice(&0u32.to_le_bytes()); // Channel
    p.extend_from_slice(&0u32.to_le_bytes()); // RemainingBytes
    p.extend_from_slice(&0u16.to_le_bytes()); // ReadChannelInfoOffset
    p.extend_from_slice(&0u16.to_le_bytes()); // ReadChannelInfoLength
    p.push(0); // Buffer
    p
}

/// WRITE (MS-SMB2 2.2.21) of `data` at offset 0.
pub fn write(
    message_id: u64,
    tree_id: u32,
    session_id: u64,
    file_id: &[u8],
    data: &[u8],
) -> Vec<u8> {
    let mut p = write_fixed(message_id, tree_id, session_id, file_id, data.len() as u32);
    p.extend_from_slice(data);
    p
}

/// A WRITE's header and fixed body declaring `length` bytes, with no data after it.
pub fn write_fixed(
    message_id: u64,
    tree_id: u32,
    session_id: u64,
    file_id: &[u8],
    length: u32,
) -> Vec<u8> {
    let mut p = header(WRITE, message_id, tree_id, session_id);
    p.extend_from_slice(&49u16.to_le_bytes()); // StructureSize
    p.extend_from_slice(&112u16.to_le_bytes()); // DataOffset (64 + 48)
    p.extend_from_slice(&length.to_le_bytes()); // Length
    p.extend_from_slice(&0u64.to_le_bytes()); // Offset
    p.extend_from_slice(file_id); // FileId
    p.extend_from_slice(&0u32.to_le_bytes()); // Channel
    p.extend_from_slice(&0u32.to_le_bytes()); // RemainingBytes
    p.extend_from_slice(&0u16.to_le_bytes()); // WriteChannelInfoOffset
    p.extend_from_slice(&0u16.to_le_bytes()); // WriteChannelInfoLength
    p.extend_from_slice(&0u32.to_le_bytes()); // Flags
    assert_eq!(p.len(), 112);
    p
}

/// QUERY_INFO (MS-SMB2 2.2.37).
pub fn query_info(
    message_id: u64,
    tree_id: u32,
    session_id: u64,
    file_id: &[u8],
    info_type: u8,
    class: u8,
) -> Vec<u8> {
    let mut p = header(QUERY_INFO, message_id, tree_id, session_id);
    p.extend_from_slice(&41u16.to_le_bytes()); // StructureSize
    p.push(info_type);
    p.push(class);
    p.extend_from_slice(&0xFFFFu32.to_le_bytes()); // OutputBufferLength
    p.extend_from_slice(&0u16.to_le_bytes()); // InputBufferOffset
    p.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    p.extend_from_slice(&0u32.to_le_bytes()); // InputBufferLength
    p.extend_from_slice(&0u32.to_le_bytes()); // AdditionalInformation
    p.extend_from_slice(&0u32.to_le_bytes()); // Flags
    p.extend_from_slice(file_id);
    p.push(0); // Buffer
    p
}

/// QUERY_DIRECTORY (MS-SMB2 2.2.33) for `pattern`.
pub fn query_directory(
    message_id: u64,
    tree_id: u32,
    session_id: u64,
    file_id: &[u8],
    class: u8,
    pattern: &str,
) -> Vec<u8> {
    let name = utf16(pattern);
    let mut p = header(QUERY_DIRECTORY, message_id, tree_id, session_id);
    p.extend_from_slice(&33u16.to_le_bytes()); // StructureSize
    p.push(class);
    p.push(0); // Flags
    p.extend_from_slice(&0u32.to_le_bytes()); // FileIndex
    p.extend_from_slice(file_id);
    p.extend_from_slice(&96u16.to_le_bytes()); // FileNameOffset (64 + 32)
    p.extend_from_slice(&(name.len() as u16).to_le_bytes()); // FileNameLength
    p.extend_from_slice(&0xFFFFu32.to_le_bytes()); // OutputBufferLength
    p.extend_from_slice(&name);
    p
}

/// A request whose body is the four-byte `StructureSize = 4` shape: LOGOFF,
/// TREE_DISCONNECT, ECHO.
pub fn simple(command: u16, message_id: u64, tree_id: u32, session_id: u64) -> Vec<u8> {
    let mut p = header(command, message_id, tree_id, session_id);
    p.extend_from_slice(&[4, 0, 0, 0]);
    p
}

/// FLUSH (MS-SMB2 2.2.17).
pub fn flush(message_id: u64, tree_id: u32, session_id: u64, file_id: &[u8]) -> Vec<u8> {
    let mut p = header(FLUSH, message_id, tree_id, session_id);
    p.extend_from_slice(&24u16.to_le_bytes()); // StructureSize
    p.extend_from_slice(&0u16.to_le_bytes()); // Reserved1
    p.extend_from_slice(&0u32.to_le_bytes()); // Reserved2
    p.extend_from_slice(file_id);
    p
}

/// IOCTL (MS-SMB2 2.2.31) carrying `ctl_code` on `file_id`, with no input.
pub fn ioctl(
    message_id: u64,
    tree_id: u32,
    session_id: u64,
    file_id: &[u8],
    ctl_code: u32,
) -> Vec<u8> {
    let mut p = header(IOCTL, message_id, tree_id, session_id);
    p.extend_from_slice(&57u16.to_le_bytes()); // StructureSize
    p.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    p.extend_from_slice(&ctl_code.to_le_bytes()); // CtlCode
    p.extend_from_slice(file_id); // FileId
    p.extend_from_slice(&0u32.to_le_bytes()); // InputOffset
    p.extend_from_slice(&0u32.to_le_bytes()); // InputCount
    p.extend_from_slice(&0u32.to_le_bytes()); // MaxInputResponse
    p.extend_from_slice(&0u32.to_le_bytes()); // OutputOffset
    p.extend_from_slice(&0u32.to_le_bytes()); // OutputCount
    p.extend_from_slice(&1024u32.to_le_bytes()); // MaxOutputResponse
    p.extend_from_slice(&1u32.to_le_bytes()); // Flags: SMB2_0_IOCTL_IS_FSCTL
    p.extend_from_slice(&0u32.to_le_bytes()); // Reserved2
    p.push(0); // StructureSize 57 counts one byte of Buffer
    p
}

/// SET_INFO (MS-SMB2 2.2.39) of FileDispositionInformation (delete pending) on `file_id`.
pub fn set_info_delete(message_id: u64, tree_id: u32, session_id: u64, file_id: &[u8]) -> Vec<u8> {
    let mut p = header(SET_INFO, message_id, tree_id, session_id);
    p.extend_from_slice(&33u16.to_le_bytes()); // StructureSize
    p.push(1); // InfoType: SMB2_0_INFO_FILE
    p.push(13); // FileInfoClass: FileDispositionInformation
    p.extend_from_slice(&1u32.to_le_bytes()); // BufferLength
    p.extend_from_slice(&96u16.to_le_bytes()); // BufferOffset (64 + 32)
    p.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    p.extend_from_slice(&0u32.to_le_bytes()); // AdditionalInformation
    p.extend_from_slice(file_id); // FileId
    p.push(1); // DeletePending
    p
}

/// LOCK (MS-SMB2 2.2.26) of the first byte of `file_id`, exclusively.
pub fn lock(message_id: u64, tree_id: u32, session_id: u64, file_id: &[u8]) -> Vec<u8> {
    let mut p = header(LOCK, message_id, tree_id, session_id);
    p.extend_from_slice(&48u16.to_le_bytes()); // StructureSize
    p.extend_from_slice(&1u16.to_le_bytes()); // LockCount
    p.extend_from_slice(&0u32.to_le_bytes()); // LockSequence
    p.extend_from_slice(file_id); // FileId
    p.extend_from_slice(&0u64.to_le_bytes()); // Offset
    p.extend_from_slice(&1u64.to_le_bytes()); // Length
    p.extend_from_slice(&2u32.to_le_bytes()); // Flags: SMB2_LOCKFLAG_EXCLUSIVE_LOCK
    p.extend_from_slice(&0u32.to_le_bytes()); // Reserved
    p
}

/// Link `messages` into one compound chain (MS-SMB2 3.2.4.1.4): every message but the last is
/// padded to 8 bytes and its `NextCommand` names that padded length.
pub fn compound(messages: Vec<Vec<u8>>) -> Vec<u8> {
    let count = messages.len();
    let mut out = Vec::new();
    for (i, mut m) in messages.into_iter().enumerate() {
        if i + 1 < count {
            while m.len() % 8 != 0 {
                m.push(0);
            }
            let next = m.len() as u32;
            m[20..24].copy_from_slice(&next.to_le_bytes());
        }
        out.extend(m);
    }
    out
}

/// Split a compound response on `NextCommand`.
pub fn split_compound(frame: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        let rest = &frame[at..];
        let next = next_command(rest) as usize;
        if next == 0 {
            out.push(rest.to_vec());
            return out;
        }
        out.push(rest[..next].to_vec());
        at += next;
    }
}

/// Read one Direct TCP frame and return the SMB2 message inside it.
pub fn read_frame_sync(stream: &mut std::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let mut nb = [0u8; 4];
    stream.read_exact(&mut nb)?;
    if nb[0] != 0 {
        return Err(std::io::Error::other(format!(
            "not a Direct TCP session message: {nb:02x?}"
        )));
    }
    let len = u32::from_be_bytes([0, nb[1], nb[2], nb[3]]) as usize;
    let mut msg = vec![0u8; len];
    stream.read_exact(&mut msg)?;
    Ok(msg)
}

/// Read one frame into `buf` (replacing its contents); returns the message length.
pub fn read_frame_into(
    stream: &mut std::net::TcpStream,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let msg = read_frame_sync(stream)?;
    buf.clear();
    buf.extend_from_slice(&msg);
    Ok(msg.len())
}

/// Async [`read_frame_sync`].
pub async fn read_frame(stream: &mut tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut nb = [0u8; 4];
    stream.read_exact(&mut nb).await?;
    if nb[0] != 0 {
        return Err(std::io::Error::other(format!(
            "not a Direct TCP session message: {nb:02x?}"
        )));
    }
    let len = u32::from_be_bytes([0, nb[1], nb[2], nb[3]]) as usize;
    let mut msg = vec![0u8; len];
    stream.read_exact(&mut msg).await?;
    Ok(msg)
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

pub fn status(resp: &[u8]) -> u32 {
    le32(resp, 8)
}

pub fn command(resp: &[u8]) -> u16 {
    u16::from_le_bytes([resp[12], resp[13]])
}

pub fn flags(resp: &[u8]) -> u32 {
    le32(resp, 16)
}

pub fn next_command(resp: &[u8]) -> u32 {
    le32(resp, 20)
}

pub fn message_id(resp: &[u8]) -> u64 {
    le64(resp, 24)
}

pub fn tree_id(resp: &[u8]) -> u32 {
    le32(resp, 36)
}

pub fn session_id(resp: &[u8]) -> u64 {
    le64(resp, 40)
}

/// The FileId of a CREATE response (MS-SMB2 2.2.14: body offset 64).
pub fn create_file_id(resp: &[u8]) -> Vec<u8> {
    resp[64 + 64..64 + 80].to_vec()
}

/// The payload of a READ response, located through its own DataOffset/DataLength.
pub fn read_payload(resp: &[u8]) -> Vec<u8> {
    let offset = resp[64 + 2] as usize;
    let len = le32(resp, 64 + 4) as usize;
    resp[offset..offset + len].to_vec()
}

/// The output buffer of a QUERY_INFO or QUERY_DIRECTORY response.
pub fn output_buffer(resp: &[u8]) -> Vec<u8> {
    let offset = u16::from_le_bytes([resp[64 + 2], resp[64 + 3]]) as usize;
    let len = le32(resp, 64 + 4) as usize;
    resp[offset..offset + len].to_vec()
}
