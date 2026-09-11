//! Modbus TCP wire format: MBAP header + PDU.
//!
//! Reference: *MODBUS Application Protocol Specification V1.1b3* and
//! *MODBUS Messaging on TCP/IP Implementation Guide V1.0b*.
//!
//! This module is deliberately free of any I/O, LLM or state: it turns bytes into
//! [`ModbusRequest`] values and turns answers back into bytes. Everything that the
//! specification determines mechanically (frame legality, quantity limits, the shape of
//! a write echo) lives here; everything a device would *decide* (what a register reads
//! as, whether a write is accepted) lives in `actions.rs` and is answered by the model.
//!
//! There is no register file here, by design — see `src/server/modbus/CLAUDE.md`.

/// Length of the MBAP header: transaction id (2), protocol id (2), length (2), unit id (1).
pub const MBAP_HEADER_LEN: usize = 7;

/// Protocol identifier reserved for Modbus in the MBAP header.
pub const MODBUS_PROTOCOL_ID: u16 = 0;

/// Largest legal PDU (function code + data), per the Modbus spec.
pub const MAX_PDU_LEN: usize = 253;

/// Largest legal ADU on TCP: MBAP header + PDU.
pub const MAX_ADU_LEN: usize = MBAP_HEADER_LEN + MAX_PDU_LEN;

// ---------------------------------------------------------------------------
// Function codes
// ---------------------------------------------------------------------------

pub const FC_READ_COILS: u8 = 0x01;
pub const FC_READ_DISCRETE_INPUTS: u8 = 0x02;
pub const FC_READ_HOLDING_REGISTERS: u8 = 0x03;
pub const FC_READ_INPUT_REGISTERS: u8 = 0x04;
pub const FC_WRITE_SINGLE_COIL: u8 = 0x05;
pub const FC_WRITE_SINGLE_REGISTER: u8 = 0x06;
pub const FC_WRITE_MULTIPLE_COILS: u8 = 0x0F;
pub const FC_WRITE_MULTIPLE_REGISTERS: u8 = 0x10;

/// Bit OR-ed into the function code of an exception response.
pub const EXCEPTION_FLAG: u8 = 0x80;

// ---------------------------------------------------------------------------
// Exception codes
// ---------------------------------------------------------------------------

pub const EXC_ILLEGAL_FUNCTION: u8 = 0x01;
pub const EXC_ILLEGAL_DATA_ADDRESS: u8 = 0x02;
pub const EXC_ILLEGAL_DATA_VALUE: u8 = 0x03;
pub const EXC_SERVER_DEVICE_FAILURE: u8 = 0x04;
pub const EXC_ACKNOWLEDGE: u8 = 0x05;
pub const EXC_SERVER_DEVICE_BUSY: u8 = 0x06;
pub const EXC_MEMORY_PARITY_ERROR: u8 = 0x08;
pub const EXC_GATEWAY_PATH_UNAVAILABLE: u8 = 0x0A;
pub const EXC_GATEWAY_TARGET_FAILED: u8 = 0x0B;

/// What [`exception_name`] returns for a code the specification does not define.
///
/// Named rather than spelled out at each site: `execute_action` refuses any code that maps
/// to it, so the two have to agree.
pub const UNKNOWN_EXCEPTION: &str = "unknown_exception";

/// Canonical name for an exception code, for logs and for the model's vocabulary.
pub fn exception_name(code: u8) -> &'static str {
    match code {
        EXC_ILLEGAL_FUNCTION => "illegal_function",
        EXC_ILLEGAL_DATA_ADDRESS => "illegal_data_address",
        EXC_ILLEGAL_DATA_VALUE => "illegal_data_value",
        EXC_SERVER_DEVICE_FAILURE => "server_device_failure",
        EXC_ACKNOWLEDGE => "acknowledge",
        EXC_SERVER_DEVICE_BUSY => "server_device_busy",
        EXC_MEMORY_PARITY_ERROR => "memory_parity_error",
        EXC_GATEWAY_PATH_UNAVAILABLE => "gateway_path_unavailable",
        EXC_GATEWAY_TARGET_FAILED => "gateway_target_device_failed_to_respond",
        _ => UNKNOWN_EXCEPTION,
    }
}

