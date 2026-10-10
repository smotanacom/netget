//! Cap'n Proto's encoding: the segment framing, a bounded pointer-following reader and a
//! single-segment builder. Hand-written so every read is checked where it happens: a pointer
//! must land inside its segment, every struct and list visited is charged to a traversal budget,
//! and nesting stops at `MAX_DEPTH`.
use anyhow::{bail, ensure, Context, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest message, framing included.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MESSAGE_WORDS: usize = MAX_MESSAGE_BYTES / 8;
/// Most segments in one message.
pub const MAX_SEGMENTS: usize = 64;
/// Deepest nesting of structs and lists the reader follows.
pub const MAX_DEPTH: u32 = 64;
/// Words the reader may visit in one message, counting every struct and list it resolves,
/// so a message that points at the same bytes many times cannot amplify.
pub const TRAVERSAL_LIMIT_WORDS: u64 = 4 * MAX_MESSAGE_WORDS as u64;
/// A message must complete within this long of its first byte.
pub const MESSAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read one framed message. `Ok(None)` is a clean end of stream before its first byte.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    idle: Duration,
) -> Result<Option<Vec<Vec<u64>>>> {
    let mut first = [0u8; 4];
    match tokio::time::timeout(idle, reader.read(&mut first[..1])).await {
        Err(_) => bail!("connection idle past its deadline"),
        Ok(Ok(0)) => return Ok(None),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e.into()),
    }
    tokio::time::timeout(MESSAGE_TIMEOUT, async {
        reader.read_exact(&mut first[1..]).await?;
        let count = u32::from_le_bytes(first) as usize + 1;
        ensure!(
            count <= MAX_SEGMENTS,
            "message has {count} segments, more than {MAX_SEGMENTS}"
        );
        // Sizes, then padding to a word boundary.
        let table_words = (count + 1).div_ceil(2) * 2 - 1;
        let mut table = vec![0u8; table_words * 4];
        reader.read_exact(&mut table).await?;
        let sizes: Vec<usize> = (0..count)
            .map(|i| u32::from_le_bytes(table[i * 4..i * 4 + 4].try_into().unwrap()) as usize)
            .collect();
        let total: usize = sizes.iter().sum();
        ensure!(
            total + table_words.div_ceil(2) < MAX_MESSAGE_WORDS,
            "message exceeds {MAX_MESSAGE_BYTES} bytes"
        );
        let mut segments = Vec::with_capacity(count);
        for size in sizes {
            let mut bytes = vec![0u8; size * 8];
            reader.read_exact(&mut bytes).await?;
            segments.push(
                bytes
                    .chunks_exact(8)
                    .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                    .collect(),
            );
        }
        Ok(Some(segments))
    })
    .await
    .context("message not completed within its deadline")?
}

/// Frame a single-segment message.
pub fn frame(words: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + words.len() * 8);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(words.len() as u32).to_le_bytes());
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElementSize {
    Void,
    Bit,
    Byte,
    TwoBytes,
    FourBytes,
    EightBytes,
    Pointer,
    Composite,
}

impl ElementSize {
    fn from_bits(b: u64) -> Self {
        match b {
            0 => Self::Void,
            1 => Self::Bit,
            2 => Self::Byte,
            3 => Self::TwoBytes,
            4 => Self::FourBytes,
            5 => Self::EightBytes,
            6 => Self::Pointer,
            _ => Self::Composite,
        }
    }
    fn bits(self) -> u64 {
        match self {
            Self::Void => 0,
            Self::Bit => 1,
            Self::Byte => 2,
            Self::TwoBytes => 3,
            Self::FourBytes => 4,
            Self::EightBytes => 5,
            Self::Pointer => 6,
            Self::Composite => 7,
        }
    }
    /// Bits per element for the non-composite sizes.
    pub fn width(self) -> u64 {
        match self {
            Self::Void => 0,
            Self::Bit => 1,
            Self::Byte => 8,
            Self::TwoBytes => 16,
            Self::FourBytes => 32,
            Self::EightBytes | Self::Pointer => 64,
            Self::Composite => 0,
        }
    }
}

/// A message being read, with its traversal budget.
pub struct Message {
    segments: Vec<Vec<u64>>,
    budget: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
pub struct StructReader<'a> {
    msg: &'a Message,
    seg: usize,
    data: usize,
    data_words: u16,
    ptrs: usize,
    ptr_count: u16,
    pub depth: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct ListReader<'a> {
    msg: &'a Message,
    seg: usize,
    start: usize,
    pub len: u32,
    pub size: ElementSize,
    /// For composite lists: the per-element layout.
    data_words: u16,
    ptr_count: u16,
    depth: u32,
}

