//! A small HTTP/1.1 fake server for offline tests, built on `tokio::net::TcpListener`.
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// A running fake. `log` holds each request as `METHOD /path`.
pub(crate) struct Fake {
    pub addr: SocketAddr,
    pub log: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn requests(&self) -> Vec<String> {
        self.log.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

/// Answer each request with `route(method, path, body)`; `None` is a 404. A body starting with
/// `data:` is sent as an event stream, anything else as JSON.
pub(crate) async fn serve(
    route: impl Fn(&str, &str, &str) -> Option<(u16, String)> + Send + Sync + 'static,
) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let route = Arc::new(route);
    let requests = log.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let route = route.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match socket.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                let mut body = vec![0_u8; length];
                if socket.read_exact(&mut body).await.is_err() {
                    return;
                }
                let mut parts = head.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                requests.lock().unwrap().push(format!("{method} {path}"));
                let body = String::from_utf8_lossy(&body).into_owned();
                let (status, body) = route(&method, &path, &body).unwrap_or((404, "{}".into()));
                let kind = if body.starts_with("data:") {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Fake { addr, log }
}

/// One request as [`serve_raw`] saw it.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A full response that closes the connection.
pub(crate) fn response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
    let headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    format!(
        "HTTP/1.1 {status} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Answer each request with the raw response from `route`. `None` keeps the connection open
/// without answering, like a server that hangs.
pub(crate) async fn serve_raw(
    route: impl Fn(&Seen) -> Option<String> + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let route = Arc::new(route);
    let requests = log.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let route = route.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match socket.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let mut lines = head.lines();
                let mut parts = lines.next().unwrap_or_default().split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        Some((name.trim().to_owned(), value.trim().to_owned()))
                    })
                    .collect();
                let mut seen = Seen {
                    method,
                    path,
                    headers,
                    body: String::new(),
                };
                let length = seen
                    .header("content-length")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = vec![0_u8; length];
                if socket.read_exact(&mut body).await.is_err() {
                    return;
                }
                seen.body = String::from_utf8_lossy(&body).into_owned();
                requests.lock().unwrap().push(seen.clone());
                match route(&seen) {
                    Some(reply) => {
                        let _ = socket.write_all(reply.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    }
                    None => tokio::time::sleep(std::time::Duration::from_secs(60)).await,
                }
            });
        }
    });
    (origin, log)
}
