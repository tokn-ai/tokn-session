use super::{Inner, InteractionError, LaunchOptions};
use serde_json::{Value, json};
use std::{
  collections::HashMap,
  process::Stdio,
  sync::{
    Mutex, Weak,
    atomic::{AtomicU64, Ordering},
  },
  time::Duration,
};
use tokio::{
  io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
  process::{Child, ChildStdin, ChildStdout, Command},
  sync::{Mutex as AsyncMutex, oneshot, watch},
  task::JoinHandle,
};

const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const OUTBOUND_LIMIT: usize = 1024 * 1024;
const IN_FLIGHT_LIMIT: usize = 32;
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const EOF_TIMEOUT: Duration = Duration::from_secs(3);

type Pending = Mutex<HashMap<u64, oneshot::Sender<Value>>>;

pub(super) struct Transport {
  child: AsyncMutex<Child>,
  writer: AsyncMutex<Option<ChildStdin>>,
  pending: Pending,
  terminal: watch::Sender<bool>,
  counter: AtomicU64,
  reader: Mutex<Option<JoinHandle<()>>>,
}

struct Registration<'a>(&'a Pending, u64);

impl Drop for Registration<'_> {
  fn drop(&mut self) {
    self.0.lock().unwrap().remove(&self.1);
  }
}

impl Drop for Transport {
  fn drop(&mut self) {
    if let Some(task) = self.reader.get_mut().unwrap().take() {
      task.abort();
    }
  }
}

impl Transport {
  pub fn launch(options: &LaunchOptions) -> Result<(Self, ChildStdout), InteractionError> {
    let mut child = Command::new(&options.executable)
      .args(&options.arguments)
      .args(["app-server", "--stdio"])
      .envs(&options.environment)
      .current_dir(&options.directory)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      // Native stderr can contain provider credentials, filesystem paths, and user input.
      .stderr(Stdio::null())
      .kill_on_drop(true)
      .spawn()
      .map_err(|_| InteractionError::unavailable("Could not launch the configured ZCode executable"))?;
    let stdin = child.stdin.take().expect("configured ZCode stdin");
    let stdout = child.stdout.take().expect("configured ZCode stdout");
    let (terminal, _) = watch::channel(false);
    Ok((
      Self {
        child: AsyncMutex::new(child),
        writer: AsyncMutex::new(Some(stdin)),
        pending: Mutex::new(HashMap::new()),
        terminal,
        counter: AtomicU64::new(1),
        reader: Mutex::new(None),
      },
      stdout,
    ))
  }

  pub fn start_reader(&self, stdout: ChildStdout, inner: Weak<Inner>) {
    let mut terminal = self.terminal.subscribe();
    *self.reader.lock().unwrap() = Some(tokio::spawn(async move {
      let mut reader = FrameReader(BufReader::new(stdout));
      loop {
        let frame = tokio::select! {
          frame = reader.read() => frame,
          _ = terminal.changed() => {
            if let Some(inner) = inner.upgrade() {
              inner.disconnect("ZCode closed its app-server connection");
            }
            // Keep stdout open during EOF shutdown so native cleanup can finish.
            let _ = tokio::io::copy(&mut reader.0, &mut tokio::io::sink()).await;
            break;
          }
        };
        let Some(inner) = inner.upgrade() else { break };
        match frame {
          Ok(Some(frame)) => {
            if let Err(error) = inner.receive(frame).await {
              inner.disconnect(&error.message);
              break;
            }
          }
          Ok(None) => {
            inner.disconnect("ZCode closed its app-server connection");
            break;
          }
          Err(()) => {
            inner.disconnect("ZCode returned an invalid or oversized app-server frame");
            break;
          }
        }
      }
    }));
  }

  pub fn is_live(&self) -> bool {
    !*self.terminal.borrow()
  }

  pub fn terminate(&self) {
    self.terminal.send_replace(true);
    self.pending.lock().unwrap().clear();
  }

  pub fn accept_reply(&self, id: &Value, frame: Value) {
    if let Some(reply) = id.as_u64().and_then(|id| self.pending.lock().unwrap().remove(&id)) {
      let _ = reply.send(frame);
    }
  }