/// Map a canonical exception name back to its code.
pub fn exception_code_from_name(name: &str) -> Option<u8> {
    match name.trim().to_ascii_lowercase().as_str() {
        "illegal_function" | "illegal function" => Some(EXC_ILLEGAL_FUNCTION),
        "illegal_data_address" | "illegal data address" => Some(EXC_ILLEGAL_DATA_ADDRESS),
        "illegal_data_value" | "illegal data value" => Some(EXC_ILLEGAL_DATA_VALUE),
        "server_device_failure" | "server device failure" | "device_failure" => {
            Some(EXC_SERVER_DEVICE_FAILURE)
        }
        "acknowledge" => Some(EXC_ACKNOWLEDGE),
        "server_device_busy" | "server device busy" | "device_busy" => Some(EXC_SERVER_DEVICE_BUSY),
        "memory_parity_error" | "memory parity error" => Some(EXC_MEMORY_PARITY_ERROR),
        "gateway_path_unavailable" | "gateway path unavailable" => {
            Some(EXC_GATEWAY_PATH_UNAVAILABLE)
        }
        "gateway_target_device_failed_to_respond" | "gateway_target_failed" => {
            Some(EXC_GATEWAY_TARGET_FAILED)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Why a byte stream could not be turned into a Modbus ADU.
///
/// Both variants are fatal for the connection: neither can be answered with a Modbus
/// exception, because we no longer know where the next frame begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The MBAP protocol identifier was not 0, so this is not Modbus at all.
    NotModbus { protocol_id: u16 },
    /// The MBAP length field is outside the legal range `2..=254`
    /// (unit id + 1..=253 PDU bytes).
    BadLength { length: u16 },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::NotModbus { protocol_id } => write!(
                f,
                "MBAP protocol identifier {protocol_id} is not 0; this is not a Modbus/TCP stream"
            ),
            FrameError::BadLength { length } => write!(
                f,
                "MBAP length field {length} is outside the legal range 2..=254"
            ),
        }
    }
}

/// A complete Modbus/TCP Application Data Unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adu {
    pub transaction_id: u16,
    pub unit_id: u8,
    pub pdu: Vec<u8>,
}

/// Try to pull one complete ADU off the front of `buf`.
///
/// Returns `Ok(None)` when more bytes are needed (the caller should accumulate and retry),
/// `Ok(Some((adu, consumed)))` on success, and `Err` when the stream is not Modbus.
pub fn try_parse_adu(buf: &[u8]) -> Result<Option<(Adu, usize)>, FrameError> {
    if buf.len() < MBAP_HEADER_LEN {
        return Ok(None);
    }

    let transaction_id = u16::from_be_bytes([buf[0], buf[1]]);
    let protocol_id = u16::from_be_bytes([buf[2], buf[3]]);
    let length = u16::from_be_bytes([buf[4], buf[5]]);

    if protocol_id != MODBUS_PROTOCOL_ID {
        return Err(FrameError::NotModbus { protocol_id });
    }

    // `length` counts the unit id plus the PDU.
    if length < 2 || length as usize > MAX_PDU_LEN + 1 {
        return Err(FrameError::BadLength { length });
    }

    let total = 6 + length as usize;
    if buf.len() < total {
        return Ok(None);
    }

    let unit_id = buf[6];
    let pdu = buf[MBAP_HEADER_LEN..total].to_vec();

    Ok(Some((
        Adu {
            transaction_id,
            unit_id,
            pdu,
        },
        total,
    )))
}

