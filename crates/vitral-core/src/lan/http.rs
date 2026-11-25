//! One HTTP/1 request per connection. The receiver owns every socket until its worker exits.
use super::POLL_INTERVAL;
use crate::offline_guard::OfflineGuard;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

const MAX_HEADER_BYTES: usize = 16 * 1024;

struct Connection<'a> {
    stream: TcpStream,
    stop: &'a AtomicBool,
    guard: &'a OfflineGuard,
    idle_timeout: Duration,
    header_deadline: Option<Instant>,
}

impl Connection<'_> {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.guard.is_pure_offline()
    }

    fn send(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let mut deadline = Instant::now() + self.idle_timeout;
        while !bytes.is_empty() {
            if self.stopped() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            if Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.stream.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    bytes = &bytes[n..];
                    deadline = Instant::now() + self.idle_timeout;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Read for Connection<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let idle_deadline = Instant::now() + self.idle_timeout;
        let deadline = self
            .header_deadline
            .map_or(idle_deadline, |d| d.min(idle_deadline));
        while !self.stopped() {
            if Instant::now() >= deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.stream.read(buf) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                result => return result,
            }
        }
        Err(io::ErrorKind::ConnectionAborted.into())
    }
}

pub(super) struct Request<'a> {
    pub method: String,
    pub url: String,
    reader: BufReader<Connection<'a>>,
    length: Option<usize>,
    chunked: bool,
    expect_continue: bool,
}

fn line(reader: &mut impl BufRead, limit: usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader.take(limit as u64).read_until(b'\n', &mut bytes)?;
    let line = bytes
        .strip_suffix(b"\r\n")
        .ok_or(io::ErrorKind::InvalidData)?;
    String::from_utf8(line.to_vec()).map_err(|_| io::ErrorKind::InvalidData.into())
}

