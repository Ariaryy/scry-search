//! Wire format for the daemon/client IPC boundary. Deliberately not rkyv or
//! any schema-driven format — these messages are tiny (a query string in,
//! a bounded list of paths out), so a hand-rolled length-prefixed encoding
//! is simpler to reason about than pulling in a schema compiler for it.
//! (The *index* uses rkyv because that payload is huge and zero-copy matters
//! there; the protocol payload doesn't have that problem.)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Prefix = 0,
    Substring = 1,
    Wildcard = 2,
}

impl QueryKind {
    fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(QueryKind::Prefix),
            1 => Some(QueryKind::Substring),
            2 => Some(QueryKind::Wildcard),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub kind: QueryKind,
    pub pattern: String,
    pub limit: u32,
}

#[derive(Debug, Clone)]
pub struct ResultEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

pub fn encode_request(req: &Request) -> Vec<u8> {
    let mut buf = Vec::with_capacity(9 + req.pattern.len());
    buf.push(req.kind as u8);
    buf.extend_from_slice(&req.limit.to_le_bytes());
    write_string(&mut buf, &req.pattern);
    buf
}

pub fn decode_request(buf: &[u8]) -> Option<Request> {
    let mut c = Cursor::new(buf);
    let kind = QueryKind::from_u8(c.read_u8()?)?;
    let limit = c.read_u32()?;
    let pattern = c.read_string()?;
    Some(Request {
        kind,
        pattern,
        limit,
    })
}

pub fn encode_results(entries: &[ResultEntry]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        write_string(&mut buf, &e.path);
        buf.extend_from_slice(&e.size.to_le_bytes());
        buf.push(e.is_dir as u8);
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
        let is_dir = c.read_u8()? != 0;
        out.push(ResultEntry { path, size, is_dir });
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
        };
        let bytes = encode_request(&req);
        let decoded = decode_request(&bytes).unwrap();
        assert_eq!(decoded.kind, QueryKind::Wildcard);
        assert_eq!(decoded.pattern, "*.docx");
        assert_eq!(decoded.limit, 50);
    }

    #[test]
    fn results_round_trip() {
        let entries = vec![
            ResultEntry {
                path: "C:\\a.txt".into(),
                size: 10,
                is_dir: false,
            },
            ResultEntry {
                path: "C:\\dir".into(),
                size: 0,
                is_dir: true,
            },
        ];
        let bytes = encode_results(&entries);
        let decoded = decode_results(&bytes).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].path, "C:\\a.txt");
        assert!(decoded[1].is_dir);
    }

    #[test]
    fn decode_rejects_truncated_input() {
        assert!(decode_request(&[0, 1, 2]).is_none());
        assert!(decode_results(&[0, 0]).is_none());
    }
}
