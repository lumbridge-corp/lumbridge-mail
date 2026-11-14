//! An in-process OpenAI-compatible embeddings server for tests (this crate's and the daemon's).
//! Vectors are deterministic: each of the first seven dimensions counts the words of one topic
//! (travel, money, food, code, garden, outdoors, jobs), the eighth is a small constant so no vector
//! is zero, the rest are zero; then it is normalised. So "flight to Lisbon" is nearest to mail
//! about trips, and ranking is testable. It records every request (headers, model, inputs), can be
//! told to fail, to be slow, or to be down.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOPICS: &[&[&str]] = &[
    &[
        "flight",
        "flights",
        "airport",
        "boarding",
        "travel",
        "trip",
        "lisbon",
        "hotel",
        "itinerary",
        "fly",
        "plane",
    ],
    &[
        "invoice", "receipt", "order", "payment", "paid", "total", "refund", "bill", "charge",
    ],
    &[
        "dinner",
        "lunch",
        "noodle",
        "restaurant",
        "food",
        "table",
        "breakfast",
        "eat",
    ],
    &[
        "kernel",
        "build",
        "deploy",
        "ci",
        "release",
        "bug",
        "merge",
        "scheduler",
        "schedulers",
        "code",
    ],
    &[
        "garden", "tulip", "tulips", "bulb", "bulbs", "plant", "frost",
    ],
    &["hike", "trail", "map", "route", "mountain", "walk"],
    &[
        "job",
        "role",
        "recruiter",
        "interview",
        "hiring",
        "position",
        "candidate",
        "engineer",
    ],
];

/// The fake's vector for `text` (`dim` dimensions, unit length).
pub fn vector_for(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0f32; dim.max(1)];
    for w in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        for (i, t) in TOPICS.iter().enumerate() {
            if i < v.len() && t.contains(&w) {
                v[i] += 1.0;
            }
        }
    }
    if v.len() > 7 {
        v[7] = 0.05;
    } else if let Some(last) = v.last_mut() {
        *last += 0.05;
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

/// One request the server saw.
#[derive(Debug, Clone, Default)]
pub struct SeenRequest {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub model: String,
    pub inputs: Vec<String>,
    pub body: Value,
}

impl SeenRequest {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
}

#[derive(Default)]
struct State {
    seen: Vec<SeenRequest>,
    fail: VecDeque<(u16, String)>,
    down: bool,
    delay: Duration,
    dim: usize,
    in_flight: usize,
    max_in_flight: usize,
}

#[derive(Clone)]
pub struct FakeEmbedServer {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<State>>,
    task: Arc<tokio::task::JoinHandle<()>>,
}

impl Drop for FakeEmbedServer {
    fn drop(&mut self) {
        if Arc::strong_count(&self.task) == 1 {
            self.task.abort();
        }
    }
}

impl FakeEmbedServer {
    /// Listens on 127.0.0.1, answering with `dim`-dimensional vectors.
    pub async fn start(dim: usize) -> FakeEmbedServer {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = l.local_addr().expect("addr");
        let state = Arc::new(Mutex::new(State {
            dim,
            ..State::default()
        }));
        let st = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    continue;
                };
                let st = st.clone();
                tokio::spawn(async move {
                    let Some((path, headers, body)) = read_request(&mut s).await else {
                        return;
                    };
                    let (status, answer) = answer(&st, path, headers, body).await;
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                        answer.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        FakeEmbedServer {
            addr,
            state,
            task: Arc::new(task),
        }
    }

    /// `http://127.0.0.1:<port>/v1`.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn requests(&self) -> Vec<SeenRequest> {
        self.st().seen.clone()
    }

    /// Requests answered so far.
    pub fn calls(&self) -> usize {
        self.st().seen.len()
    }

    /// Inputs embedded so far (all requests).
    pub fn inputs(&self) -> usize {
        self.st().seen.iter().map(|r| r.inputs.len()).sum()
    }

    /// The next request gets this status and body.
    pub fn fail_next(&self, status: u16, body: &str) {
        self.st().fail.push_back((status, body.into()));
    }

    /// Every request gets 503 while down.
    pub fn set_down(&self, down: bool) {
        self.st().down = down;
    }

    /// Each answer takes this long (to measure pacing and concurrency).
    pub fn set_delay(&self, d: Duration) {
        self.st().delay = d;
    }

    /// Answer with this many dimensions from now on.
    pub fn set_dim(&self, dim: usize) {
        self.st().dim = dim;
    }

    /// Most requests that were in flight at once.
    pub fn max_in_flight(&self) -> usize {
        self.st().max_in_flight
    }
}

async fn answer(
    st: &Arc<Mutex<State>>,
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
) -> (u16, String) {
    let delay = {
        let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
        s.in_flight += 1;
        s.max_in_flight = s.max_in_flight.max(s.in_flight);
        s.delay
    };
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
    s.in_flight -= 1;
    let inputs: Vec<String> = match body.get("input") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    };
    s.seen.push(SeenRequest {
        path: path.clone(),
        headers,
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        inputs: inputs.clone(),
        body: body.clone(),
    });
    if let Some((status, b)) = s.fail.pop_front() {
        return (status, b);
    }
    if s.down {
        return (503, "{\"error\": \"down\"}".into());
    }
    if !path.ends_with("/embeddings") {
        return (404, "{}".into());
    }
    let dim = s.dim;
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"object": "embedding", "index": i, "embedding": vector_for(t, dim)}))
        .collect();
    (
        200,
        json!({"object": "list", "data": data, "model": body.get("model")}).to_string(),
    )
}

async fn read_request(
    s: &mut tokio::net::TcpStream,
) -> Option<(String, Vec<(String, String)>, Value)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_end, len) = loop {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            break (i, len);
        }
    };
    while buf.len() < head_end + 4 + len {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
    let headers = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let body = serde_json::from_slice(&buf[head_end + 4..]).unwrap_or(Value::Null);
    Some((path, headers, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn vectors_follow_topics() {
        let q = vector_for("when is my flight to Lisbon", 8);
        let trip = vector_for("Your flight itinerary: boarding at the airport", 8);
        let food = vector_for("Dinner at the noodle restaurant", 8);
        assert!(cos(&q, &trip) > cos(&q, &food));
        assert!((cos(&q, &q) - 1.0).abs() < 1e-5);
        assert_eq!(vector_for("x", 2048).len(), 2048);
        assert_eq!(vector_for("x", 4).len(), 4);
    }
}
