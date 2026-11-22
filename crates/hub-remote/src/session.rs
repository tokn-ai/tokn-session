//! One ordered Noise channel per carrier. The driver is the sole owner of its
//! cipher state, so concurrent HTTP exchanges cannot reorder encrypted records.
use crate::exchange::read_record;
use std::{
  collections::HashMap,
  sync::{Arc, OnceLock},
  time::Duration,
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tokn_hub_client_core::{
  discovery::DiscoveryGroup,
  exchange::StreamResponse,
  protocol::RESPONSE_WINDOW,
  secure::{InnerMessage, MAX_STREAMS, NoiseIdentity, NoiseInitiator, SecureChannel},
};
use tokn_hub_transport::BoxTransport;

enum Outgoing {
  Abandon {
    id: u32,
    timed_out: bool,
  },
  Open {
    id: u32,
    first: InnerMessage,
    response: StreamResponse,
    reply: mpsc::Sender<InnerMessage>,
  },
  Frame {
    id: u32,
    frame: InnerMessage,
  },
}

pub(crate) struct Session {
  sender: mpsc::Sender<Outgoing>,
  last_id: Mutex<u32>,
  slots: Arc<Semaphore>,
  failure: Arc<OnceLock<String>>,
  stopped: CancellationToken,
}

impl Session {
  pub async fn authenticate(
    mut transport: BoxTransport,
    identity: &NoiseIdentity,
    host_key: &str,
    cancellation: CancellationToken,
  ) -> Result<Arc<Self>, String> {
    let mut initiator = NoiseInitiator::new(identity, host_key)?;
    transport.send(initiator.start()?).await?;
    let channel = initiator.finish(&read_record(&mut transport).await?)?;
    let (sender, commands) = mpsc::channel(MAX_STREAMS * 3);
    let session = Arc::new(Self {
      sender,
      last_id: Mutex::new(0),
      slots: Arc::new(Semaphore::new(MAX_STREAMS)),
      failure: Arc::new(OnceLock::new()),
      stopped: CancellationToken::new(),
    });
    let failure = session.failure.clone();
    let done = session.stopped.clone().drop_guard();
    tokio::spawn(async move {
      let mut driver = Driver {
        channel,
        last_id: 0,
        routes: HashMap::new(),
      };
      let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err("Machine disconnected".into()),
        result = driver.run(&mut transport, commands) => result,
      };
      if let Err(error) = result {
        let _ = failure.set(error);
      }
      drop(done);
      drop(driver);
      let _ = tokio::time::timeout(Duration::from_secs(3), transport.close()).await;
    });
    Ok(session)
  }

  pub fn stopped(&self) -> CancellationToken {
    self.stopped.clone()
  }

  pub async fn open(&self, first: InnerMessage) -> Result<Stream, String> {
    let response = StreamResponse::new(&first)?;
    let slot = self
      .slots
      .clone()
      .try_acquire_owned()
      .map_err(|_| "Too many concurrent machine streams")?;
    // Reserving a queue slot makes drop/timeout cancellation reliable even
    // while other streams are uploading. Each logical stream owns one permit.
    let cancel = self.sender.clone().reserve_owned().await.map_err(|_| self.error())?;
    let (reply, receiver) = mpsc::channel(RESPONSE_WINDOW + 2);
    let mut last_id = self.last_id.lock().await;
    let id = last_id.checked_add(1).ok_or("Machine stream identities exhausted")?;
    let stream = Stream {
      id,
      sender: self.sender.clone(),
      receiver,
      cancel: Some(cancel),
      failure: self.failure.clone(),
      _slot: slot,
    };
    self
      .sender
      .send(Outgoing::Open {
        id,
        first,
        response,
        reply,
      })
      .await
      .map_err(|_| self.error())?;
    *last_id = id;
    Ok(stream)
  }

  pub async fn discovery(&self) -> Result<Option<DiscoveryGroup>, String> {
    match self.request_reply(InnerMessage::DiscoveryRequest {}).await? {
      InnerMessage::DiscoveryResponse { group } => Ok(group),
      _ => Err("Machine returned invalid discovery metadata".into()),
    }
  }

  pub async fn request_reply(&self, request: InnerMessage) -> Result<InnerMessage, String> {
    let mut stream = self.open(request).await?;
    match stream.receive().await? {
      InnerMessage::Error { message } => Err(message),
      reply => Ok(reply),
    }
  }

  fn error(&self) -> String {
    session_error(&self.failure)
  }
}

