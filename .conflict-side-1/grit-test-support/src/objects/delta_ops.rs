//! Hand-built Git pack delta instruction streams.

/// Builder for binary delta bodies (the zlib-compressed payload of delta objects).
///
/// The stream begins with base and result size varints, followed by copy/insert
/// (or arbitrary raw) opcodes understood by Git's delta decoder.
#[derive(Debug, Default, Clone)]
pub struct DeltaOps {
    bytes: Vec<u8>,
    sizes_written: bool,
}

impl DeltaOps {
    /// Create an empty delta; call [`Self::header`] before opcodes unless using [`Self::push_raw`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Write the base and target size varints that prefix every delta body.
    ///
    /// `base_size` and `target_size` are the uncompressed sizes Git expects in the
    /// delta header.
    pub fn header(&mut self, base_size: usize, target_size: usize) -> &mut Self {
        write_varint(&mut self.bytes, base_size);
        write_varint(&mut self.bytes, target_size);
        self.sizes_written = true;
        self
    }

    /// Append a `COPY` opcode copying `size` bytes from `offset` in the base object.
    ///
    /// Large copies are split into 64 KiB chunks, matching Git's encoder limits.
    pub fn copy(&mut self, mut offset: usize, mut size: usize) -> &mut Self {
        while size > 0 {
            let chunk = size.min(0x10000);
            push_copy_opcode(&mut self.bytes, offset, chunk);
            offset = offset.saturating_add(chunk);
            size -= chunk;
        }
        self
    }

    /// Append an `INSERT` opcode with literal `data`.
    pub fn insert(&mut self, data: &[u8]) -> &mut Self {
        let mut rest = data;
        while !rest.is_empty() {
            let n = rest.len().min(127);
            self.bytes.push(n as u8);
            self.bytes.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
        }
        self
    }

    /// Append raw bytes (for malformed or adversarial instruction streams).
    pub fn push_raw(&mut self, raw: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(raw);
        self
    }

    /// Finish building and return the delta instruction bytes.
    ///
    /// When no explicit [`Self::header`] was used, base/target sizes of zero are
    /// prepended so the stream is syntactically well-formed.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        if !self.sizes_written {
            let mut prefixed = Vec::new();
            write_varint(&mut prefixed, 0);
            write_varint(&mut prefixed, 0);
            prefixed.extend(self.bytes);
            return prefixed;
        }
        self.bytes
    }
}

fn write_varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

fn push_copy_opcode(out: &mut Vec<u8>, offset: usize, size: usize) {
    let mut op = 0x80u8;
    if offset & 0x0000_00ff != 0 {
        op |= 0x01;
    }
    if offset & 0x0000_ff00 != 0 {
        op |= 0x02;
    }
    if offset & 0x00ff_0000 != 0 {
        op |= 0x04;
    }
    if offset & 0xff00_0000 != 0 {
        op |= 0x08;
    }
    if size != 0x10000 {
        if size & 0x00ff != 0 {
            op |= 0x10;
        }
        if size & 0xff00 != 0 {
            op |= 0x20;
        }
    }
    out.push(op);
    if op & 0x01 != 0 {
        out.push(offset as u8);
    }
    if op & 0x02 != 0 {
        out.push((offset >> 8) as u8);
    }
    if op & 0x04 != 0 {
        out.push((offset >> 16) as u8);
    }
    if op & 0x08 != 0 {
        out.push((offset >> 24) as u8);
    }
    if size != 0x10000 {
        if op & 0x10 != 0 {
            out.push(size as u8);
        }
        if op & 0x20 != 0 {
            out.push((size >> 8) as u8);
        }
    }
}