/// Wrap a PDU in an MBAP header addressed back to the requester.
pub fn encode_adu(transaction_id: u16, unit_id: u8, pdu: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MBAP_HEADER_LEN + pdu.len());
    out.extend_from_slice(&transaction_id.to_be_bytes());
    out.extend_from_slice(&MODBUS_PROTOCOL_ID.to_be_bytes());
    out.extend_from_slice(&((pdu.len() as u16) + 1).to_be_bytes());
    out.push(unit_id);
    out.extend_from_slice(pdu);
    out
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A decoded, spec-legal Modbus request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModbusRequest {
    ReadCoils { start: u16, quantity: u16 },
    ReadDiscreteInputs { start: u16, quantity: u16 },
    ReadHoldingRegisters { start: u16, quantity: u16 },
    ReadInputRegisters { start: u16, quantity: u16 },
    WriteSingleCoil { address: u16, value: bool },
    WriteSingleRegister { address: u16, value: u16 },
    WriteMultipleCoils { start: u16, values: Vec<bool> },
    WriteMultipleRegisters { start: u16, values: Vec<u16> },
}

impl ModbusRequest {
    pub fn function_code(&self) -> u8 {
        match self {
            ModbusRequest::ReadCoils { .. } => FC_READ_COILS,
            ModbusRequest::ReadDiscreteInputs { .. } => FC_READ_DISCRETE_INPUTS,
            ModbusRequest::ReadHoldingRegisters { .. } => FC_READ_HOLDING_REGISTERS,
            ModbusRequest::ReadInputRegisters { .. } => FC_READ_INPUT_REGISTERS,
            ModbusRequest::WriteSingleCoil { .. } => FC_WRITE_SINGLE_COIL,
            ModbusRequest::WriteSingleRegister { .. } => FC_WRITE_SINGLE_REGISTER,
            ModbusRequest::WriteMultipleCoils { .. } => FC_WRITE_MULTIPLE_COILS,
            ModbusRequest::WriteMultipleRegisters { .. } => FC_WRITE_MULTIPLE_REGISTERS,
        }
    }

    /// Stable, human/model-readable name of the function.
    pub fn function_name(&self) -> &'static str {
        match self {
            ModbusRequest::ReadCoils { .. } => "read_coils",
            ModbusRequest::ReadDiscreteInputs { .. } => "read_discrete_inputs",
            ModbusRequest::ReadHoldingRegisters { .. } => "read_holding_registers",
            ModbusRequest::ReadInputRegisters { .. } => "read_input_registers",
            ModbusRequest::WriteSingleCoil { .. } => "write_single_coil",
            ModbusRequest::WriteSingleRegister { .. } => "write_single_register",
            ModbusRequest::WriteMultipleCoils { .. } => "write_multiple_coils",
            ModbusRequest::WriteMultipleRegisters { .. } => "write_multiple_registers",
        }
    }

    /// First address touched by this request.
    pub fn start_address(&self) -> u16 {
        match self {
            ModbusRequest::ReadCoils { start, .. }
            | ModbusRequest::ReadDiscreteInputs { start, .. }
            | ModbusRequest::ReadHoldingRegisters { start, .. }
            | ModbusRequest::ReadInputRegisters { start, .. }
            | ModbusRequest::WriteMultipleCoils { start, .. }
            | ModbusRequest::WriteMultipleRegisters { start, .. } => *start,
            ModbusRequest::WriteSingleCoil { address, .. }
            | ModbusRequest::WriteSingleRegister { address, .. } => *address,
        }
    }

    /// How many coils/registers this request touches.
    pub fn quantity(&self) -> u16 {
        match self {
            ModbusRequest::ReadCoils { quantity, .. }
            | ModbusRequest::ReadDiscreteInputs { quantity, .. }
            | ModbusRequest::ReadHoldingRegisters { quantity, .. }
            | ModbusRequest::ReadInputRegisters { quantity, .. } => *quantity,
            ModbusRequest::WriteSingleCoil { .. } | ModbusRequest::WriteSingleRegister { .. } => 1,
            ModbusRequest::WriteMultipleCoils { values, .. } => values.len() as u16,
            ModbusRequest::WriteMultipleRegisters { values, .. } => values.len() as u16,
        }
    }

    /// True for FC 1/2 — the answer is a list of bits.
    pub fn is_bit_read(&self) -> bool {
        matches!(
            self,
            ModbusRequest::ReadCoils { .. } | ModbusRequest::ReadDiscreteInputs { .. }
        )
    }

    /// True for FC 3/4 — the answer is a list of 16-bit registers.
    pub fn is_register_read(&self) -> bool {
        matches!(
            self,
            ModbusRequest::ReadHoldingRegisters { .. } | ModbusRequest::ReadInputRegisters { .. }
        )
    }

    /// True for FC 5/6/15/16.
    pub fn is_write(&self) -> bool {
        !self.is_bit_read() && !self.is_register_read()
    }
}

