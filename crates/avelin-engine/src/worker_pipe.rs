use super::transport;
use anyhow::{Context, Result, bail, ensure};
use avelin_engine::EngineObserver;
use serde_json::{Value, json};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

struct State {
    cancelled: AtomicBool,
    error: Mutex<Option<String>>,
    stop: AtomicBool,
}
type SendRequest = (Value, mpsc::Sender<Result<(), String>>);
pub struct Observer {
    state: Arc<State>,
    sender: mpsc::SyncSender<SendRequest>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Observer {
    pub fn new(mut connection: transport::Connection) -> Result<Self> {
        let state = Arc::new(State {
            cancelled: AtomicBool::new(false),
            error: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let shared = state.clone();
        let (sender, receiver) = mpsc::sync_channel::<SendRequest>(16);
        let worker = thread::Builder::new()
            .name("avelin-engine-pipe".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<()> {
                        while !shared.stop.load(Ordering::Acquire) {
                            match receiver.recv_timeout(Duration::from_millis(5)) {
                                Ok((event, reply)) => {
                                    let result = connection.send(event);
                                    let _ = reply.send(
                                        result.as_ref().map(|_| ()).map_err(ToString::to_string),
                                    );
                                    result?;
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                            }
                            match connection.recv_timeout(Duration::ZERO) {
                                Ok(event) if event["kind"] == "cancel" => {
                                    if !shared.cancelled.swap(true, Ordering::AcqRel) {
                                        connection.send(json!({"kind":"cancel_received"}))?;
                                    }
                                }
                                Ok(_) => {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "unexpected coordinator command",
                                    ));
                                }
                                Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                                Err(error) => return Err(error),
                            }
                        }
                        Ok(())
                    }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(_) => Some("pipe worker panicked".into()),
                };
                if let Some(error) = error
                    && let Ok(mut slot) = shared.error.lock()
                {
                    *slot = Some(error);
                }
            })?;
        Ok(Self {
            state,
            sender,
            worker: Some(worker),
        })
    }
    fn send(&self, event: Value) -> Result<()> {
        self.connected()?;
        let (reply, result) = mpsc::channel();
        self.sender
            .try_send((event, reply))
            .context("engine event queue unavailable")?;
        result
            .recv_timeout(Duration::from_secs(3))
            .context("engine event send timed out")?
            .map_err(anyhow::Error::msg)
    }
    fn connected(&self) -> Result<()> {
        let error = self
            .state
            .error
            .lock()
            .map_err(|_| anyhow::anyhow!("engine pipe state poisoned"))?;
        if let Some(error) = &*error {
            bail!("coordinator connection lost: {error}");
        }
        ensure!(
            self.worker
                .as_ref()
                .is_some_and(|thread| !thread.is_finished()),
            "engine pipe thread stopped"
        );
        Ok(())
    }
    pub fn finish(&self, code: i32, detail: &str) -> Result<()> {
        let result = match code {
            0 => "Succeeded",
            2 => "Cancelled",
            3 => "Failed",
            4 => "Unknown",
            _ => bail!("invalid terminal code"),
        };
        let detail: String = detail.chars().take(2048).collect();
        self.send(json!({"kind":"finish","result":result,"code":code,"detail":detail}))
    }
}
impl EngineObserver for Observer {
    fn progress(&mut self, phase: &str, completed: u64, total: u64) -> Result<()> {
        self.send(json!({"kind":"progress","phase":phase,"completed":completed,"total":total}))
    }
    fn checkpoint(&mut self) -> Result<bool> {
        self.connected()?;
        Ok(self.state.cancelled.load(Ordering::Acquire))
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
