//! Git object and pack-file construction.
//!
//! The LLM describes a repository as ordinary structured data - a branch name, a commit
//! message and a list of `{path, content}` files - and this module turns that into real Git
//! objects: blobs, trees, a commit, and a version-2 pack file. Object IDs are *computed*
//! here, never supplied by the model, which is what makes `git clone` able to succeed: the
//! SHA advertised by `GET /info/refs` and the SHA of the commit inside the pack returned by
//! `POST /git-upload-pack` are derived from the same bytes by the same code.
//!
//! Nothing is written to disk and no repository state is kept between requests. Each request
//! rebuilds the objects from the snapshot the model (or a static/script handler) provided for
//! that request; see `CLAUDE.md` for what that implies about determinism.
//!
//! Deliberately not implemented: deltas (every object is stored whole), ofs-delta/ref-delta
//! parsing, thin packs, shallow clones, annotated tag objects, submodules and symlinks.

use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// SHA-1, implemented here because the `sha1` crate is a dev-dependency of this workspace and
/// is not linked into the binary. Git object IDs and the pack trailer are SHA-1 by definition
/// (this server does not implement the SHA-256 object format), so the algorithm is required,
/// not chosen. Verified against the standard test vectors in `tests/` and against the real
/// `git hash-object`.
#[derive(Clone)]
struct Sha1 {
    state: [u32; 5],
    buffer: [u8; 64],
    buffered: usize,
    length_bits: u64,
}

