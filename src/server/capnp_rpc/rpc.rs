//! The RPC messages NetGet speaks (rpc.capnp, level 1 without promise resolution), shared by the
//! server and the client. Offsets read from `capnp compile -ocapnp rpc.capnp`; booleans whose
//! declared default is true are stored inverted, as the encoding requires.
use super::layout::{Builder, Message, StructBuilder, StructReader, Target};
use anyhow::{Context, Result};

pub const MSG_UNIMPLEMENTED: u16 = 0;
pub const MSG_ABORT: u16 = 1;
pub const MSG_CALL: u16 = 2;
pub const MSG_RETURN: u16 = 3;
pub const MSG_FINISH: u16 = 4;
pub const MSG_RESOLVE: u16 = 5;
pub const MSG_RELEASE: u16 = 6;
pub const MSG_BOOTSTRAP: u16 = 8;
pub const MSG_DISEMBARGO: u16 = 13;

/// Exception.Type.
pub const EXC_FAILED: u16 = 0;
pub const EXC_OVERLOADED: u16 = 1;
pub const EXC_UNIMPLEMENTED: u16 = 3;

pub fn exception_type(name: &str) -> Option<u16> {
    match name {
        "failed" => Some(EXC_FAILED),
        "overloaded" => Some(EXC_OVERLOADED),
        "disconnected" => Some(2),
        "unimplemented" => Some(EXC_UNIMPLEMENTED),
        _ => None,
    }
}

pub fn exception_name(t: u16) -> &'static str {
    match t {
        0 => "failed",
        1 => "overloaded",
        2 => "disconnected",
        3 => "unimplemented",
        _ => "unknown",
    }
}

/// Where a call is addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTarget {
    Imported(u32),
    /// A promised answer, with how many pointer-field steps it takes into the results.
    Promised {
        question: u32,
        steps: u32,
    },
}

pub enum Returned<'a> {
    /// The payload's content and the export ids named by its cap table.
    Results {
        content: Target<'a>,
        caps: Vec<Option<u32>>,
    },
    Exception {
        reason: String,
        kind: u16,
    },
    Other,
}

pub enum Incoming<'a> {
    Bootstrap {
        question: u32,
    },
    Call {
        question: u32,
        target: CallTarget,
        interface: u64,
        method: u16,
        content: Target<'a>,
    },
    Return {
        answer: u32,
        returned: Returned<'a>,
    },
    Finish {
        question: u32,
    },
    Release,
    Resolve,
    Abort {
        reason: String,
    },
    Unimplemented,
    /// Anything else, by its union tag: answered with `unimplemented`.
    Other(u16),
}

fn body<'a>(m: &StructReader<'a>) -> Result<StructReader<'a>> {
    m.struct_field(0)?.context("message without a body")
}

pub fn decode(msg: &Message) -> Result<Incoming<'_>> {
    let m = msg.root_struct()?;
    let tag = m.u16(0);
    Ok(match tag {
        MSG_BOOTSTRAP => Incoming::Bootstrap {
            question: body(&m)?.u32(0),
        },
        MSG_CALL => {
            let c = body(&m)?;
            let t = c.struct_field(0)?.context("call without a target")?;
            let target = match t.u16(2) {
                0 => CallTarget::Imported(t.u32(0)),
                _ => {
                    let p = t.struct_field(0)?.context("promised answer missing")?;
                    CallTarget::Promised {
                        question: p.u32(0),
                        steps: p.list_field(0)?.map_or(0, |l| l.len),
                    }
                }
            };
            let content = match c.struct_field(1)? {
                Some(payload) => payload.pointer(0)?,
                None => Target::Null,
            };
            Incoming::Call {
                question: c.u32(0),
                target,
                interface: c.u64(1),
                method: c.u16(2),
                content,
            }
        }
        MSG_RETURN => {
            let r = body(&m)?;
            let returned = match r.u16(3) {
                0 => {
                    let payload = r.struct_field(0)?;
                    let mut caps = Vec::new();
                    if let Some(table) = payload
                        .as_ref()
                        .map(|p| p.list_field(1))
                        .transpose()?
                        .flatten()
                    {
                        for i in 0..table.len {
                            let d = table.struct_at(i)?;
                            caps.push(match d.u16(0) {
                                1 | 2 => Some(d.u32(1)),
                                _ => None,
                            });
                        }
                    }
                    Returned::Results {
                        content: match payload {
                            Some(p) => p.pointer(0)?,
                            None => Target::Null,
                        },
                        caps,
                    }
                }
                1 => {
                    let e = r.struct_field(0)?;
                    Returned::Exception {
                        reason: e
                            .as_ref()
                            .map(|e| e.text(0))
                            .transpose()?
                            .flatten()
                            .unwrap_or_default(),
                        kind: e.map_or(0, |e| e.u16(2)),
                    }
                }
                _ => Returned::Other,
            };
            Incoming::Return {
                answer: r.u32(0),
                returned,
            }
        }
        MSG_FINISH => Incoming::Finish {
            question: body(&m)?.u32(0),
        },
        MSG_RELEASE => Incoming::Release,
        MSG_RESOLVE => Incoming::Resolve,
        MSG_ABORT => Incoming::Abort {
            reason: m
                .struct_field(0)?
                .map(|e| e.text(0))
                .transpose()?
                .flatten()
                .unwrap_or_default(),
        },
        MSG_UNIMPLEMENTED => Incoming::Unimplemented,
        other => Incoming::Other(other),
    })
}