  pub async fn call(&self, method: &str, params: Value, mutation: bool) -> Result<Value, InteractionError> {
    let mut terminal = self.terminal.subscribe();
    if *terminal.borrow_and_update() {
      return Err(InteractionError::unavailable("ZCode app-server is disconnected"));
    }
    let id = self.counter.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = oneshot::channel();
    {
      let mut pending = self.pending.lock().unwrap();
      if pending.len() >= IN_FLIGHT_LIMIT {
        return Err(InteractionError::unavailable(
          "ZCode app-server has too many pending requests",
        ));
      }
      pending.insert(id, sender);
    }
    let _registration = Registration(&self.pending, id);
    self
      .send(json!({"id": id, "method": method, "params": params}), mutation)
      .await?;
    let lost = || {
      if mutation {
        InteractionError::uncertain("ZCode did not confirm whether the request took effect")
      } else {
        InteractionError::unavailable("ZCode did not reply to the app-server request")
      }
    };
    let mut reply = tokio::time::timeout(REQUEST_TIMEOUT, async {
      tokio::select! {
        biased;
        response = receiver => response.map_err(|_| lost()),
        _ = terminal.changed() => Err(lost()),
      }
    })
    .await
    .map_err(|_| lost())??;
    if let Some(error) = reply.get("error") {
      return Err(native_error(error, mutation));
    }
    reply
      .as_object_mut()
      .and_then(|reply| reply.remove("result"))
      .ok_or_else(lost)
  }

  pub async fn send(&self, frame: Value, mutation: bool) -> Result<(), InteractionError> {
    let mut bytes = serde_json::to_vec(&frame).map_err(|_| InteractionError::invalid("Invalid ZCode request"))?;
    if bytes.len() >= OUTBOUND_LIMIT {
      return Err(InteractionError::invalid("ZCode request exceeds the 1 MiB frame limit"));
    }
    bytes.push(b'\n');
    let mut writer = tokio::time::timeout(WRITE_TIMEOUT, self.writer.lock())
      .await
      .map_err(|_| InteractionError::unavailable("ZCode app-server writer is busy"))?;
    if writer.is_none() || !self.is_live() {
      return Err(InteractionError::unavailable("ZCode app-server is disconnected"));
    }
    // A caller cancelling its future must not leave a partially written frame in the pipe.
    // The frame is owned by this scope until completion; a cancelled write closes the pipe.
    let mut write = WriteGuard {
      writer: &mut writer,
      transport: self,
      complete: false,
    };
    let pipe = write.writer.as_mut().expect("checked ZCode stdin");
    if matches!(
      tokio::time::timeout(WRITE_TIMEOUT, pipe.write_all(&bytes)).await,
      Ok(Ok(()))
    ) {
      write.complete = true;
      Ok(())
    } else if mutation {
      Err(InteractionError::uncertain("Could not finish writing a ZCode mutation"))
    } else {
      Err(InteractionError::unavailable("Could not write to the ZCode app-server"))
    }
  }

  pub async fn close(&self) -> Result<(), InteractionError> {
    self.terminate();
    self.writer.lock().await.take();
    let mut child = self.child.lock().await;
    let result = match tokio::time::timeout(EOF_TIMEOUT, child.wait()).await {
      Ok(status) => status
        .map(|_| ())
        .map_err(|_| InteractionError::unavailable("Could not reap the ZCode child")),
      Err(_) => match tokio::time::timeout(EOF_TIMEOUT, child.kill()).await {
        Ok(status) => status.map_err(|_| InteractionError::unavailable("Could not terminate the ZCode child")),
        Err(_) => Err(InteractionError::unavailable("Timed out terminating the ZCode child")),
      },
    };
    if let Some(reader) = self.reader.lock().unwrap().take() {
      reader.abort();
    }
    result
  }
}

struct WriteGuard<'a> {
  writer: &'a mut Option<ChildStdin>,
  transport: &'a Transport,
  complete: bool,
}

impl Drop for WriteGuard<'_> {
  fn drop(&mut self) {
    if !self.complete {
      self.writer.take();
      self.transport.terminate();
    }
  }
}