impl<'a> Request<'a> {
    pub fn accept(
        stream: TcpStream,
        stop: &'a AtomicBool,
        guard: &'a OfflineGuard,
        idle_timeout: Duration,
    ) -> io::Result<Self> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(POLL_INTERVAL))?;
        stream.set_write_timeout(Some(POLL_INTERVAL))?;
        let header_deadline = Instant::now() + idle_timeout;
        let mut reader = BufReader::new(Connection {
            stream,
            stop,
            guard,
            idle_timeout,
            header_deadline: Some(header_deadline),
        });
        let start = line(&mut reader, MAX_HEADER_BYTES)?;
        let mut fields = start.split(' ');
        let (Some(method), Some(url), Some("HTTP/1.1" | "HTTP/1.0"), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(io::ErrorKind::InvalidData.into());
        };
        let mut remaining = MAX_HEADER_BYTES - start.len() - 2;
        let mut length = None;
        let mut chunked = false;
        let mut expect_continue = false;
        loop {
            let header = line(&mut reader, remaining)?;
            remaining -= header.len() + 2;
            if header.is_empty() {
                break;
            }
            let (key, value) = header.split_once(':').ok_or(io::ErrorKind::InvalidData)?;
            let value = value.trim();
            if key.eq_ignore_ascii_case("content-length") {
                if length.is_some() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                length = Some(value.parse().map_err(|_| io::ErrorKind::InvalidData)?);
            } else if key.eq_ignore_ascii_case("transfer-encoding") {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                chunked = true;
            } else if key.eq_ignore_ascii_case("expect") {
                expect_continue = value.eq_ignore_ascii_case("100-continue");
            }
        }
        if Instant::now() >= header_deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        reader.get_mut().header_deadline = None;
        if chunked && length.is_some() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Self {
            method: method.into(),
            url: url.into(),
            reader,
            length,
            chunked,
            expect_continue,
        })
    }

    pub fn body(&mut self, limit: usize) -> Result<Vec<u8>, u16> {
        if self.length.is_some_and(|n| n > limit) {
            return Err(413);
        }
        if self.expect_continue {
            self.reader
                .get_mut()
                .send(b"HTTP/1.1 100 Continue\r\n\r\n")
                .map_err(|_| 400u16)?;
        }
        let mut body = Vec::new();
        loop {
            let size = if self.chunked {
                let header = line(&mut self.reader, MAX_HEADER_BYTES).map_err(|_| 400u16)?;
                let size = header.split(';').next().unwrap_or("");
                usize::from_str_radix(size, 16).map_err(|_| 400u16)?
            } else {
                self.length.unwrap_or(0)
            };
            if size > limit - body.len() {
                return Err(413);
            }
            let count = self
                .reader
                .by_ref()
                .take(size as u64)
                .read_to_end(&mut body)
                .map_err(|_| 400u16)?;
            if count != size {
                return Err(400);
            }
            if !self.chunked {
                return Ok(body);
            }
            if size == 0 {
                let mut remaining = MAX_HEADER_BYTES;
                loop {
                    let trailer = line(&mut self.reader, remaining).map_err(|_| 400u16)?;
                    if trailer.is_empty() {
                        return Ok(body);
                    }
                    remaining -= trailer.len() + 2;
                }
            }
            if !line(&mut self.reader, 2).map_err(|_| 400u16)?.is_empty() {
                return Err(400);
            }
        }
    }

    pub fn stopped(&self) -> bool {
        self.reader.get_ref().stopped()
    }

    pub fn respond(&mut self, status: u16, mime: &str, body: &str) -> io::Result<()> {
        let reason = tiny_http::StatusCode(status).default_reason_phrase();
        let reply = format!(
            "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        self.reader.get_mut().send(reply.as_bytes())
    }

    pub fn empty(&mut self, status: u16) -> io::Result<()> {
        self.respond(status, "text/plain", "")
    }
    pub fn text(&mut self, status: u16, body: &str) -> io::Result<()> {
        self.respond(status, "text/plain", body)
    }
    pub fn json(&mut self, status: u16, body: &str) -> io::Result<()> {
        self.respond(status, "application/json", body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn slow_header_cannot_extend_the_total_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let stream = listener.accept().unwrap().0;
        let stop = AtomicBool::new(false);
        let guard = OfflineGuard::new(false);
        std::thread::scope(|scope| {
            let sender = scope.spawn(move || {
                for byte in b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n" {
                    if writer.write_all(&[*byte]).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let start = Instant::now();
            let error = Request::accept(stream, &stop, &guard, Duration::from_millis(80))
                .err()
                .expect("slow header was accepted");
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(start.elapsed() < Duration::from_millis(500));
            sender.join().unwrap();
        });
    }

    #[test]
    fn body_progress_resets_idle_timeout_but_stalled_reads_and_writes_expire() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let stream = listener.accept().unwrap().0;
        let stop = AtomicBool::new(false);
        let guard = OfflineGuard::new(false);
        std::thread::scope(|scope| {
            let sender = scope.spawn(move || {
                writer
                    .write_all(b"POST / HTTP/1.1\r\nContent-Length: 8\r\n\r\n")
                    .unwrap();
                for _ in 0..8 {
                    std::thread::sleep(Duration::from_millis(30));
                    writer.write_all(b"x").unwrap();
                }
                writer
            });
            let mut request =
                Request::accept(stream, &stop, &guard, Duration::from_millis(100)).unwrap();
            let start = Instant::now();
            assert_eq!(request.body(8).unwrap(), b"xxxxxxxx");
            assert!(start.elapsed() > Duration::from_millis(100));
            // Keep the peer open without consuming the response or sending more data.
            let _writer = sender.join().unwrap();
            let connection = request.reader.get_mut();
            let start = Instant::now();
            assert_eq!(
                connection.read(&mut [0]).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            assert!(start.elapsed() < Duration::from_millis(500));
            let start = Instant::now();
            assert_eq!(
                connection
                    .send(&vec![0; 8 * 1024 * 1024])
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::TimedOut
            );
            assert!(start.elapsed() < Duration::from_secs(1));
        });
    }

    #[test]
    fn pending_body_read_observes_stop_and_offline() {
        for offline in [false, true] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let stream = listener.accept().unwrap().0;
            stream.set_read_timeout(Some(POLL_INTERVAL)).unwrap();
            writer.write_all(b"first part").unwrap();
            let stop = AtomicBool::new(false);
            let guard = OfflineGuard::new(false);
            std::thread::scope(|scope| {
                let (ready, reading) = mpsc::channel();
                let (stop_ref, guard_ref) = (&stop, &guard);
                let reader = scope.spawn(move || {
                    let _online = guard_ref.enter_online("receiver_body_test").unwrap();
                    let mut reader = BufReader::new(Connection {
                        stream,
                        stop: stop_ref,
                        guard: guard_ref,
                        idle_timeout: IDLE_TIMEOUT,
                        header_deadline: None,
                    });
                    assert!(!reader.fill_buf().unwrap().is_empty());
                    let mut request = Request {
                        method: String::new(),
                        url: String::new(),
                        reader,
                        length: Some(10_000),
                        chunked: false,
                        expect_continue: false,
                    };
                    ready.send(()).unwrap();
                    request.body(10_000)
                });
                reading.recv_timeout(Duration::from_secs(1)).unwrap();
                let start = Instant::now();
                if offline {
                    guard.set_pure_offline(true);
                } else {
                    stop.store(true, Ordering::Release);
                }
                assert_eq!(reader.join().unwrap(), Err(400));
                assert!(start.elapsed() < Duration::from_millis(500));
            });
        }
    }
}
