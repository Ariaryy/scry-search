//! Wire format for the daemon/client IPC boundary. Deliberately not rkyv or
//! any schema-driven format — these messages are tiny (a query string in,
//! a bounded list of paths out), so a hand-rolled length-prefixed encoding
//! is simpler to reason about than pulling in a schema compiler for it.
//! (The *index* uses rkyv because that payload is huge and zero-copy matters
//! there; the protocol payload doesn't have that problem.)

pub use crate::rank::Order;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Prefix = 0,
    Substring = 1,
    Wildcard = 2,
    PathTerms = 3,
    /// Internal capability request; path terms retain discriminant 3.
    ShareIndex = 4,
    /// Internal observability request returning a UTF-8 report.
    QueryStats = 5,
    /// Bounded application and compatibility status.
    Status = 6,
}

impl QueryKind {
    fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(QueryKind::Prefix),
            1 => Some(QueryKind::Substring),
            2 => Some(QueryKind::Wildcard),
            3 => Some(QueryKind::PathTerms),
            4 => Some(QueryKind::ShareIndex),
            5 => Some(QueryKind::QueryStats),
            6 => Some(QueryKind::Status),
            _ => None,
        }
    }
}

pub struct SharedIndexResponse {
    pub handle: u64,
    pub len: u64,
    pub generation: u64,
    pub overlay: Vec<u8>,
}

const SHARED_INDEX_MAGIC: &[u8; 8] = b"SCRYSHR1";

pub fn encode_shared_index(response: &SharedIndexResponse) -> Vec<u8> {
    let mut out = Vec::with_capacity(36 + response.overlay.len());
    out.extend_from_slice(SHARED_INDEX_MAGIC);
    out.extend_from_slice(&response.handle.to_le_bytes());
    out.extend_from_slice(&response.len.to_le_bytes());
    out.extend_from_slice(&response.generation.to_le_bytes());
    out.extend_from_slice(&(response.overlay.len() as u32).to_le_bytes());
    out.extend_from_slice(&response.overlay);
    out
}

pub fn decode_shared_index(bytes: &[u8]) -> Option<SharedIndexResponse> {
    if bytes.get(..8)? != SHARED_INDEX_MAGIC {
        return None;
    }
    let mut cursor = Cursor::new(&bytes[8..]);
    let handle = cursor.read_u64()?;
    let len = cursor.read_u64()?;
    let generation = cursor.read_u64()?;
    let overlay_len = cursor.read_u32()? as usize;
    let overlay = cursor
        .buf
        .get(cursor.pos..cursor.pos.checked_add(overlay_len)?)?
        .to_vec();
    (cursor.pos + overlay_len == cursor.buf.len()).then_some(SharedIndexResponse {
        handle,
        len,
        generation,
        overlay,
    })
}

#[derive(Debug, Clone)]
pub struct Request {
    pub kind: QueryKind,
    pub pattern: String,
    pub limit: u32,
    /// How to order results. Encoded as one byte after `limit`; an unknown
    /// value is rejected rather than silently treated as relevance, so a newer
    /// client asking an older daemon for an ordering it can't honor gets an
    /// error instead of a wrong answer.
    pub order: Order,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultEntry {
    pub path: String,
    pub size: u64,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: u32,
    pub is_dir: bool,
    pub size_exact: bool,
}

pub fn encode_request(req: &Request) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10 + req.pattern.len());
    buf.push(req.kind as u8);
    buf.extend_from_slice(&req.limit.to_le_bytes());
    buf.push(req.order as u8);
    write_string(&mut buf, &req.pattern);
    buf
}

pub fn decode_request(buf: &[u8]) -> Option<Request> {
    let mut c = Cursor::new(buf);
    let kind = QueryKind::from_u8(c.read_u8()?)?;
    let limit = c.read_u32()?;
    let order = Order::from_u8(c.read_u8()?)?;
    let pattern = c.read_string()?;
    Some(Request {
        kind,
        pattern,
        limit,
        order,
    })
}

