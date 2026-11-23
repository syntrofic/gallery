use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::{AppError, AppResult};

pub const NORMAL_QUEUE: usize = 16;
pub const CONTROL_QUEUE: usize = 4;
pub const STDERR_BUDGET: usize = 4096;
pub const STDERR_INTERVAL: Duration = Duration::from_secs(1);

struct Outbound {
    bytes: Vec<u8>,
    done: oneshot::Sender<()>,
}

#[derive(Clone)]
pub struct StdinGate {
    normal: mpsc::Sender<Outbound>,
    control: mpsc::Sender<Outbound>,
}

impl StdinGate {
    pub fn spawn<W>(mut writer: W) -> Self
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (normal, mut normal_rx) = mpsc::channel::<Outbound>(NORMAL_QUEUE);
        let (control, mut control_rx) = mpsc::channel::<Outbound>(CONTROL_QUEUE);
        tokio::spawn(async move {
            loop {
                let next = tokio::select! {
                    biased;
                    msg = control_rx.recv() => msg,
                    msg = normal_rx.recv() => msg,
                };
                let Some(msg) = next else {
                    break;
                };
                if writer.write_all(&msg.bytes).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
                let _ = msg.done.send(());
            }
        });
        Self { normal, control }
    }

    pub async fn send_normal(&self, bytes: Vec<u8>) -> AppResult<()> {
        self.enqueue(&self.normal, bytes, false).await
    }

    pub async fn send_control(&self, bytes: Vec<u8>) -> AppResult<()> {
        self.enqueue(&self.control, bytes, true).await
    }

    async fn enqueue(&self, tx: &mpsc::Sender<Outbound>, bytes: Vec<u8>, control: bool) -> AppResult<()> {
        let (done, ack) = oneshot::channel();
        let outbound = Outbound { bytes, done };
        let queued = if control {
            tx.try_send(outbound).map_err(|_| ()).map(|_| ())
        } else {
            tx.send(outbound).await.map_err(|_| ())
        };
        if queued.is_err() {
            return Err(closed());
        }
        ack.await.map_err(|_| closed())
    }
}

fn closed() -> AppError {
    AppError::new(
        axum::http::StatusCode::BAD_GATEWAY,
        "UPSTREAM_CLOSED",
        "Bad Gateway",
        "agent upstream closed",
    )
}

pub fn encode_line(value: &serde_json::Value) -> AppResult<Vec<u8>> {
    let mut line = serde_json::to_vec(value).map_err(anyhow::Error::from)?;
    line.push(b'\n');
    Ok(line)
}

#[async_trait::async_trait]
pub trait AgentStdin: Send {
    async fn write_normal(&mut self, value: &serde_json::Value) -> AppResult<()>;
    async fn write_control(&mut self, value: &serde_json::Value) -> AppResult<()>;
}

#[async_trait::async_trait]
impl<W> AgentStdin for W
where
    W: AsyncWrite + Unpin + Send,
{
    async fn write_normal(&mut self, value: &serde_json::Value) -> AppResult<()> {
        write_direct(self, value).await
    }

    async fn write_control(&mut self, value: &serde_json::Value) -> AppResult<()> {
        write_direct(self, value).await
    }
}

async fn write_direct<W>(writer: &mut W, value: &serde_json::Value) -> AppResult<()>
where
    W: AsyncWrite + Unpin,
{
    let line = encode_line(value)?;
    writer
        .write_all(&line)
        .await
        .map_err(|_| closed())?;
    writer.flush().await.map_err(|_| closed())?;
    Ok(())
}

#[async_trait::async_trait]
impl AgentStdin for StdinGate {
    async fn write_normal(&mut self, value: &serde_json::Value) -> AppResult<()> {
        self.send_normal(encode_line(value)?).await
    }

    async fn write_control(&mut self, value: &serde_json::Value) -> AppResult<()> {
        self.send_control(encode_line(value)?).await
    }
}

pub struct StderrGuard {
    kept: usize,
    dropped: usize,
    last_emit: Option<Instant>,
}

impl StderrGuard {
    pub fn new() -> Self {
        Self {
            kept: 0,
            dropped: 0,
            last_emit: None,
        }
    }