fn session_error(failure: &OnceLock<String>) -> String {
  failure
    .get()
    .cloned()
    .unwrap_or_else(|| "Machine connection closed; delivery may be uncertain".into())
}

pub(crate) struct Stream {
  id: u32,
  sender: mpsc::Sender<Outgoing>,
  receiver: mpsc::Receiver<InnerMessage>,
  cancel: Option<mpsc::OwnedPermit<Outgoing>>,
  failure: Arc<OnceLock<String>>,
  _slot: OwnedSemaphorePermit,
}

impl Stream {
  pub async fn send(&self, frame: InnerMessage) -> Result<(), String> {
    self
      .sender
      .send(Outgoing::Frame { id: self.id, frame })
      .await
      .map_err(|_| session_error(&self.failure))
  }

  pub async fn receive(&mut self) -> Result<InnerMessage, String> {
    match tokio::time::timeout(Duration::from_secs(90), self.receiver.recv()).await {
      Ok(Some(message)) => Ok(message),
      Ok(None) => Err(session_error(&self.failure)),
      Err(_) => {
        self.abandon(true);
        Err("Machine response timed out; delivery may be uncertain".into())
      }
    }
  }

  fn abandon(&mut self, timed_out: bool) {
    if !self.receiver.is_closed() {
      if let Some(cancel) = self.cancel.take() {
        cancel.send(Outgoing::Abandon { id: self.id, timed_out });
      }
    }
  }
}

impl Drop for Stream {
  fn drop(&mut self) {
    self.abandon(false);
  }
}

struct Route {
  reply: mpsc::Sender<InnerMessage>,
  response: StreamResponse,
}

struct Driver {
  channel: SecureChannel,
  last_id: u32,
  routes: HashMap<u32, Route>,
}

impl Driver {
  async fn run(&mut self, transport: &mut BoxTransport, mut commands: mpsc::Receiver<Outgoing>) -> Result<(), String> {
    loop {
      tokio::select! {
        command = commands.recv() => {
          let Some(command) = command else { return Ok(()); };
          if let Some(frame) = self.outgoing(command)? {
            transport.send(self.channel.encrypt(&frame)?).await?;
          }
        }
        record = transport.receive() => {
          let record = record?.ok_or("Machine connection closed; delivery may be uncertain")?;
          let message = self.channel.decrypt(&record)?;
          self.incoming(message)?;
        }
      }
    }
  }

  fn outgoing(&mut self, command: Outgoing) -> Result<Option<InnerMessage>, String> {
    let (stream_id, frame) = match command {
      Outgoing::Open {
        id,
        first,
        response,
        reply,
      } => {
        self.last_id = id;
        if reply.is_closed() {
          return Ok(None);
        }
        self.routes.insert(id, Route { reply, response });
        (id, first)
      }
      Outgoing::Frame { id, frame } => {
        let Some(route) = self.routes.get_mut(&id) else {
          return Ok(None);
        };
        if let InnerMessage::Window { credits } = &frame {
          route.response.acknowledge(*credits)?;
        }
        (id, frame)
      }
      Outgoing::Abandon { id, timed_out } => {
        if self.routes.remove(&id).is_none() {
          return Ok(None);
        }
        // A timed-out route can still have healthy siblings. When the final
        // outstanding exchange times out, force the next call to reauthenticate.
        if timed_out && self.routes.is_empty() {
          return Err("Machine response timed out; delivery may be uncertain".into());
        }
        (id, InnerMessage::Cancel {})
      }
    };
    Ok(Some(frame.on_stream(stream_id)))
  }

  fn incoming(&mut self, message: InnerMessage) -> Result<(), String> {
    let (id, frame) = match message {
      InnerMessage::Stream { stream_id, frame } => (stream_id, *frame),
      InnerMessage::Error { message } => return Err(message),
      _ => return Err("Expected a machine stream response".into()),
    };
    if id == 0 || id > self.last_id {
      return Err("Invalid machine response stream".into());
    }
    let Some(route) = self.routes.get_mut(&id) else {
      return Ok(());
    };
    let ended = route.response.accept(&frame)?;
    if let Err(mpsc::error::TrySendError::Full(_)) = route.reply.try_send(frame) {
      return Err("Machine exceeded its stream response window".into());
    }
    if ended {
      self.routes.remove(&id);
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests;
