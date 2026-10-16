//! The y-websocket binding (protocol 2, Appendix B) and craft-live's control message, as the
//! add-in speaks them. Decoding never panics.

/// A message on the room's WebSocket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Sync1(Vec<u8>),
    Sync2(Vec<u8>),
    Update(Vec<u8>),
    Awareness(Vec<u8>),
    Denied(String),
    QueryAwareness,
    Control(String),
}

const CONTROL: u64 = 100;

fn put_var(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_bytes(out: &mut Vec<u8>, d: &[u8]) {
    put_var(out, d.len() as u64);
    out.extend_from_slice(d);
}

struct Reader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn var(&mut self) -> Option<u64> {
        let mut n = 0u64;
        let mut shift = 0u32;
        loop {
            let b = *self.d.get(self.pos)?;
            self.pos += 1;
            if shift > 63 {
                return None;
            }
            n |= u64::from(b & 0x7f).checked_shl(shift)?;
            if b & 0x80 == 0 {
                return Some(n);
            }
            shift += 7;
        }
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.var()?).ok()?;
        let end = self.pos.checked_add(n)?;
        let out = self.d.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Frame::Sync1(d) => {
                o.extend_from_slice(&[0, 0]);
                put_bytes(&mut o, d);
            }
            Frame::Sync2(d) => {
                o.extend_from_slice(&[0, 1]);
                put_bytes(&mut o, d);
            }
            Frame::Update(d) => {
                o.extend_from_slice(&[0, 2]);
                put_bytes(&mut o, d);
            }
            Frame::Awareness(d) => {
                o.push(1);
                put_bytes(&mut o, d);
            }
            Frame::Denied(r) => {
                o.extend_from_slice(&[2, 0]);
                put_bytes(&mut o, r.as_bytes());
            }
            Frame::QueryAwareness => o.push(3),
            Frame::Control(j) => {
                put_var(&mut o, CONTROL);
                put_bytes(&mut o, j.as_bytes());
            }
        }
        o
    }

    pub fn decode(d: &[u8]) -> Option<Frame> {
        let mut r = Reader { d, pos: 0 };
        Some(match r.var()? {
            0 => {
                let step = r.var()?;
                let b = r.bytes()?.to_vec();
                match step {
                    0 => Frame::Sync1(b),
                    1 => Frame::Sync2(b),
                    2 => Frame::Update(b),
                    _ => return None,
                }
            }
            1 => Frame::Awareness(r.bytes()?.to_vec()),
            2 => {
                r.var()?;
                Frame::Denied(String::from_utf8_lossy(r.bytes()?).into_owned())
            }
            3 => Frame::QueryAwareness,
            CONTROL => Frame::Control(std::str::from_utf8(r.bytes()?).ok()?.to_string()),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for f in [
            Frame::Sync1(vec![1]),
            Frame::Sync2(vec![]),
            Frame::Update(vec![7; 200]),
            Frame::Awareness(vec![3]),
            Frame::Denied("x".into()),
            Frame::QueryAwareness,
            Frame::Control("{}".into()),
        ] {
            assert_eq!(Frame::decode(&f.encode()), Some(f));
        }
        assert_eq!(Frame::decode(&[0, 1, 9]), None);
        assert_eq!(Frame::decode(&[0xff; 11]), None);
        assert_eq!(Frame::decode(&[]), None);
    }
}