    pub fn dropped(&self) -> usize {
        self.dropped
    }

    pub fn push(&mut self, raw: &str, now: Instant) -> Option<String> {
        let clean = scrub_process_text(raw);
        if clean.is_empty() {
            self.dropped += raw.len();
            return None;
        }
        if self
            .last_emit
            .is_some_and(|last| now.saturating_duration_since(last) < STDERR_INTERVAL)
        {
            self.dropped += clean.len();
            return None;
        }
        let room = STDERR_BUDGET.saturating_sub(self.kept);
        if room == 0 {
            self.dropped += clean.len();
            return None;
        }
        let out: String = clean.chars().take(room).collect();
        if out.chars().count() < clean.chars().count() {
            self.dropped += clean.chars().count() - out.chars().count();
        }
        self.kept += out.chars().count();
        self.last_emit = Some(now);
        Some(out).filter(|text| !text.is_empty())
    }
}

pub fn scrub_process_text(raw: &str) -> String {
    let mut out = String::new();
    for token in raw.split_whitespace() {
        if token.contains('/') || token.contains('\\') || token.contains("sk-") {
            continue;
        }
        let clean: String = token.chars().filter(|ch| !ch.is_control()).collect();
        if clean.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&clean);
    }
    out
}

pub fn pump_stderr<R>(reader: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(reader);
        let mut guard = StderrGuard::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(safe) = guard.push(&line, Instant::now()) {
                        tracing::warn!(target: "gallery_agent_stderr", "{safe}");
                    }
                }
            }
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotError {
    SessionBusy,
    InstanceBusy,
}

struct IdleProc<T> {
    session_id: String,
    at: Instant,
    handle: T,
}

pub struct CodexSlots<T> {
    active: Option<String>,
    idle: VecDeque<IdleProc<T>>,
    limit: usize,
    ttl: Duration,
}

pub struct Admission<T> {
    pub resumed: Option<T>,
    pub released: Vec<T>,
}

impl<T> CodexSlots<T> {
    pub fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            active: None,
            idle: VecDeque::new(),
            limit,
            ttl,
        }
    }

    pub fn begin(&mut self, session_id: &str, now: Instant) -> Result<Admission<T>, (SlotError, Vec<T>)> {
        let released = self.reap(now);
        if self.active.as_deref() == Some(session_id) {
            return Err((SlotError::SessionBusy, released));
        }
        if self.active.is_some() {
            return Err((SlotError::InstanceBusy, released));
        }
        let resumed = self
            .idle
            .iter()
            .position(|row| row.session_id == session_id)
            .map(|pos| self.idle.remove(pos).unwrap().handle);
        self.active = Some(session_id.to_string());
        Ok(Admission { resumed, released })
    }

    pub fn park(&mut self, session_id: &str, handle: T, now: Instant) -> Vec<T> {
        if self.active.as_deref() == Some(session_id) {
            self.active = None;
        }
        self.idle.push_back(IdleProc {
            session_id: session_id.to_string(),
            at: now,
            handle,
        });
        let mut released = self.reap(now);
        while self.idle.len() > self.limit {
            if let Some(old) = self.idle.pop_front() {
                released.push(old.handle);
            }
        }
        released
    }

    pub fn abort(&mut self, session_id: &str) {
        if self.active.as_deref() == Some(session_id) {
            self.active = None;
        }
    }

    pub fn take_all(&mut self) -> Vec<T> {
        self.active = None;
        let mut released = Vec::new();
        while let Some(row) = self.idle.pop_front() {
            released.push(row.handle);
        }
        released
    }

    pub fn idle_sessions(&self) -> Vec<String> {
        self.idle.iter().map(|row| row.session_id.clone()).collect()
    }

    fn reap(&mut self, now: Instant) -> Vec<T> {
        let mut released = Vec::new();
        let mut kept = VecDeque::new();
        while let Some(row) = self.idle.pop_front() {
            if now.saturating_duration_since(row.at) >= self.ttl {
                released.push(row.handle);
            } else {
                kept.push_back(row);
            }
        }
        self.idle = kept;
        released
    }
}