/// Decode a PDU.
///
/// The error carries the exception code the specification prescribes, which the caller
/// answers with directly — no model round-trip, because none of these is a decision:
/// an unknown function code is *always* `0x01`, a quantity of 0 is *always* `0x03`.
pub fn parse_request(pdu: &[u8]) -> Result<ModbusRequest, u8> {
    let Some(&fc) = pdu.first() else {
        return Err(EXC_ILLEGAL_FUNCTION);
    };
    let body = &pdu[1..];

    // Reads and the two "write multiple" headers all start with a u16 pair.
    let pair = |b: &[u8]| -> Option<(u16, u16)> {
        if b.len() < 4 {
            None
        } else {
            Some((
                u16::from_be_bytes([b[0], b[1]]),
                u16::from_be_bytes([b[2], b[3]]),
            ))
        }
    };

    match fc {
        FC_READ_COILS | FC_READ_DISCRETE_INPUTS => {
            let (start, quantity) = pair(body).ok_or(EXC_ILLEGAL_DATA_VALUE)?;
            if !(1..=2000).contains(&quantity) {
                return Err(EXC_ILLEGAL_DATA_VALUE);
            }
            check_range(start, quantity)?;
            Ok(if fc == FC_READ_COILS {
                ModbusRequest::ReadCoils { start, quantity }
            } else {
                ModbusRequest::ReadDiscreteInputs { start, quantity }
            })
        }
        FC_READ_HOLDING_REGISTERS | FC_READ_INPUT_REGISTERS => {
            let (start, quantity) = pair(body).ok_or(EXC_ILLEGAL_DATA_VALUE)?;
            if !(1..=125).contains(&quantity) {
                return Err(EXC_ILLEGAL_DATA_VALUE);
            }
            check_range(start, quantity)?;
            Ok(if fc == FC_READ_HOLDING_REGISTERS {
                ModbusRequest::ReadHoldingRegisters { start, quantity }
            } else {
                ModbusRequest::ReadInputRegisters { start, quantity }
            })
        }
        FC_WRITE_SINGLE_COIL => {
            let (address, raw) = pair(body).ok_or(EXC_ILLEGAL_DATA_VALUE)?;
            let value = match raw {
                0x0000 => false,
                0xFF00 => true,
                // The spec allows exactly these two encodings; anything else is a
                // data-value error, not a device decision.
                _ => return Err(EXC_ILLEGAL_DATA_VALUE),
            };
            Ok(ModbusRequest::WriteSingleCoil { address, value })
        }
        FC_WRITE_SINGLE_REGISTER => {
            let (address, value) = pair(body).ok_or(EXC_ILLEGAL_DATA_VALUE)?;
            Ok(ModbusRequest::WriteSingleRegister { address, value })
        }
        FC_WRITE_MULTIPLE_COILS => {
            let (start, quantity) = pair(body).ok_or(EXC_ILLEGAL_DATA_VALUE)?;
            if !(1..=1968).contains(&quantity) {
                return Err(EXC_ILLEGAL_DATA_VALUE);
            }
            check_range(start, quantity)?;
            let byte_count = *body.get(4).ok_or(EXC_ILLEGAL_DATA_VALUE)? as usize;
            let expected = quantity.div_ceil(8) as usize;
            if byte_count != expected || body.len() < 5 + byte_count {
                return Err(EXC_ILLEGAL_DATA_VALUE);
            }
            let data = &body[5..5 + byte_count];
            let values = (0..quantity as usize)
                .map(|i| data[i / 8] & (1 << (i % 8)) != 0)
                .collect();
            Ok(ModbusRequest::WriteMultipleCoils { start, values })
        }
        FC_WRITE_MULTIPLE_REGISTERS => {
            let (start, quantity) = pair(body).ok_or(EXC_ILLEGAL_DATA_VALUE)?;
            if !(1..=123).contains(&quantity) {
                return Err(EXC_ILLEGAL_DATA_VALUE);
            }
            check_range(start, quantity)?;
            let byte_count = *body.get(4).ok_or(EXC_ILLEGAL_DATA_VALUE)? as usize;
            if byte_count != quantity as usize * 2 || body.len() < 5 + byte_count {
                return Err(EXC_ILLEGAL_DATA_VALUE);
            }
            let data = &body[5..5 + byte_count];
            let values = data
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            Ok(ModbusRequest::WriteMultipleRegisters { start, values })
        }
        _ => Err(EXC_ILLEGAL_FUNCTION),
    }
}

