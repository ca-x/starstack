//! Loopback HTTP fixtures for GitHub and Ollama integration tests.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(crate) struct Request {
    pub target: String,
    pub headers: String,
    pub body: Vec<u8>,
}

pub(crate) struct Reply {
    pub status: u16,
    pub headers: &'static str,
    pub body: String,
}

impl Reply {
    pub fn json(value: serde_json::Value) -> Self {
        Self {
            status: 200,
            headers: "",
            body: value.to_string(),
        }
    }

    pub fn status(code: u16) -> Self {
        Self {
            status: code,
            headers: "",
            body: String::new(),
        }
    }
}

pub(crate) struct Server {
    pub url: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub fn new(route: impl Fn(Request) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let target = line.split_whitespace().nth(1).unwrap().to_owned();
                let mut headers = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                let size = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map_or(0, |v| v.parse().unwrap());
                let mut body = vec![0; size];
                reader.read_exact(&mut body).unwrap();
                let reply = route(Request {
                    target,
                    headers,
                    body,
                });
                write!(
                    socket,
                    "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.headers,
                    reply.body
                )
                .unwrap();
            }
        });
        Self {
            url: format!("http://{addr}"),
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}
