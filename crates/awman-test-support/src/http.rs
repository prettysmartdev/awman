//! A minimal HTTP/1.1 server and client over std sockets, used by the
//! in-process fakes and the conformance checks. Not a general HTTP stack:
//! Content-Length and chunked request bodies, one request per read loop,
//! responses always carry Content-Length.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    /// Lowercased names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn query_param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| percent_decode(v))
        })
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response {
            status,
            ..Response::default()
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Response {
        self.body = body.into();
        self
    }

    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// A server on an owned listener; stops (and closes the listener) on drop.
pub(crate) struct HttpServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HttpServer {
    pub fn start(listener: TcpListener, handler: Handler) -> HttpServer {
        let addr = listener.local_addr().expect("listener address");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let handler = handler.clone();
                std::thread::spawn(move || serve_connection(stream, handler));
            }
        });
        HttpServer {
            addr,
            stop,
            thread: Some(thread),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_connection(stream: TcpStream, handler: Handler) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    loop {
        let request = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            _ => return,
        };
        let close = request
            .header("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        let head = request.method == "HEAD";
        let response = handler(request);
        if write_response(&mut writer, &response, head).is_err() || close {
            return;
        }
    }
}

fn read_head(reader: &mut impl BufRead) -> std::io::Result<Option<Vec<String>>> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            if lines.is_empty() {
                continue;
            }
            return Ok(Some(lines));
        }
        lines.push(line);
    }
}

fn parse_headers(lines: &[String]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect()
}

fn read_body(reader: &mut impl BufRead, headers: &[(String, String)]) -> std::io::Result<Vec<u8>> {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    if get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        let mut body = Vec::new();
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| std::io::Error::other("bad chunk size"))?;
            if size == 0 {
                let mut trailer = String::new();
                while reader.read_line(&mut trailer)? > 2 {
                    trailer.clear();
                }
                return Ok(body);
            }
            let mut chunk = vec![0; size];
            reader.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            let mut crlf = String::new();
            reader.read_line(&mut crlf)?;
        }
    }
    let length: usize = get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

pub(crate) fn read_request(reader: &mut impl BufRead) -> std::io::Result<Option<Request>> {
    let Some(lines) = read_head(reader)? else {
        return Ok(None);
    };
    let mut parts = lines[0].split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let headers = parse_headers(&lines[1..]);
    let body = read_body(reader, &headers)?;
    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Status",
    }
}

fn write_response(writer: &mut impl Write, response: &Response, head: bool) -> std::io::Result<()> {
    let mut out = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason(response.status)
    );
    for (k, v) in &response.headers {
        if !k.eq_ignore_ascii_case("content-length") {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    let length = response
        .get_header("content-length")
        .map(str::to_string)
        .unwrap_or_else(|| response.body.len().to_string());
    out.push_str(&format!("Content-Length: {length}\r\n\r\n"));
    writer.write_all(out.as_bytes())?;
    if !head {
        writer.write_all(&response.body)?;
    }
    writer.flush()
}

/// One request with `Connection: close`.
pub(crate) fn request(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut out = format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(out.as_bytes())?;
    stream.write_all(body)?;
    let mut reader = BufReader::new(stream);
    let lines = read_head(&mut reader)?.ok_or_else(|| std::io::Error::other("empty response"))?;
    let status: u16 = lines[0]
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("bad status line"))?;
    let headers = parse_headers(&lines[1..]);
    let body = if method == "HEAD" {
        Vec::new()
    } else {
        let mut body = Vec::new();
        reader.read_to_end(&mut body)?;
        body
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_and_chunked_bodies() {
        let raw = b"POST /a?x=1%2F2 HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&raw[..]);
        let first = read_request(&mut reader).unwrap().unwrap();
        assert_eq!(first.path, "/a");
        assert_eq!(first.query_param("x").as_deref(), Some("1/2"));
        assert_eq!(first.body, b"abc");
        let second = read_request(&mut reader).unwrap().unwrap();
        assert_eq!(second.body, b"hi");
        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("sha256%3Aab"), "sha256:ab");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