/// What a pointer points at.
pub enum Target<'a> {
    Null,
    Struct(StructReader<'a>),
    List(ListReader<'a>),
    Capability(u32),
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Message({} segments)", self.segments.len())
    }
}

impl Message {
    pub fn new(segments: Vec<Vec<u64>>) -> Self {
        Self {
            segments,
            budget: AtomicU64::new(TRAVERSAL_LIMIT_WORDS),
        }
    }

    fn charge(&self, words: u64) -> Result<()> {
        let left = self.budget.load(Ordering::Relaxed);
        ensure!(words.max(1) <= left, "message traversal limit exceeded");
        self.budget.store(left - words.max(1), Ordering::Relaxed);
        Ok(())
    }

    fn word(&self, seg: usize, at: usize) -> Result<u64> {
        self.segments
            .get(seg)
            .and_then(|s| s.get(at))
            .copied()
            .context("pointer outside its segment")
    }

    fn check_range(&self, seg: usize, start: usize, words: u64) -> Result<()> {
        let len = self.segments.get(seg).context("no such segment")?.len() as u64;
        ensure!(
            start as u64 + words <= len,
            "pointer target outside its segment"
        );
        Ok(())
    }

    /// The root pointer.
    pub fn root(&self) -> Result<Target<'_>> {
        ensure!(
            self.segments.first().is_some_and(|s| !s.is_empty()),
            "empty message"
        );
        self.follow(0, 0, 0)
    }

    pub fn root_struct(&self) -> Result<StructReader<'_>> {
        match self.root()? {
            Target::Struct(s) => Ok(s),
            _ => bail!("the root is not a struct"),
        }
    }

    /// Resolve the pointer stored at `(seg, at)`, following far pointers.
    fn follow(&self, seg: usize, at: usize, depth: u32) -> Result<Target<'_>> {
        ensure!(depth <= MAX_DEPTH, "nesting deeper than {MAX_DEPTH}");
        let mut word = self.word(seg, at)?;
        if word == 0 {
            return Ok(Target::Null);
        }
        let (mut seg, mut origin) = (seg, at as i64 + 1);
        let mut tag_override = None;
        if word & 3 == 2 {
            let double = word & 4 != 0;
            let pad = ((word >> 3) & 0x1fff_ffff) as usize;
            let target_seg = (word >> 32) as usize;
            if !double {
                word = self.word(target_seg, pad)?;
                ensure!(word & 3 != 2, "far pointer to a far pointer");
                seg = target_seg;
                origin = pad as i64 + 1;
            } else {
                let landing = self.word(target_seg, pad)?;
                let tag = self.word(target_seg, pad + 1)?;
                ensure!(
                    landing & 7 == 2,
                    "double-far landing pad is not a single far pointer"
                );
                seg = (landing >> 32) as usize;
                origin = ((landing >> 3) & 0x1fff_ffff) as i64;
                ensure!(tag & 3 != 2, "double-far tag is a far pointer");
                tag_override = Some(tag);
                word = tag;
            }
        }
        let offset = |w: u64| -> i64 {
            if tag_override.is_some() {
                0
            } else {
                ((w as u32 as i32) >> 2) as i64
            }
        };
        match word & 3 {
            0 => {
                let data_words = (word >> 32) as u16;
                let ptr_count = (word >> 48) as u16;
                let start = origin + offset(word);
                ensure!(start >= 0, "pointer before its segment");
                let size = u64::from(data_words) + u64::from(ptr_count);
                self.check_range(seg, start as usize, size)?;
                self.charge(size)?;
                Ok(Target::Struct(StructReader {
                    msg: self,
                    seg,
                    data: start as usize,
                    data_words,
                    ptrs: start as usize + data_words as usize,
                    ptr_count,
                    depth: depth + 1,
                }))
            }
            1 => {
                let size = ElementSize::from_bits((word >> 32) & 7);
                let count = (word >> 35) as u32;
                let start = origin + offset(word);
                ensure!(start >= 0, "pointer before its segment");
                let start = start as usize;
                if size == ElementSize::Composite {
                    let words = u64::from(count);
                    self.check_range(seg, start, words + 1)?;
                    let tag = self.word(seg, start)?;
                    ensure!(tag & 3 == 0, "composite list tag is not a struct");
                    let len = ((tag as u32) >> 2) as u64;
                    let data_words = (tag >> 32) as u16;
                    let ptr_count = (tag >> 48) as u16;
                    let per = u64::from(data_words) + u64::from(ptr_count);
                    ensure!(
                        len * per <= words,
                        "composite list elements overrun its word count"
                    );
                    // A list of zero-sized structs still costs a word an element.
                    self.charge(words.max(len))?;
                    Ok(Target::List(ListReader {
                        msg: self,
                        seg,
                        start: start + 1,
                        len: len as u32,
                        size,
                        data_words,
                        ptr_count,
                        depth: depth + 1,
                    }))
                } else {
                    let bits = u64::from(count) * size.width();
                    self.check_range(seg, start, bits.div_ceil(64))?;
                    self.charge(if size == ElementSize::Void {
                        u64::from(count)
                    } else {
                        bits.div_ceil(64)
                    })?;
                    Ok(Target::List(ListReader {
                        msg: self,
                        seg,
                        start,
                        len: count,
                        size,
                        data_words: 0,
                        ptr_count: 0,
                        depth: depth + 1,
                    }))
                }
            }
            3 => {
                ensure!((word as u32) >> 2 == 0, "unknown 'other' pointer");
                Ok(Target::Capability((word >> 32) as u32))
            }
            _ => bail!("far pointer where content was expected"),
        }
    }
}