pub fn encode_results(entries: &[ResultEntry]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        write_string(&mut buf, &e.path);
        buf.extend_from_slice(&e.size.to_le_bytes());
        buf.extend_from_slice(&e.mtime.to_le_bytes());
        buf.push(e.is_dir as u8);
    }
    buf.extend_from_slice(b"SCRE");
    buf.push(1);
    for entry in entries {
        buf.push(entry.size_exact as u8);
    }
    buf
}

pub fn decode_results(buf: &[u8]) -> Option<Vec<ResultEntry>> {
    let mut c = Cursor::new(buf);
    let count = c.read_u32()?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let path = c.read_string()?;
        let size = c.read_u64()?;
        let mtime = c.read_u32()?;
        let is_dir = c.read_u8()? != 0;
        out.push(ResultEntry {
            path,
            size,
            mtime,
            is_dir,
            size_exact: !is_dir && size != 0,
        });
    }
    let remaining = &buf[c.pos..];
    if remaining.starts_with(b"SCRE") {
        let mut trailer = Cursor::new(&remaining[4..]);
        if trailer.read_u8()? != 1 {
            return None;
        }
        for entry in &mut out {
            entry.size_exact = match trailer.read_u8()? {
                0 => false,
                1 => true,
                _ => return None,
            };
        }
        if trailer.pos != remaining.len() - 4 {
            return None;
        }
    }
    Some(out)
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn read_u32(&mut self) -> Option<u32> {
        let bytes = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn read_u64(&mut self) -> Option<u64> {
        let bytes = self.buf.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    fn read_string(&mut self) -> Option<String> {
        let len = self.read_u32()? as usize;
        let bytes = self.buf.get(self.pos..self.pos + len)?;
        self.pos += len;
        String::from_utf8(bytes.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = Request {
            kind: QueryKind::Wildcard,
            pattern: "*.docx".into(),
            limit: 50,
            order: Order::Recent,
        };
        let bytes = encode_request(&req);
        let decoded = decode_request(&bytes).unwrap();
        assert_eq!(decoded.kind, QueryKind::Wildcard);
        assert_eq!(decoded.pattern, "*.docx");
        assert_eq!(decoded.limit, 50);
        assert_eq!(decoded.order, Order::Recent);
    }

    /// An ordering this build doesn't know about must fail the decode rather
    /// than fall back to relevance and return a confidently wrong list.
    #[test]
    fn an_unknown_order_is_rejected() {
        let mut bytes = encode_request(&Request {
            kind: QueryKind::Prefix,
            pattern: "a".into(),
            limit: 1,
            order: Order::Relevance,
        });
        bytes[5] = 99;
        assert!(decode_request(&bytes).is_none());
    }

    #[test]
    fn results_round_trip() {
        let entries = vec![
            ResultEntry {
                path: "C:\\a.txt".into(),
                size: 10,
                mtime: 1_700_000_000,
                is_dir: false,
                size_exact: true,
            },
            ResultEntry {
                path: "C:\\dir".into(),
                size: 0,
                mtime: 0,
                is_dir: true,
                size_exact: false,
            },
        ];
        let bytes = encode_results(&entries);
        let decoded = decode_results(&bytes).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].path, "C:\\a.txt");
        assert!(decoded[1].is_dir);
        assert!(decoded[0].size_exact);
        assert!(!decoded[1].size_exact);

        let legacy = &bytes[..bytes.len() - 7];
        let decoded = decode_results(legacy).unwrap();
        assert!(decoded[0].size_exact, "nonzero legacy file is safely exact");
        assert!(
            !decoded[1].size_exact,
            "legacy directory is not provably exact"
        );

        let mut malformed = legacy.to_vec();
        malformed.extend_from_slice(b"SCRE\x01\x01");
        assert!(decode_results(&malformed).is_none());
    }

    #[test]
    fn decode_rejects_truncated_input() {
        assert!(decode_request(&[0, 1, 2]).is_none());
        assert!(decode_results(&[0, 0]).is_none());
    }

    #[test]
    fn shared_index_response_round_trips() {
        let encoded = encode_shared_index(&SharedIndexResponse {
            handle: 17,
            len: 4096,
            generation: 9,
            overlay: vec![1, 2, 3],
        });
        let decoded = decode_shared_index(&encoded).unwrap();
        assert_eq!(decoded.handle, 17);
        assert_eq!(decoded.len, 4096);
        assert_eq!(decoded.generation, 9);
        assert_eq!(decoded.overlay, [1, 2, 3]);
        assert!(decode_shared_index(&encoded[..encoded.len() - 1]).is_none());
    }
}