fn message(tag: u16) -> (Builder, StructBuilder) {
    let (mut b, m) = Builder::with_root(1, 1);
    b.set_u16(m, 0, tag);
    (b, m)
}

pub fn bootstrap(question: u32) -> Result<Vec<u64>> {
    let (mut b, m) = message(MSG_BOOTSTRAP);
    let s = b.init_struct(m, 0, 1, 1)?;
    b.set_u32(s, 0, question);
    Ok(b.words)
}

/// A Return whose results are built by `fill`, which receives the Payload struct.
pub fn return_results(
    answer: u32,
    fill: impl FnOnce(&mut Builder, StructBuilder) -> Result<()>,
) -> Result<Vec<u64>> {
    let (mut b, m) = message(MSG_RETURN);
    let r = b.init_struct(m, 0, 2, 1)?;
    b.set_u32(r, 0, answer);
    // union tag 0 = results; releaseParamCaps keeps its default (true).
    let payload = b.init_struct(r, 0, 0, 2)?;
    fill(&mut b, payload)?;
    Ok(b.words)
}

/// A Return whose results are the capability this side exports as `export`.
pub fn return_capability(answer: u32, export: u32) -> Result<Vec<u64>> {
    return_results(answer, |b, payload| {
        b.set_capability(payload, 0, 0)?;
        let descriptors = b.init_struct_list(payload, 1, 1, 1, 1)?;
        b.set_u16(descriptors[0], 0, 1); // senderHosted
        b.set_u32(descriptors[0], 1, export);
        Ok(())
    })
}

pub fn return_exception(answer: u32, reason: &str, kind: u16) -> Result<Vec<u64>> {
    let (mut b, m) = message(MSG_RETURN);
    let r = b.init_struct(m, 0, 2, 1)?;
    b.set_u32(r, 0, answer);
    b.set_u16(r, 3, 1);
    let e = b.init_struct(r, 0, 1, 2)?;
    b.set_text(e, 0, reason)?;
    b.set_u16(e, 2, kind);
    Ok(b.words)
}

/// A Call on an imported capability; `fill` receives the Payload struct.
pub fn call(
    question: u32,
    import: u32,
    interface: u64,
    method: u16,
    fill: impl FnOnce(&mut Builder, StructBuilder) -> Result<()>,
) -> Result<Vec<u64>> {
    let (mut b, m) = message(MSG_CALL);
    let c = b.init_struct(m, 0, 3, 3)?;
    b.set_u32(c, 0, question);
    b.set_u16(c, 2, method);
    b.set_u64(c, 1, interface);
    let t = b.init_struct(c, 0, 1, 1)?;
    b.set_u32(t, 0, import);
    let payload = b.init_struct(c, 1, 0, 2)?;
    fill(&mut b, payload)?;
    Ok(b.words)
}

/// Finish a question. `release_result_caps` false keeps the capabilities its results carried
/// (the bootstrap capability must survive its question); the field defaults to true, so false
/// is stored as a set bit.
pub fn finish(question: u32, release_result_caps: bool) -> Result<Vec<u64>> {
    let (mut b, m) = message(MSG_FINISH);
    let f = b.init_struct(m, 0, 1, 0)?;
    b.set_u32(f, 0, question);
    b.set_bool(f, 32, !release_result_caps);
    Ok(b.words)
}

pub fn abort(reason: &str) -> Result<Vec<u64>> {
    let (mut b, m) = message(MSG_ABORT);
    let e = b.init_struct(m, 0, 1, 2)?;
    b.set_text(e, 0, reason)?;
    b.set_u16(e, 2, EXC_FAILED);
    Ok(b.words)
}

/// `unimplemented`, echoing the message it answers.
pub fn unimplemented(original: &Message) -> Result<Vec<u64>> {
    let (mut b, m) = message(MSG_UNIMPLEMENTED);
    let slot = Builder::pointer_slot_of(m, 0)?;
    b.copy_into(slot, original.root()?)?;
    Ok(b.words)
}
