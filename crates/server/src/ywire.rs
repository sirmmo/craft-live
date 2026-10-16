//! The y-websocket wire format (protocol 2, Appendix B): lib0 variable-length integers and byte
//! strings, the sync and awareness messages, and craft-live's own control message.
//!
//! | First varuint | Then | Meaning |
//! |---|---|---|
//! | `0` sync | `0` + varbytes state vector | sync step 1 (`collab/sync1`) |
//! | `0` sync | `1` + varbytes update | sync step 2 (`collab/sync2`) |
//! | `0` sync | `2` + varbytes update | an update (`collab/update`) |
//! | `1` awareness | varbytes awareness update | `collab/awareness` |
//! | `2` auth | `0` + varstring reason | permission denied (y-websocket's auth message) |
//! | `3` query awareness | nothing | asks for everyone's awareness |
//! | `100` control | varstring JSON | craft-live's room events (docs/design.md, "Control messages") |
//!
//! Everything here is input from the network: decoding never panics and refuses lengths that
//! run past the end of the frame.

/// A message on a room's WebSocket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Sync1(Vec<u8>),
    Sync2(Vec<u8>),
    Update(Vec<u8>),
    Awareness(Vec<u8>),
    /// Permission denied, with the reason.
    Denied(String),
    QueryAwareness,
    /// A craft-live control message (JSON text).
    Control(String),
}

/// The control message's type number.
pub const CONTROL: u64 = 100;

/// Appends a lib0 varuint.
pub fn write_var(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends lib0 varbytes (length, then the bytes).
pub fn write_bytes(out: &mut Vec<u8>, data: &[u8]) {
    write_var(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// A reader over a frame.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    /// A varuint of at most 64 bits.
    pub fn var(&mut self) -> Option<u64> {
        let mut n: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = *self.data.get(self.pos)?;
            self.pos += 1;
            if shift >= 64 {
                return None;
            }
            let part = u64::from(b & 0x7f);
            if shift == 63 && part > 1 {
                return None;
            }
            n |= part << shift;
            if b & 0x80 == 0 {
                return Some(n);
            }
            shift += 7;
        }
    }

    /// Varbytes, borrowed.
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.var()?).ok()?;
        let end = self.pos.checked_add(len)?;
        let out = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    /// A varstring (UTF-8).
    pub fn string(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes()?).ok()
    }

    pub fn done(&self) -> bool {
        self.pos >= self.data.len()
    }
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Frame::Sync1(d) => {
                out.extend_from_slice(&[0, 0]);
                write_bytes(&mut out, d);
            }
            Frame::Sync2(d) => {
                out.extend_from_slice(&[0, 1]);
                write_bytes(&mut out, d);
            }
            Frame::Update(d) => {
                out.extend_from_slice(&[0, 2]);
                write_bytes(&mut out, d);
            }
            Frame::Awareness(d) => {
                out.push(1);
                write_bytes(&mut out, d);
            }
            Frame::Denied(reason) => {
                out.extend_from_slice(&[2, 0]);
                write_bytes(&mut out, reason.as_bytes());
            }
            Frame::QueryAwareness => out.push(3),
            Frame::Control(json) => {
                write_var(&mut out, CONTROL);
                write_bytes(&mut out, json.as_bytes());
            }
        }
        out
    }

    /// `None` for anything malformed or unknown.
    pub fn decode(data: &[u8]) -> Option<Frame> {
        let mut r = Reader::new(data);
        let frame = match r.var()? {
            0 => {
                let step = r.var()?;
                let payload = r.bytes()?.to_vec();
                match step {
                    0 => Frame::Sync1(payload),
                    1 => Frame::Sync2(payload),
                    2 => Frame::Update(payload),
                    _ => return None,
                }
            }
            1 => Frame::Awareness(r.bytes()?.to_vec()),
            2 => {
                if r.var()? != 0 {
                    return None;
                }
                Frame::Denied(r.string()?.to_string())
            }
            3 => Frame::QueryAwareness,
            CONTROL => Frame::Control(r.string()?.to_string()),
            _ => return None,
        };
        Some(frame)
    }
}

/// One entry of an awareness update: a client's state (JSON text, `null` when it left).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwarenessEntry {
    pub client: u64,
    pub clock: u64,
    pub state: String,
}

/// Most entries read from one awareness update.
pub const MAX_AWARENESS_ENTRIES: usize = 4096;

/// Parses a y-protocols awareness update.
pub fn parse_awareness(data: &[u8]) -> Option<Vec<AwarenessEntry>> {
    let mut r = Reader::new(data);
    let n = usize::try_from(r.var()?).ok()?;
    if n > MAX_AWARENESS_ENTRIES {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let client = r.var()?;
        let clock = r.var()?;
        let state = r.string()?.to_string();
        out.push(AwarenessEntry { client, clock, state });
    }
    Some(out)
}

/// Encodes an awareness update.
pub fn encode_awareness(entries: &[AwarenessEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    write_var(&mut out, entries.len() as u64);
    for e in entries {
        write_var(&mut out, e.client);
        write_var(&mut out, e.clock);
        write_bytes(&mut out, e.state.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        for f in [
            Frame::Sync1(vec![1, 2]),
            Frame::Sync2(vec![]),
            Frame::Update(vec![9; 300]),
            Frame::Awareness(vec![0]),
            Frame::Denied("no".into()),
            Frame::QueryAwareness,
            Frame::Control("{\"type\":\"hello\"}".into()),
        ] {
            assert_eq!(Frame::decode(&f.encode()), Some(f));
        }
        assert_eq!(Frame::Sync1(vec![7]).encode(), vec![0, 0, 1, 7]);
        assert_eq!(Frame::decode(&[]), None);
        assert_eq!(Frame::decode(&[0, 2, 5, 1]), None, "length past the end");
        assert_eq!(Frame::decode(&[0, 3, 0]), None);
        assert_eq!(Frame::decode(&[9]), None);
        assert_eq!(Frame::decode(&[0xff; 12]), None, "varuint over 64 bits");
    }

    #[test]
    fn varuints() {
        for n in [0u64, 1, 127, 128, 300, 1 << 53, u64::MAX] {
            let mut b = Vec::new();
            write_var(&mut b, n);
            assert_eq!(Reader::new(&b).var(), Some(n));
        }
    }

    #[test]
    fn awareness_round_trip() {
        let e = vec![AwarenessEntry { client: 1 << 40, clock: 3, state: "{\"user\":{\"name\":\"Ada\"}}".into() }];
        assert_eq!(parse_awareness(&encode_awareness(&e)), Some(e));
        assert_eq!(parse_awareness(&[2, 1, 1]), None);
        assert_eq!(parse_awareness(&[0xff, 0xff, 0x03]), None, "too many entries");
    }
}
