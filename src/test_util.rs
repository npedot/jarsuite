//! Helpers shared by tests.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

/// Minimal HTTP server: answers 200 for known paths, 404 for the rest.
/// The key is the request path, query string included. In text bodies
/// `{BASE}` becomes the server URL (the port is known only after bind).
pub struct TestServer {
    pub url: String,
}

pub fn serve(files: &[(&str, &[u8])]) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let files: HashMap<String, Vec<u8>> = files
        .iter()
        .map(|(p, b)| {
            let body = match std::str::from_utf8(b) {
                Ok(text) if text.contains("{BASE}") => text.replace("{BASE}", &url).into_bytes(),
                _ => b.to_vec(),
            };
            (p.to_string(), body)
        })
        .collect();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut header = String::new();
            while reader.read_line(&mut header).unwrap() > 2 {
                header.clear();
            }
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let (status, body) = match files.get(path) {
                Some(b) => ("200 OK", b.as_slice()),
                None => ("404 Not Found", &b""[..]),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
        }
    });
    TestServer { url }
}