impl<'a> StructReader<'a> {
    /// `bits` wide, at bit `offset` of the data section; zero beyond it.
    pub fn bits(&self, offset: u64, bits: u64) -> u64 {
        let word = (offset / 64) as usize;
        if word >= self.data_words as usize {
            return 0;
        }
        let w = self.msg.segments[self.seg][self.data + word];
        let shift = offset % 64;
        let v = w >> shift;
        if bits == 64 {
            v
        } else {
            v & ((1u64 << bits) - 1)
        }
    }
    pub fn u16(&self, index: u64) -> u16 {
        self.bits(index * 16, 16) as u16
    }
    pub fn u32(&self, index: u64) -> u32 {
        self.bits(index * 32, 32) as u32
    }
    pub fn u64(&self, index: u64) -> u64 {
        self.bits(index * 64, 64)
    }
    pub fn bool(&self, bit: u64) -> bool {
        self.bits(bit, 1) != 0
    }
    pub fn pointer(&self, index: u16) -> Result<Target<'a>> {
        if index >= self.ptr_count {
            return Ok(Target::Null);
        }
        self.msg
            .follow(self.seg, self.ptrs + index as usize, self.depth)
    }
    pub fn struct_field(&self, index: u16) -> Result<Option<StructReader<'a>>> {
        match self.pointer(index)? {
            Target::Null => Ok(None),
            Target::Struct(s) => Ok(Some(s)),
            _ => bail!("pointer {index} is not a struct"),
        }
    }
    pub fn list_field(&self, index: u16) -> Result<Option<ListReader<'a>>> {
        match self.pointer(index)? {
            Target::Null => Ok(None),
            Target::List(l) => Ok(Some(l)),
            _ => bail!("pointer {index} is not a list"),
        }
    }
    pub fn text(&self, index: u16) -> Result<Option<String>> {
        self.list_field(index)?.map(|l| l.text()).transpose()
    }
    pub fn data(&self, index: u16) -> Result<Option<Vec<u8>>> {
        self.list_field(index)?.map(|l| l.bytes()).transpose()
    }
}