fn native_error(error: &Value, mutation: bool) -> InteractionError {
  use tokn_interaction_core::ErrorKind;
  match error["code"].as_i64() {
    Some(-32601) => InteractionError::unsupported("ZCode app-server does not support this operation"),
    Some(-32602) => InteractionError::invalid("ZCode rejected the request parameters or identity"),
    Some(-32004) => InteractionError::new(ErrorKind::StaleTarget, "ZCode session is no longer available"),
    Some(-32009) => InteractionError::new(ErrorKind::Conflict, "ZCode session state changed"),
    Some(-32010) => InteractionError::new(ErrorKind::Conflict, "ZCode is busy or the session is read-only"),
    Some(-32031) => InteractionError::new(
      ErrorKind::Rejected,
      "ZCode requires model or session configuration before sending",
    ),
    _ if mutation => {
      InteractionError::uncertain("ZCode returned an error without confirming whether the request took effect")
    }
    _ => InteractionError::unavailable("ZCode app-server could not complete the request"),
  }
}

struct FrameReader<R>(BufReader<R>);

impl<R: AsyncRead + Unpin> FrameReader<R> {
  async fn read(&mut self) -> Result<Option<Value>, ()> {
    let mut frame = Vec::new();
    loop {
      let available = self.0.fill_buf().await.map_err(|_| ())?;
      if available.is_empty() {
        return if frame.is_empty() { Ok(None) } else { Err(()) };
      }
      let newline = available.iter().position(|byte| *byte == b'\n');
      let size = newline.unwrap_or(available.len());
      if size > FRAME_LIMIT - frame.len() {
        return Err(());
      }
      frame.extend_from_slice(&available[..size]);
      self.0.consume(size + usize::from(newline.is_some()));
      if newline.is_some() {
        if frame.iter().all(u8::is_ascii_whitespace) {
          frame.clear();
          continue;
        }
        let value: Value = serde_json::from_slice(&frame).map_err(|_| ())?;
        return value.is_object().then_some(Some(value)).ok_or(());
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[cfg(unix)]
  #[tokio::test]
  async fn a_partial_response_write_is_uncertain_and_permanently_closes_the_pipe() {
    let options = LaunchOptions {
      runtime_id: "blocked-fixture".into(),
      executable: "/bin/sh".into(),
      arguments: vec!["-c".into(), "kill -STOP $$".into()],
      directory: std::env::current_dir().unwrap(),
      history_path: "/fixture/db.sqlite".into(),
      environment: Default::default(),
    };
    let (transport, _stdout) = Transport::launch(&options).unwrap();
    let error = transport
      .send(
        json!({"id":"server-1","result":{
          "action":"accept", "content":{"answer":"x".repeat(OUTBOUND_LIMIT / 2)}
        }}),
        true,
      )
      .await
      .unwrap_err();
    assert_eq!(error.kind, tokn_interaction_core::ErrorKind::Uncertain);
    assert!(!transport.is_live());
    assert!(transport.writer.lock().await.is_none());
    assert!(
      transport
        .send(json!({"id":"server-2","result":{"action":"accept"}}), true)
        .await
        .is_err()
    );
    transport.close().await.unwrap();
  }

  #[tokio::test]
  async fn reads_adjacent_native_ndjson_frames_and_preserves_request_id_types() {
    let frames = b"\n{\"id\":1,\"result\":{}}\r\n{\"id\":\"callback\",\"method\":\"interaction/requestPermission\"}\n";
    let mut reader = FrameReader(BufReader::new(&frames[..]));
    assert_eq!(reader.read().await.unwrap(), Some(json!({"id": 1, "result": {}})));
    assert_eq!(reader.read().await.unwrap().unwrap()["id"], "callback");
    assert_eq!(reader.read().await.unwrap(), None);
  }

  #[tokio::test]
  async fn rejects_truncated_oversized_and_invalid_frames() {
    for bytes in [&b"{\"id\":1}"[..], b"{}\x01\n", b"[{}]\n"] {
      let mut reader = FrameReader(BufReader::new(bytes));
      assert!(reader.read().await.is_err());
    }
    let bytes = vec![b' '; FRAME_LIMIT + 1];
    let mut reader = FrameReader(BufReader::new(bytes.as_slice()));
    assert!(reader.read().await.is_err());
  }
}