/// `start + quantity` must stay inside the 16-bit address space.
fn check_range(start: u16, quantity: u16) -> Result<(), u8> {
    if start as u32 + quantity as u32 > 0x1_0000 {
        Err(EXC_ILLEGAL_DATA_ADDRESS)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// PDU for a FC 1/2 read response: byte count then packed bits, least-significant bit
/// of the first byte carrying the first requested coil.
pub fn encode_bits_response(function_code: u8, values: &[bool]) -> Vec<u8> {
    let byte_count = values.len().div_ceil(8);
    let mut pdu = Vec::with_capacity(2 + byte_count);
    pdu.push(function_code);
    pdu.push(byte_count as u8);
    let mut packed = vec![0u8; byte_count];
    for (i, &on) in values.iter().enumerate() {
        if on {
            packed[i / 8] |= 1 << (i % 8);
        }
    }
    pdu.extend_from_slice(&packed);
    pdu
}

/// PDU for a FC 3/4 read response: byte count then big-endian registers.
pub fn encode_registers_response(function_code: u8, values: &[u16]) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(2 + values.len() * 2);
    pdu.push(function_code);
    pdu.push((values.len() * 2) as u8);
    for v in values {
        pdu.extend_from_slice(&v.to_be_bytes());
    }
    pdu
}

/// PDU acknowledging a write.
///
/// FC 5 and 6 echo the request verbatim; FC 15 and 16 echo the starting address and the
/// quantity written. Reconstructed from the parsed request rather than copied from the
/// input bytes, so a malformed echo cannot be produced.
pub fn encode_write_ack(request: &ModbusRequest) -> Vec<u8> {
    let mut pdu = Vec::with_capacity(5);
    pdu.push(request.function_code());
    match request {
        ModbusRequest::WriteSingleCoil { address, value } => {
            pdu.extend_from_slice(&address.to_be_bytes());
            pdu.extend_from_slice(&if *value { 0xFF00u16 } else { 0x0000u16 }.to_be_bytes());
        }
        ModbusRequest::WriteSingleRegister { address, value } => {
            pdu.extend_from_slice(&address.to_be_bytes());
            pdu.extend_from_slice(&value.to_be_bytes());
        }
        ModbusRequest::WriteMultipleCoils { start, values } => {
            pdu.extend_from_slice(&start.to_be_bytes());
            pdu.extend_from_slice(&(values.len() as u16).to_be_bytes());
        }
        ModbusRequest::WriteMultipleRegisters { start, values } => {
            pdu.extend_from_slice(&start.to_be_bytes());
            pdu.extend_from_slice(&(values.len() as u16).to_be_bytes());
        }
        // Not a write; the caller checks this, but produce something legal rather than
        // panicking on a programming error.
        _ => {
            pdu.clear();
            pdu.push(request.function_code() | EXCEPTION_FLAG);
            pdu.push(EXC_SERVER_DEVICE_FAILURE);
        }
    }
    pdu
}

/// PDU for an exception response: `function_code | 0x80`, then the exception code.
pub fn encode_exception(function_code: u8, exception_code: u8) -> Vec<u8> {
    vec![function_code | EXCEPTION_FLAG, exception_code]
}