impl<'a> ListReader<'a> {
    fn word(&self, i: usize) -> u64 {
        self.msg.segments[self.seg][self.start + i]
    }
    /// The bytes of a byte list.
    pub fn bytes(&self) -> Result<Vec<u8>> {
        ensure!(self.size == ElementSize::Byte, "not a byte list");
        let mut out = Vec::with_capacity(self.len as usize);
        for i in 0..self.len as usize {
            out.push((self.word(i / 8) >> ((i % 8) * 8)) as u8);
        }
        Ok(out)
    }
    /// Text: a NUL-terminated byte list.
    pub fn text(&self) -> Result<String> {
        let mut bytes = self.bytes()?;
        ensure!(bytes.pop() == Some(0), "text is not NUL-terminated");
        String::from_utf8(bytes).context("text is not UTF-8")
    }
    /// Element `i` of a primitive list, as raw bits.
    pub fn primitive(&self, i: u32) -> Result<u64> {
        ensure!(i < self.len, "index past the list");
        let width = match self.size {
            ElementSize::Composite => bail!("composite list read as primitive"),
            ElementSize::Pointer => bail!("pointer list read as primitive"),
            s => s.width(),
        };
        if width == 0 {
            return Ok(0);
        }
        let bit = u64::from(i) * width;
        let w = self.word((bit / 64) as usize) >> (bit % 64);
        Ok(if width == 64 {
            w
        } else {
            w & ((1 << width) - 1)
        })
    }
    /// Element `i` of a struct list. A primitive list read as structs reads each element as a
    /// one-field struct, as the encoding allows.
    pub fn struct_at(&self, i: u32) -> Result<StructReader<'a>> {
        ensure!(i < self.len, "index past the list");
        ensure!(
            self.size == ElementSize::Composite,
            "expected a list of structs"
        );
        let per = self.data_words as usize + self.ptr_count as usize;
        let at = self.start + per * i as usize;
        Ok(StructReader {
            msg: self.msg,
            seg: self.seg,
            data: at,
            data_words: self.data_words,
            ptrs: at + self.data_words as usize,
            ptr_count: self.ptr_count,
            depth: self.depth,
        })
    }
    /// Element `i` of a pointer list.
    pub fn pointer_at(&self, i: u32) -> Result<Target<'a>> {
        ensure!(i < self.len, "index past the list");
        ensure!(self.size == ElementSize::Pointer, "expected a pointer list");
        self.msg
            .follow(self.seg, self.start + i as usize, self.depth)
    }
}

/// A single-segment message under construction.
#[derive(Default)]
pub struct Builder {
    pub words: Vec<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct StructBuilder {
    pub data: usize,
    pub data_words: u16,
    pub ptrs: usize,
    pub ptr_count: u16,
}

impl Builder {
    /// A message whose root is a new struct of the given size.
    pub fn with_root(data_words: u16, ptr_count: u16) -> (Self, StructBuilder) {
        let mut b = Self { words: vec![0] };
        let s = b.alloc_struct(data_words, ptr_count);
        b.point_struct(0, s);
        (b, s)
    }

    fn alloc(&mut self, words: usize) -> Result<usize> {
        let at = self.words.len();
        ensure!(
            at + words < MAX_MESSAGE_WORDS,
            "message exceeds {MAX_MESSAGE_BYTES} bytes"
        );
        self.words.resize(at + words, 0);
        Ok(at)
    }

    fn alloc_struct(&mut self, data_words: u16, ptr_count: u16) -> StructBuilder {
        let at = self
            .alloc(data_words as usize + ptr_count as usize)
            .unwrap_or(0);
        StructBuilder {
            data: at,
            data_words,
            ptrs: at + data_words as usize,
            ptr_count,
        }
    }

    fn offset(at: usize, target: usize) -> u64 {
        (((target as i64 - at as i64 - 1) as i32 as u32) << 2) as u64
    }

    fn point_struct(&mut self, at: usize, s: StructBuilder) {
        self.words[at] = Self::offset(at, s.data)
            | (u64::from(s.data_words) << 32)
            | (u64::from(s.ptr_count) << 48);
    }

    /// Store `bits` wide at bit `offset` of the data section.
    pub fn set_bits(&mut self, s: StructBuilder, offset: u64, bits: u64, value: u64) {
        let word = (offset / 64) as usize;
        if word >= s.data_words as usize {
            return;
        }
        let shift = offset % 64;
        let mask = if bits == 64 {
            u64::MAX
        } else {
            ((1u64 << bits) - 1) << shift
        };
        let w = &mut self.words[s.data + word];
        *w = (*w & !mask) | ((value << shift) & mask);
    }
    pub fn set_u16(&mut self, s: StructBuilder, index: u64, v: u16) {
        self.set_bits(s, index * 16, 16, u64::from(v));
    }
    pub fn set_u32(&mut self, s: StructBuilder, index: u64, v: u32) {
        self.set_bits(s, index * 32, 32, u64::from(v));
    }
    pub fn set_u64(&mut self, s: StructBuilder, index: u64, v: u64) {
        self.set_bits(s, index * 64, 64, v);
    }
    pub fn set_bool(&mut self, s: StructBuilder, bit: u64, v: bool) {
        self.set_bits(s, bit, 1, u64::from(v));
    }