impl Sha1 {
    fn new() -> Self {
        Self {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            buffer: [0u8; 64],
            buffered: 0,
            length_bits: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length_bits = self.length_bits.wrapping_add((data.len() as u64) * 8);

        if self.buffered > 0 {
            let take = std::cmp::min(64 - self.buffered, data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }

        let mut chunks = data.chunks_exact(64);
        for chunk in &mut chunks {
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            self.compress(&block);
        }

        let rest = chunks.remainder();
        if !rest.is_empty() {
            self.buffer[..rest.len()].copy_from_slice(rest);
            self.buffered = rest.len();
        }
    }

    fn finalize(mut self) -> [u8; 20] {
        let length_bits = self.length_bits;

        // 0x80 then zeros until 56 bytes mod 64, then the 64-bit big-endian bit length.
        self.update_raw(&[0x80]);
        while self.buffered != 56 {
            self.update_raw(&[0x00]);
        }
        self.update_raw(&length_bits.to_be_bytes());

        let mut out = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// Feed padding bytes without counting them towards the message length.
    fn update_raw(&mut self, data: &[u8]) {
        for &byte in data {
            self.buffer[self.buffered] = byte;
            self.buffered += 1;
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = self.state;

        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
}

/// Convenience: SHA-1 of a single buffer.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finalize()
}

/// Object types that appear in a pack, with their pack type IDs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectKind {
    Commit = 1,
    Tree = 2,
    Blob = 3,
}

impl ObjectKind {
    fn header_name(self) -> &'static str {
        match self {
            ObjectKind::Commit => "commit",
            ObjectKind::Tree => "tree",
            ObjectKind::Blob => "blob",
        }
    }
}

/// A loose Git object: its type and its uncompressed body.
#[derive(Clone, Debug)]
pub struct GitObject {
    pub kind: ObjectKind,
    pub body: Vec<u8>,
}

impl GitObject {
    /// The object's SHA-1: `sha1("<type> <len>\0" + body)`.
    pub fn id(&self) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(format!("{} {}\0", self.kind.header_name(), self.body.len()).as_bytes());
        hasher.update(&self.body);
        hasher.finalize()
    }
}

/// One file in the repository snapshot.
#[derive(Clone, Debug)]
pub struct RepoFile {
    pub path: String,
    pub content: Vec<u8>,
    pub executable: bool,
}

/// Author/committer identity and commit timestamp.
#[derive(Clone, Debug)]
pub struct CommitMeta {
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    /// Seconds since the Unix epoch, written with a `+0000` offset.
    ///
    /// Defaulted by the caller to a *fixed* value rather than "now": the commit timestamp is
    /// part of the commit object, so a wall-clock default would give the two HTTP requests of
    /// a single clone two different commit SHAs.
    pub timestamp: i64,
}

/// A repository snapshot compiled into Git objects.
#[derive(Clone, Debug)]
pub struct BuiltRepo {
    pub branch: String,
    pub commit_id: [u8; 20],
    /// Every object needed to reconstruct the commit: the commit, all trees, all blobs.
    pub objects: Vec<GitObject>,
}

impl BuiltRepo {
    pub fn commit_hex(&self) -> String {
        hex_id(&self.commit_id)
    }
}

/// Render a 20-byte object ID as lowercase hex.
pub fn hex_id(id: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for b in id {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Build the objects for a single-commit repository containing `files`.
///
/// Returns an error for a path Git itself would refuse, so a model that invents
/// `../../etc/passwd` gets a clear failure instead of a pack no client will accept.
pub fn build_repo(branch: &str, files: &[RepoFile], meta: &CommitMeta) -> Result<BuiltRepo> {
    if branch.is_empty() || branch.contains(|c: char| c.is_whitespace() || c.is_control()) {
        bail!("Invalid branch name {branch:?}: must be non-empty and contain no whitespace");
    }

    let mut objects: Vec<GitObject> = Vec::new();
    let mut root = TreeNode::default();

    for file in files {
        let components = validate_path(&file.path)?;
        let blob = GitObject {
            kind: ObjectKind::Blob,
            body: file.content.clone(),
        };
        let blob_id = blob.id();
        objects.push(blob);

        let mode = if file.executable { "100755" } else { "100644" };
        root.insert(&components, blob_id, mode)?;
    }

    let tree_id = root.write(&mut objects);

    let commit_body = build_commit(&tree_id, meta);
    let commit = GitObject {
        kind: ObjectKind::Commit,
        body: commit_body,
    };
    let commit_id = commit.id();
    objects.push(commit);

    Ok(BuiltRepo {
        branch: branch.to_string(),
        commit_id,
        objects,
    })
}

/// Split and validate a repository-relative path.
fn validate_path(path: &str) -> Result<Vec<String>> {
    if path.is_empty() {
        bail!("File path is empty");
    }
    if path.starts_with('/') {
        bail!("File path {path:?} is absolute; paths are relative to the repository root");
    }
    if path.contains('\0') {
        bail!("File path {path:?} contains a NUL byte");
    }

    let components: Vec<String> = path.split('/').map(|s| s.to_string()).collect();
    for component in &components {
        match component.as_str() {
            "" => {
                bail!("File path {path:?} has an empty component (leading, trailing or double '/')")
            }
            "." | ".." => bail!("File path {path:?} contains a '.' or '..' component"),
            ".git" => bail!(
                "File path {path:?} contains a '.git' component, which Git refuses to check out"
            ),
            _ => {}
        }
    }
    Ok(components)
}

/// In-memory tree under construction.
#[derive(Default)]
struct TreeNode {
    /// name -> (mode, blob id)
    files: BTreeMap<String, (&'static str, [u8; 20])>,
    /// name -> subtree
    dirs: BTreeMap<String, TreeNode>,
}

impl TreeNode {
    fn insert(
        &mut self,
        components: &[String],
        blob_id: [u8; 20],
        mode: &'static str,
    ) -> Result<()> {
        let (name, rest) = components
            .split_first()
            .expect("validate_path guarantees at least one component");

        if rest.is_empty() {
            if self.dirs.contains_key(name) {
                bail!("Path collision: {name:?} is used both as a directory and as a file");
            }
            self.files.insert(name.clone(), (mode, blob_id));
        } else {
            if self.files.contains_key(name) {
                bail!("Path collision: {name:?} is used both as a file and as a directory");
            }
            self.dirs
                .entry(name.clone())
                .or_default()
                .insert(rest, blob_id, mode)?;
        }
        Ok(())
    }

    /// Serialise this tree (and its subtrees) into `objects`, returning this tree's ID.
    fn write(&self, objects: &mut Vec<GitObject>) -> [u8; 20] {
        // Git sorts tree entries by name, with directory names compared as if they ended in
        // '/'. Getting this wrong produces a tree whose SHA no other Git implementation
        // reproduces, so it is done explicitly rather than relying on BTreeMap order.
        let mut entries: Vec<(String, &'static str, [u8; 20])> = Vec::new();

        for (name, (mode, id)) in &self.files {
            entries.push((name.clone(), mode, *id));
        }
        for (name, node) in &self.dirs {
            let id = node.write(objects);
            entries.push((format!("{name}/"), "40000", id));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut body = Vec::new();
        for (sort_name, mode, id) in entries {
            let name = sort_name.strip_suffix('/').unwrap_or(&sort_name);
            body.extend_from_slice(mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(&id);
        }

        let tree = GitObject {
            kind: ObjectKind::Tree,
            body,
        };
        let id = tree.id();
        objects.push(tree);
        id
    }
}

fn build_commit(tree_id: &[u8; 20], meta: &CommitMeta) -> Vec<u8> {
    // Identities may not contain '<', '>' or newlines; sanitise rather than emit a commit no
    // Git client can parse.
    let name = sanitize_identity(&meta.author_name, "NetGet");
    let email = sanitize_identity(&meta.author_email, "netget@localhost");
    let when = format!("{} +0000", meta.timestamp);

    let mut message = meta.message.replace('\r', "");
    if message.is_empty() {
        message.push_str("Initial commit");
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }

    let mut body = String::new();
    body.push_str(&format!("tree {}\n", hex_id(tree_id)));
    body.push_str(&format!("author {name} <{email}> {when}\n"));
    body.push_str(&format!("committer {name} <{email}> {when}\n"));
    body.push('\n');
    body.push_str(&message);
    body.into_bytes()
}

fn sanitize_identity(value: &str, fallback: &str) -> String {
    // `<` and `>` delimit the email in `author Name <email> <when>`, so they are removed rather
    // than substituted: a space where one stood would still shift the header's fields. Every
    // control character becomes a space, which is `utils::sanitize`'s line-field rule — the
    // header is one line, and deleting a newline would join two names into one.
    let cleaned: String = crate::utils::sanitize::line_field(value)
        .chars()
        .filter(|c| !matches!(c, '<' | '>'))
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

/// Serialise objects into a version-2 pack file.
///
/// Layout: `"PACK"`, version 2, object count, then for each object a type/size header
/// followed by its zlib-compressed body, then a SHA-1 trailer over everything preceding it.
pub fn write_pack(objects: &[GitObject]) -> Vec<u8> {
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());

    for object in objects {
        write_object_header(&mut pack, object.kind, object.body.len());
        pack.extend_from_slice(&zlib_stored(&object.body));
    }

    let checksum = sha1(&pack);
    pack.extend_from_slice(&checksum);
    pack
}

/// Pack object header: 3 type bits and a variable-length size, 7 bits per continuation byte.
fn write_object_header(out: &mut Vec<u8>, kind: ObjectKind, size: usize) {
    let mut byte = ((kind as u8) << 4) | ((size & 0x0f) as u8);
    let mut remaining = size >> 4;
    while remaining > 0 {
        out.push(byte | 0x80);
        byte = (remaining & 0x7f) as u8;
        remaining >>= 7;
    }
    out.push(byte);
}

/// Wrap `data` in a zlib stream using only uncompressed ("stored") deflate blocks.
///
/// Git requires object bodies to be zlib streams but does not require them to be *compressed*;
/// a stored block is a valid deflate block, so this produces a stream every zlib decoder
/// accepts. Doing it by hand keeps the Git server free of a compression dependency, which
/// matters because `flate2` is optional in this workspace and is not enabled by the `git`
/// feature. The cost is that packs are slightly larger than the input.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    // CMF = 0x78 (deflate, 32K window), FLG = 0x01 -> 0x7801 is divisible by 31, no preset dict.
    out.push(0x78);
    out.push(0x01);

    if data.is_empty() {
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(!0u16).to_le_bytes());
    } else {
        let mut chunks = data.chunks(u16::MAX as usize).peekable();
        while let Some(chunk) = chunks.next() {
            let final_block = chunks.peek().is_none();
            out.push(if final_block { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}