/// RPC compatibility is independent of application and snapshot versions.
pub const IPC_VERSION: u32 = 1;
pub const CAP_SHARED_INDEX: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    pub version: String,
    pub ipc_version: u32,
    pub index_format: u32,
    pub capabilities: u32,
}

impl DaemonStatus {
    pub fn current(shared_index: bool) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            ipc_version: IPC_VERSION,
            index_format: crate::record::FORMAT_VERSION,
            capabilities: if shared_index { CAP_SHARED_INDEX } else { 0 },
        }
    }

    pub fn rpc_compatible(&self) -> bool {
        self.ipc_version == IPC_VERSION
    }

    pub fn shared_index_compatible(&self) -> bool {
        self.rpc_compatible()
            && self.index_format == crate::record::FORMAT_VERSION
            && self.capabilities & CAP_SHARED_INDEX != 0
    }
}

pub fn encode_status(status: &DaemonStatus) -> Vec<u8> {
    assert!(valid_version(&status.version));
    let mut bytes = Vec::with_capacity(96);
    bytes.extend_from_slice(b"SCRYSTA1");
    bytes.extend_from_slice(&status.ipc_version.to_le_bytes());
    bytes.extend_from_slice(&status.index_format.to_le_bytes());
    bytes.extend_from_slice(&status.capabilities.to_le_bytes());
    bytes.push(status.version.len() as u8);
    bytes.extend_from_slice(status.version.as_bytes());
    bytes
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
}

pub fn decode_status(bytes: &[u8]) -> Option<DaemonStatus> {
    if bytes.len() > 85 || bytes.get(..8)? != b"SCRYSTA1" {
        return None;
    }
    let ipc_version = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?);
    let index_format = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?);
    let capabilities = u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?);
    let len = *bytes.get(20)? as usize;
    let version = std::str::from_utf8(bytes.get(21..)?).ok()?;
    if version.len() != len || !valid_version(version) {
        return None;
    }
    Some(DaemonStatus {
        version: version.to_owned(),
        ipc_version,
        index_format,
        capabilities,
    })
}

#[cfg(test)]
mod status_tests {
    use super::*;
    #[test]
    fn status_is_bounded_versioned_and_exact() {
        let status = DaemonStatus::current(true);
        let bytes = encode_status(&status);
        assert_eq!(decode_status(&bytes), Some(status));
        for end in 0..bytes.len() {
            assert!(decode_status(&bytes[..end]).is_none());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_status(&trailing).is_none());
        let mut unknown = bytes;
        unknown[7] = b'2';
        assert!(decode_status(&unknown).is_none());
        assert!(decode_status(&vec![0; 1024]).is_none());
    }
    #[test]
    fn application_and_storage_versions_do_not_define_rpc() {
        let mut status = DaemonStatus::current(true);
        status.version = "99.0.0".into();
        status.index_format += 1;
        assert!(status.rpc_compatible());
        assert!(!status.shared_index_compatible());
        status.ipc_version += 1;
        assert!(!status.rpc_compatible());
    }
}