pub const CODEX_IDLE_LIMIT: usize = 2;
pub const CODEX_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn control_line_is_written_before_queued_normal_lines() {
        let (client, mut server) = tokio::io::duplex(16);
        let gate = StdinGate::spawn(client);
        let first = gate.clone();
        let started = tokio::spawn(async move {
            first
                .send_normal(b"N1-padded-so-the-pipe-stays-full-until-read\n".to_vec())
                .await
                .unwrap();
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        let second = gate.clone();
        let third = gate.clone();
        let later = tokio::spawn(async move {
            second.send_normal(b"N2\n".to_vec()).await.unwrap();
            third.send_normal(b"N3\n".to_vec()).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let stop = gate.clone();
        let stopped = tokio::spawn(async move {
            stop.send_control(b"STOP\n".to_vec()).await.unwrap();
        });
        let mut collected = Vec::new();
        let mut buf = [0u8; 128];
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !collected.ends_with(b"N3\n") && !String::from_utf8_lossy(&collected).contains("N3") {
            let n = tokio::time::timeout(Duration::from_millis(200), server.read(&mut buf))
                .await
                .ok()
                .and_then(|result| result.ok())
                .unwrap_or(0);
            if n == 0 {
                if started.is_finished() && later.is_finished() && stopped.is_finished() {
                    break;
                }
                tokio::task::yield_now().await;
                continue;
            }
            collected.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8_lossy(&collected);
        let n1 = text.find("N1-padded").expect(&text);
        let stop_at = text.find("STOP").expect(&text);
        let n2 = text.find("N2").expect(&text);
        assert!(n1 < stop_at && stop_at < n2, "{text}");
        started.await.unwrap();
        later.await.unwrap();
        stopped.await.unwrap();
    }

    #[test]
    fn stderr_drops_paths_keys_and_extra_bursts() {
        let mut guard = StderrGuard::new();
        let now = Instant::now();
        let kept = guard
            .push(
                "failed plan=/tmp/secret.png error_code=CODEC elapsed_ms=4 sk-or-v1-secret",
                now,
            )
            .unwrap();
        assert_eq!(kept, "failed error_code=CODEC elapsed_ms=4");
        assert!(guard.push("again error_code=CODEC", now).is_none());
        assert!(guard.dropped() > 0);
        let later = guard
            .push("turn_id=abc elapsed_ms=9", now + STDERR_INTERVAL)
            .unwrap();
        assert_eq!(later, "turn_id=abc elapsed_ms=9");
    }

    #[test]
    fn idle_processes_stay_under_two_and_expire() {
        let start = Instant::now();
        let mut slots = CodexSlots::new(CODEX_IDLE_LIMIT, Duration::from_secs(15 * 60));
        assert!(slots.begin("a", start).is_ok());
        assert!(matches!(
            slots.begin("a", start),
            Err((SlotError::SessionBusy, _))
        ));
        assert!(matches!(
            slots.begin("b", start),
            Err((SlotError::InstanceBusy, _))
        ));
        assert!(slots.park("a", "proc-a", start).is_empty());
        let resumed = slots.begin("a", start).unwrap();
        assert_eq!(resumed.resumed, Some("proc-a"));
        assert!(slots.park("a", "proc-a", start).is_empty());
        assert!(slots.begin("b", start).unwrap().resumed.is_none());
        assert!(slots.park("b", "proc-b", start).is_empty());
        assert!(slots.begin("c", start).unwrap().resumed.is_none());
        let evicted = slots.park("c", "proc-c", start);
        assert_eq!(evicted, vec!["proc-a"]);
        assert_eq!(slots.idle_sessions(), vec!["b".to_string(), "c".to_string()]);
        let mut short = CodexSlots::new(2, Duration::from_secs(15 * 60));
        assert!(short.begin("old", start).unwrap().resumed.is_none());
        assert!(short.park("old", "gone", start).is_empty());
        let later = short.begin("new", start + CODEX_IDLE_TTL).unwrap();
        assert_eq!(later.released, vec!["gone"]);
        assert!(later.resumed.is_none());
    }
}