    fn pointer_slot(s: StructBuilder, index: u16) -> Result<usize> {
        ensure!(index < s.ptr_count, "pointer {index} outside the struct");
        Ok(s.ptrs + index as usize)
    }

    /// A new struct stored in pointer `index` of `s`.
    pub fn init_struct(
        &mut self,
        s: StructBuilder,
        index: u16,
        data_words: u16,
        ptr_count: u16,
    ) -> Result<StructBuilder> {
        let slot = Self::pointer_slot(s, index)?;
        self.struct_at_slot(slot, data_words, ptr_count)
    }

    fn struct_at_slot(
        &mut self,
        slot: usize,
        data_words: u16,
        ptr_count: u16,
    ) -> Result<StructBuilder> {
        let at = self.alloc(data_words as usize + ptr_count as usize)?;
        let child = StructBuilder {
            data: at,
            data_words,
            ptrs: at + data_words as usize,
            ptr_count,
        };
        self.point_struct(slot, child);
        Ok(child)
    }

    /// A primitive list of `len` elements stored in `slot`; returns its first word.
    fn list_at_slot(&mut self, slot: usize, size: ElementSize, len: u32) -> Result<usize> {
        ensure!(len < 1 << 29, "list too long");
        let words = (u64::from(len) * size.width()).div_ceil(64) as usize;
        let at = self.alloc(words)?;
        self.words[slot] =
            Self::offset(slot, at) | 1 | (size.bits() << 32) | (u64::from(len) << 35);
        Ok(at)
    }

    pub fn set_bytes(&mut self, s: StructBuilder, index: u16, bytes: &[u8]) -> Result<()> {
        let slot = Self::pointer_slot(s, index)?;
        self.bytes_at_slot(slot, bytes)
    }

    fn bytes_at_slot(&mut self, slot: usize, bytes: &[u8]) -> Result<()> {
        let at = self.list_at_slot(slot, ElementSize::Byte, bytes.len() as u32)?;
        for (i, b) in bytes.iter().enumerate() {
            self.words[at + i / 8] |= u64::from(*b) << ((i % 8) * 8);
        }
        Ok(())
    }

    pub fn set_text(&mut self, s: StructBuilder, index: u16, text: &str) -> Result<()> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        self.set_bytes(s, index, &bytes)
    }

    /// A capability pointer naming entry `cap` of the payload's cap table.
    pub fn set_capability(&mut self, s: StructBuilder, index: u16, cap: u32) -> Result<()> {
        let slot = Self::pointer_slot(s, index)?;
        self.words[slot] = 3 | (u64::from(cap) << 32);
        Ok(())
    }

    /// A primitive list in pointer `index` of `s`, from raw element bits.
    pub fn set_primitive_list(
        &mut self,
        s: StructBuilder,
        index: u16,
        size: ElementSize,
        values: &[u64],
    ) -> Result<()> {
        let slot = Self::pointer_slot(s, index)?;
        self.primitive_list_at_slot(slot, size, values)
    }

    fn primitive_list_at_slot(
        &mut self,
        slot: usize,
        size: ElementSize,
        values: &[u64],
    ) -> Result<()> {
        let at = self.list_at_slot(slot, size, values.len() as u32)?;
        let width = size.width();
        if width == 0 {
            return Ok(());
        }
        for (i, v) in values.iter().enumerate() {
            let bit = i as u64 * width;
            let masked = if width == 64 {
                *v
            } else {
                v & ((1 << width) - 1)
            };
            self.words[at + (bit / 64) as usize] |= masked << (bit % 64);
        }
        Ok(())
    }

    /// A composite list of `len` structs in pointer `index` of `s`.
    pub fn init_struct_list(
        &mut self,
        s: StructBuilder,
        index: u16,
        len: u32,
        data_words: u16,
        ptr_count: u16,
    ) -> Result<Vec<StructBuilder>> {
        let slot = Self::pointer_slot(s, index)?;
        self.struct_list_at_slot(slot, len, data_words, ptr_count)
    }

