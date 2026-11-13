use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use zeroize::Zeroize as _;

/// Largest frame accepted either way.
pub const MAX_FRAME: usize = 64 * 1024;

/// First frame of a version 1 request (token, prompt follow).
pub(crate) const MAGIC_V1: &[u8] = b"LECTOR-ASKPASS/1";
/// First frame of a version 2 request (token, operation id, prompt follow).
pub(crate) const MAGIC: &[u8] = b"LECTOR-ASKPASS/2";

/// Reply tags.
pub(crate) const TAG_ANSWER: u8 = b'A';
pub(crate) const TAG_CANCEL: u8 = b'C';

/// Where an [`AskpassServer`](super::AskpassServer) listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix domain socket (Unix).
    Unix(PathBuf),
    /// A loopback TCP port (Windows; available everywhere for tests).
    Tcp(SocketAddr),
}

impl Endpoint {
    /// `unix:/path` or `tcp:127.0.0.1:port` (the value of `LECTOR_ASKPASS_ENDPOINT`).
    pub fn to_env_value(&self) -> String {
        match self {
            Endpoint::Unix(p) => format!("unix:{}", p.display()),
            Endpoint::Tcp(a) => format!("tcp:{a}"),
        }
    }

    /// Parses [`to_env_value`](Self::to_env_value). TCP endpoints must be loopback.
    pub fn parse(value: &str) -> Option<Endpoint> {
        if let Some(p) = value.strip_prefix("unix:") {
            return (!p.is_empty()).then(|| Endpoint::Unix(PathBuf::from(p)));
        }
        let addr: SocketAddr = value.strip_prefix("tcp:")?.parse().ok()?;
        addr.ip().is_loopback().then_some(Endpoint::Tcp(addr))
    }

    /// Connects (client side).
    pub(crate) fn connect(&self, timeout: Duration) -> io::Result<Box<dyn Conn>> {
        match self {
            Endpoint::Tcp(addr) => {
                let s = TcpStream::connect_timeout(addr, timeout)?;
                s.set_nodelay(true)?;
                Ok(Box::new(s))
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => Ok(Box::new(std::os::unix::net::UnixStream::connect(path)?)),
            #[cfg(not(unix))]
            Endpoint::Unix(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix domain sockets are not supported here",
            )),
        }
    }
}

/// A bidirectional byte stream with timeouts.
pub(crate) trait Conn: Read + Write + Send {
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()>;
    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()>;
}

impl Conn for TcpStream {
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, t)
    }
}

#[cfg(unix)]
impl Conn for std::os::unix::net::UnixStream {
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, t)
    }
    fn set_write_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_write_timeout(self, t)
    }
}

/// Writes one frame.
pub(crate) fn write_frame(w: &mut dyn Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|n| (*n as usize) <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "askpass frame too large"))?;
    // One buffer, one write: the frame is never split across partial writes by us.
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    let res = w.write_all(&buf).and_then(|()| w.flush());
    buf.zeroize();
    res
}

/// Reads one frame (at most [`MAX_FRAME`] bytes).
pub(crate) fn read_frame(r: &mut dyn Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "askpass frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    if let Err(err) = r.read_exact(&mut buf) {
        buf.zeroize();
        return Err(err);
    }
    Ok(buf)
}

/// Reads one frame (at most `max` bytes) that must arrive completely before `deadline`:
/// the read timeout is re-armed with the time left before every read, so a peer trickling
/// bytes cannot hold the connection past the deadline.
pub(crate) fn read_frame_until(
    conn: &mut dyn Conn,
    max: usize,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    read_exact_until(conn, &mut len, deadline)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max.min(MAX_FRAME) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "askpass frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    if let Err(err) = read_exact_until(conn, &mut buf, deadline) {
        buf.zeroize();
        return Err(err);
    }
    Ok(buf)
}

fn read_exact_until(conn: &mut dyn Conn, mut buf: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !buf.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "askpass request too slow",
            ));
        }
        // A zero timeout means "block forever" to the OS; `left` is never zero here.
        conn.set_read_timeout(Some(left))?;
        match conn.read(buf) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Constant-time comparison (the token check must not leak how many bytes matched).
pub(crate) fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_are_bounded() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        write_frame(&mut buf, b"").unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), b"hello");
        assert_eq!(read_frame(&mut r).unwrap(), b"");
        assert!(read_frame(&mut r).is_err());
        let big = vec![0u8; MAX_FRAME + 1];
        assert!(write_frame(&mut Vec::new(), &big).is_err());
        let mut evil = ((MAX_FRAME as u32) + 1).to_be_bytes().to_vec();
        evil.extend_from_slice(b"xx");
        assert!(read_frame(&mut &evil[..]).is_err());
    }

    #[test]
    fn endpoints_parse_and_reject_non_loopback() {
        let e = Endpoint::Tcp("127.0.0.1:4242".parse().unwrap());
        assert_eq!(Endpoint::parse(&e.to_env_value()), Some(e));
        let u = Endpoint::Unix(PathBuf::from("/tmp/x/s"));
        assert_eq!(Endpoint::parse(&u.to_env_value()), Some(u));
        assert_eq!(Endpoint::parse("tcp:10.0.0.1:22"), None);
        assert_eq!(Endpoint::parse("unix:"), None);
        assert_eq!(Endpoint::parse("garbage"), None);
    }

    #[test]
    fn constant_time_compare() {
        assert!(same_bytes(b"abc", b"abc"));
        assert!(!same_bytes(b"abc", b"abd"));
        assert!(!same_bytes(b"abc", b"ab"));
    }
}
