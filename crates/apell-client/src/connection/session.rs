use crate::host;
use futures_channel::{mpsc, oneshot};
use futures_util::{
  StreamExt,
  future::{AbortHandle, Abortable},
};
use std::{
  cell::{Cell, RefCell},
  collections::BTreeMap,
  rc::Rc,
};
use tokn_hub_client_core::{
  exchange::StreamResponse,
  protocol::RESPONSE_WINDOW,
  secure::{InnerMessage, MAX_STREAMS, NoiseIdentity, NoiseInitiator, SecureChannel},
};

struct Write {
  stream_id: u32,
  frame: InnerMessage,
  sent: Option<oneshot::Sender<()>>,
}

struct Route {
  incoming: mpsc::Sender<InnerMessage>,
  response: StreamResponse,
}

/// One reader and one writer per socket. Dropping a caller cannot cancel an
/// encrypted write halfway through and desynchronize the shared Noise nonces.
pub(super) struct Session {
  socket: Rc<host::Socket>,
  channel: RefCell<SecureChannel>,
  outgoing: RefCell<mpsc::Sender<Write>>,
  streams: RefCell<BTreeMap<u32, Route>>,
  wake_reader: RefCell<mpsc::Sender<()>>,
  last_id: Cell<u32>,
  failure: RefCell<Option<String>>,
  workers: [AbortHandle; 2],
}

impl Session {
  pub async fn connect(endpoint: &str, identity: &NoiseIdentity, host_key: &str) -> Result<Rc<Self>, String> {
    let socket = Rc::new(host::Socket::open(endpoint).await?);
    let mut initiator = NoiseInitiator::new_with_entropy(identity, host_key, host::entropy().await?)?;
    socket.send(initiator.start()?).await?;
    let channel = initiator.finish(&socket.receive().await?)?;
    // Each stream awaits its current write. Reserve a further slot per stream
    // for cancellation, which must also be possible while a write is queued.
    let (outgoing, mut writes) = mpsc::channel::<Write>(2 * MAX_STREAMS);
    let (wake_reader, mut new_stream) = mpsc::channel(1);
    let (reader, reader_abort) = AbortHandle::new_pair();
    let (writer, writer_abort) = AbortHandle::new_pair();
    let session = Rc::new(Self {
      socket: socket.clone(),
      channel: RefCell::new(channel),
      outgoing: RefCell::new(outgoing),
      streams: RefCell::new(BTreeMap::new()),
      wake_reader: RefCell::new(wake_reader),
      last_id: Cell::new(0),
      failure: RefCell::new(None),
      workers: [reader, writer],
    });
    let weak = Rc::downgrade(&session);
    let input = socket.clone();
    apell_guest::spawn(async move {
      let _ = Abortable::new(
        async move {
          loop {
            let Some(idle) = weak.upgrade().map(|session| session.streams.borrow().is_empty()) else {
              break;
            };
            // The Apell socket service retires a socket when a receive times out.
            // Start that timeout only while a stream is expecting a response.
            if idle {
              if new_stream.next().await.is_none() {
                break;
              }
              continue;
            }
            let record = input.receive().await;
            let Some(session) = weak.upgrade() else { break };
            let result = record
              .and_then(|record| session.channel.borrow_mut().decrypt(&record))
              .and_then(|frame| session.deliver(frame));
            if let Err(error) = result {
              session.stop(error);
              break;
            }
          }
        },
        reader_abort,
      )
      .await;
    })
    .map_err(|error| error.message)?;
    let weak = Rc::downgrade(&session);
    apell_guest::spawn(async move {
      let _ = Abortable::new(
        async move {
          while let Some(write) = writes.next().await {
            let record = {
              let Some(session) = weak.upgrade() else { break };
              if let InnerMessage::Window { credits } = &write.frame {
                let mut routes = session.streams.borrow_mut();
                if let Some(route) = routes.get_mut(&write.stream_id) {
                  if let Err(error) = route.response.acknowledge(*credits) {
                    drop(routes);
                    session.stop(error);
                    break;
                  }
                } else {
                  // The terminal record arrived before the consumer returned its credit.
                  if let Some(sent) = write.sent {
                    let _ = sent.send(());
                  }
                  continue;
                }
              }
              session
                .channel
                .borrow_mut()
                .encrypt(&write.frame.on_stream(write.stream_id))
            };
            let result = match record {
              Ok(bytes) => socket.send(bytes).await,
              Err(error) => Err(error),
            };
            if let Err(error) = result {
              if let Some(session) = weak.upgrade() {
                session.stop(error);
              }
              break;
            }
            if let Some(sent) = write.sent {
              let _ = sent.send(());
            }
          }
        },
        writer_abort,
      )
      .await;
    })
    .map_err(|error| error.message)?;
    Ok(session)
  }