    fn struct_list_at_slot(
        &mut self,
        slot: usize,
        len: u32,
        data_words: u16,
        ptr_count: u16,
    ) -> Result<Vec<StructBuilder>> {
        let per = data_words as usize + ptr_count as usize;
        let words = per * len as usize;
        ensure!(words < 1 << 29, "list too long");
        let tag = self.alloc(1 + words)?;
        self.words[slot] = Self::offset(slot, tag) | 1 | (7 << 32) | ((words as u64) << 35);
        self.words[tag] =
            (u64::from(len) << 2) | (u64::from(data_words) << 32) | (u64::from(ptr_count) << 48);
        Ok((0..len as usize)
            .map(|i| {
                let at = tag + 1 + per * i;
                StructBuilder {
                    data: at,
                    data_words,
                    ptrs: at + data_words as usize,
                    ptr_count,
                }
            })
            .collect())
    }

    /// A pointer list of `len` null pointers in pointer `index` of `s`; returns the slots.
    pub fn init_pointer_list(
        &mut self,
        s: StructBuilder,
        index: u16,
        len: u32,
    ) -> Result<Vec<usize>> {
        let slot = Self::pointer_slot(s, index)?;
        self.pointer_list_at_slot(slot, len)
    }

    fn pointer_list_at_slot(&mut self, slot: usize, len: u32) -> Result<Vec<usize>> {
        let at = self.list_at_slot(slot, ElementSize::Pointer, len)?;
        Ok((at..at + len as usize).collect())
    }

    /// Writers addressed by raw slot, for elements of a pointer list.
    pub fn slot_struct(
        &mut self,
        slot: usize,
        data_words: u16,
        ptr_count: u16,
    ) -> Result<StructBuilder> {
        self.struct_at_slot(slot, data_words, ptr_count)
    }
    pub fn slot_bytes(&mut self, slot: usize, bytes: &[u8]) -> Result<()> {
        self.bytes_at_slot(slot, bytes)
    }
    pub fn slot_primitive_list(
        &mut self,
        slot: usize,
        size: ElementSize,
        values: &[u64],
    ) -> Result<()> {
        self.primitive_list_at_slot(slot, size, values)
    }
    pub fn slot_struct_list(
        &mut self,
        slot: usize,
        len: u32,
        data_words: u16,
        ptr_count: u16,
    ) -> Result<Vec<StructBuilder>> {
        self.struct_list_at_slot(slot, len, data_words, ptr_count)
    }
    pub fn slot_pointer_list(&mut self, slot: usize, len: u32) -> Result<Vec<usize>> {
        self.pointer_list_at_slot(slot, len)
    }
    pub fn pointer_slot_of(s: StructBuilder, index: u16) -> Result<usize> {
        Self::pointer_slot(s, index)
    }

    /// Deep-copy what `from` points at into `slot`, capabilities kept as indexes.
    pub fn copy_into(&mut self, slot: usize, from: Target<'_>) -> Result<()> {
        match from {
            Target::Null => Ok(()),
            Target::Capability(c) => {
                self.words[slot] = 3 | (u64::from(c) << 32);
                Ok(())
            }
            Target::Struct(s) => {
                let to = self.struct_at_slot(slot, s.data_words, s.ptr_count)?;
                for i in 0..s.data_words as usize {
                    self.words[to.data + i] = s.msg.segments[s.seg][s.data + i];
                }
                for i in 0..s.ptr_count {
                    self.copy_into(to.ptrs + i as usize, s.pointer(i)?)?;
                }
                Ok(())
            }
            Target::List(l) => match l.size {
                ElementSize::Composite => {
                    let items = self.struct_list_at_slot(slot, l.len, l.data_words, l.ptr_count)?;
                    for (i, to) in items.into_iter().enumerate() {
                        let s = l.struct_at(i as u32)?;
                        for w in 0..s.data_words as usize {
                            self.words[to.data + w] = s.msg.segments[s.seg][s.data + w];
                        }
                        for p in 0..s.ptr_count {
                            self.copy_into(to.ptrs + p as usize, s.pointer(p)?)?;
                        }
                    }
                    Ok(())
                }
                ElementSize::Pointer => {
                    let slots = self.pointer_list_at_slot(slot, l.len)?;
                    for (i, to) in slots.into_iter().enumerate() {
                        self.copy_into(to, l.pointer_at(i as u32)?)?;
                    }
                    Ok(())
                }
                size => {
                    let values = (0..l.len)
                        .map(|i| l.primitive(i))
                        .collect::<Result<Vec<_>>>()?;
                    self.primitive_list_at_slot(slot, size, &values)
                }
            },
        }
    }
}