  pub fn is_live(&self) -> bool {
    self.failure.borrow().is_none()
  }

  fn closed_reason(&self) -> String {
    self
      .failure
      .borrow()
      .clone()
      .unwrap_or_else(|| "Machine connection closed".into())
  }

  pub fn stop(&self, reason: String) {
    if self.is_live() {
      *self.failure.borrow_mut() = Some(reason);
      self.streams.borrow_mut().clear();
      for worker in &self.workers {
        worker.abort();
      }
      let socket = self.socket.clone();
      let _ = apell_guest::spawn(async move { socket.close().await });
    }
  }

  fn deliver(&self, frame: InnerMessage) -> Result<(), String> {
    let (stream_id, frame) = match frame {
      InnerMessage::Stream { stream_id, frame } if stream_id <= self.last_id.get() => (stream_id, *frame),
      InnerMessage::Error { message } => return Err(message),
      _ => return Err("Unexpected encrypted machine response".into()),
    };
    let mut routes = self.streams.borrow_mut();
    if let Some(route) = routes.get_mut(&stream_id) {
      let terminal = route.response.accept(&frame)?;
      route
        .incoming
        .try_send(frame)
        .map_err(|_| "Machine exceeded its stream response window")?;
      if terminal {
        routes.remove(&stream_id);
      }
    }
    Ok(())
  }

  fn queue(&self, write: Write) -> Result<(), String> {
    if !self.is_live() {
      return Err(self.closed_reason());
    }
    if self.outgoing.borrow_mut().try_send(write).is_err() {
      let reason = "Machine connection write queue is full";
      self.stop(reason.into());
      return Err(reason.into());
    }
    Ok(())
  }

  pub async fn open(self: &Rc<Self>, first: InnerMessage) -> Result<Stream, String> {
    if self.streams.borrow().len() >= MAX_STREAMS {
      return Err("Too many active machine requests".into());
    }
    let Some(id) = self.last_id.get().checked_add(1) else {
      self.stop("Machine stream IDs exhausted".into());
      return Err(self.closed_reason());
    };
    self.last_id.set(id);
    let (incoming, frames) = mpsc::channel(RESPONSE_WINDOW + 2);
    self.streams.borrow_mut().insert(
      id,
      Route {
        incoming,
        response: StreamResponse::new(&first)?,
      },
    );
    // A pending wake covers all opens since the last time the reader went idle.
    let _ = self.wake_reader.borrow_mut().try_send(());
    let stream = Stream {
      id,
      session: self.clone(),
      frames,
    };
    // Allocation and enqueue happen before the first yield, preserving open
    // order even when several renderer calls start concurrently.
    stream.send(first).await?;
    Ok(stream)
  }
}

impl Drop for Session {
  fn drop(&mut self) {
    for worker in &self.workers {
      worker.abort();
    }
  }
}

pub(super) struct Stream {
  id: u32,
  session: Rc<Session>,
  frames: mpsc::Receiver<InnerMessage>,
}

impl Stream {
  pub async fn send(&self, frame: InnerMessage) -> Result<(), String> {
    let (sent, wait) = oneshot::channel();
    self.session.queue(Write {
      stream_id: self.id,
      frame,
      sent: Some(sent),
    })?;
    wait.await.map_err(|_| self.session.closed_reason())
  }

  pub async fn receive(&mut self) -> Result<InnerMessage, String> {
    let frame = self.frames.next().await.ok_or_else(|| self.session.closed_reason())?;
    Ok(frame)
  }
}

impl Drop for Stream {
  fn drop(&mut self) {
    let cancel = self.session.streams.borrow_mut().remove(&self.id).is_some();
    if cancel && self.session.is_live() {
      let _ = self.session.queue(Write {
        stream_id: self.id,
        frame: InnerMessage::Cancel {},
        sent: None,
      });
    }
  }
}
