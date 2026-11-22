//! One ordered Noise connection carries independent, credit-limited HTTP exchanges.
use super::{
  ConnectorConfig, DirectManager, Host, InnerMessage, NegotiatedPeer, RecordOutput, SecureChannel, discovery_group,
  forward_device, protocol,
};
use crate::secure::MAX_STREAMS;
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

struct Reply {
  id: u32,
  frame: InnerMessage,
  peer: Option<NegotiatedPeer>,
}
impl Reply {
  fn new(id: u32, frame: InnerMessage) -> Self {
    Self { id, frame, peer: None }
  }

  fn terminal(&self) -> bool {
    matches!(
      self.frame,
      InnerMessage::End {}
        | InnerMessage::Error { .. }
        | InnerMessage::DirectAnswer { .. }
        | InnerMessage::DiscoveryResponse { .. }
        | InnerMessage::DirectConfig { .. }
    )
  }
}

struct Output {
  id: u32,
  queue: mpsc::Sender<Reply>,
}

impl Output {
  async fn send(&self, frame: InnerMessage) -> Result<(), String> {
    self
      .queue
      .send(Reply::new(self.id, frame))
      .await
      .map_err(|_| "Encrypted connection closed".into())
  }
}

type Jobs<'a> = FuturesUnordered<BoxFuture<'a, ()>>;

struct Request {
  input: Option<mpsc::Sender<InnerMessage>>,
  stop: CancellationToken,
  outstanding: usize,
}

impl Drop for Request {
  fn drop(&mut self) {
    self.stop.cancel();
  }
}

#[derive(Clone, Copy)]
struct Peer<'a> {
  host: &'a Host,
  host_id: &'a str,
  config: &'a ConnectorConfig,
  client: &'a reqwest::Client,
  recipient: &'a str,
  relay: bool,
  direct: Option<&'a Arc<DirectManager>>,
}

impl Peer<'_> {
  fn verify(&self) -> Result<(), String> {
    self.host.verify(self.host_id, self.recipient)
  }

  async fn execute(
    &self,
    header: InnerMessage,
    output: &Output,
    input: &mut mpsc::Receiver<InnerMessage>,
  ) -> Result<Reply, String> {
    if let InnerMessage::DirectOffer { sdp } = header {
      let manager = self.direct.ok_or("Direct connections are unavailable on this host")?;
      let (sdp, peer) = manager.negotiate(self.recipient, &sdp).await?;
      return Ok(Reply {
        id: output.id,
        frame: InnerMessage::DirectAnswer { sdp },
        peer: Some(peer),
      });
    }
    let InnerMessage::DeviceRequest { method, path } = header else {
      return Err("Expected encrypted device request".into());
    };
    let body = tokio::time::timeout(Duration::from_secs(10), read_body(input))
      .await
      .map_err(|_| "Encrypted request body timed out")??;
    self.verify()?;
    self
      .host
      .verify_runtime_management(self.config, self.recipient, &method, &path)?;
    respond(self.config, self.client, &method, &path, body, output, input).await?;
    Ok(Reply::new(output.id, InnerMessage::End {}))
  }
}

struct Requests<'a> {
  peer: Peer<'a>,
  last_open: u32,
  active: HashMap<u32, Request>,
  replies: mpsc::Sender<Reply>,
  jobs: Jobs<'a>,
}

impl Requests<'_> {
  fn open(&mut self, id: u32, frame: InnerMessage) -> Result<Option<InnerMessage>, String> {
    if id <= self.last_open {
      return Err("Encrypted stream identities must increase on every open".into());
    }
    self.last_open = id;
    self.peer.verify()?;
    match frame {
      InnerMessage::DiscoveryRequest {} => {
        return Ok(Some(match discovery_group(self.peer.config) {
          Ok(group) => InnerMessage::DiscoveryResponse { group },
          Err(message) => InnerMessage::Error { message },
        }));
      }
      InnerMessage::DirectConfigRequest {} | InnerMessage::DirectOffer { .. } if !self.peer.relay => {
        return Ok(Some(error(
          "Negotiate direct connections through the encrypted Hub channel",
        )));
      }
      InnerMessage::DirectConfigRequest {} => {
        return Ok(Some(InnerMessage::DirectConfig {
          ice_servers: self.peer.config.ice_servers.clone(),
        }));
      }
      _ => {}
    }
    if self.active.len() >= MAX_STREAMS || self.jobs.len() >= MAX_STREAMS {
      return Ok(Some(error("Too many requests on this machine connection")));
    }
    let Ok(permit) = self.peer.host.requests.clone().try_acquire_owned() else {
      return Ok(Some(error("Host request capacity reached")));
    };
    let (input, mut pending) = mpsc::channel(protocol::MAX_BODY.div_ceil(protocol::CHUNK_SIZE) + 2);
    let stop = CancellationToken::new();
    self.active.insert(
      id,
      Request {
        input: matches!(frame, InnerMessage::DeviceRequest { .. }).then_some(input),
        stop: stop.clone(),
        outstanding: 0,
      },
    );
    let peer = self.peer;
    let output = Output {
      id,
      queue: self.replies.clone(),
    };
    self.jobs.push(
      async move {
        let _permit = permit;
        let work = async {
          let terminal = peer
            .execute(frame, &output, &mut pending)
            .await
            .unwrap_or_else(|message| Reply::new(id, InnerMessage::Error { message }));
          let _ = output.queue.send(terminal).await;
        };
        tokio::select! { biased; _ = stop.cancelled() => {}, _ = work => {} }
      }
      .boxed(),
    );
    Ok(None)
  }

  fn input(&mut self, id: u32, frame: InnerMessage) -> Result<Option<InnerMessage>, String> {
    if matches!(
      frame,
      InnerMessage::DeviceRequest { .. }
        | InnerMessage::DiscoveryRequest {}
        | InnerMessage::DirectConfigRequest {}
        | InnerMessage::DirectOffer { .. }
    ) {
      return self.open(id, frame);
    }
    if id == 0 || id > self.last_open {
      return Err("Unexpected encrypted stream identity".into());
    }
    if matches!(frame, InnerMessage::Cancel {}) {
      self.active.remove(&id);
      return Ok(None);
    }
    if !matches!(
      frame,
      InnerMessage::RequestBody { .. } | InnerMessage::RequestEnd {} | InnerMessage::Window { .. }
    ) {
      return Err("Unexpected encrypted request frame".into());
    }
    let Some(request) = self.active.get_mut(&id) else {
      // Credits can cross a terminal response. A rejected upload can also
      // have valid body frames in flight. The ID cannot open a second request.
      return Ok(None);
    };
    if let InnerMessage::Window { credits } = &frame {
      if *credits > request.outstanding {
        self.active.remove(&id);
        return Ok(Some(error("Invalid encrypted response window")));
      }
      request.outstanding -= credits;
    }
    let Some(input) = &request.input else {
      self.active.remove(&id);
      return Ok(Some(error("Direct signaling requires a single request frame")));
    };
    if input
      .try_send(frame)
      .is_err_and(|e| matches!(e, mpsc::error::TrySendError::Full(_)))
    {
      self.active.remove(&id);
      return Ok(Some(error("Too many pending encrypted request frames")));
    }
    Ok(None)
  }
}

fn error(message: &str) -> InnerMessage {
  InnerMessage::Error {
    message: message.into(),
  }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn serve(
  host: &Host,
  host_id: &str,
  config: &ConnectorConfig,
  client: &reqwest::Client,
  mut channel: SecureChannel,
  mut incoming: mpsc::Receiver<bytes::Bytes>,
  outgoing: &RecordOutput,
  first: InnerMessage,
  relay: bool,
  direct: Option<&Arc<DirectManager>>,
) -> Result<(), String> {
  let mut initial = Some(first);
  // A single owner encrypts and writes each record, preserving the Noise
  // send nonce order even if several response futures become ready together.
  let (replies, mut responses) = mpsc::channel(protocol::RESPONSE_WINDOW);
  let recipient = channel.remote_public_key().to_owned();
  let peer = Peer {
    host,
    host_id,
    config,
    client,
    recipient: &recipient,
    relay,
    direct,
  };
  let mut requests = Requests {
    peer,
    last_open: 0,
    active: HashMap::new(),
    replies,
    jobs: FuturesUnordered::new(),
  };
  let mut policy = tokio::time::interval(Duration::from_secs(1));
  policy.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
  let result: Result<(), String> = async {
    loop {
      let reply = tokio::select! {
        _ = policy.tick() => { peer.verify()?; continue; }
        _ = requests.jobs.next(), if !requests.jobs.is_empty() => continue,
        message = async {
          match initial.take() {
            Some(first) => Ok(Some(first)),
            None => incoming.recv().await.map(|record| channel.decrypt(&record)).transpose(),
          }
        } => {
          let Some(frame) = message? else { return Ok(()); };
          let (id, frame) = unpack(frame)?;
          let Some(reply) = requests.input(id, frame)? else { continue; };
          Reply::new(id, reply)
        },
        Some(reply) = responses.recv() => {
          let Some(request) = requests.active.get_mut(&reply.id) else { continue; };
          if matches!(reply.frame, InnerMessage::Chunk { .. }) { request.outstanding += 1; }
          if reply.terminal() { requests.active.remove(&reply.id); }
          reply
        },
      };
      if reply.peer.is_some() {
        peer.verify()?;
      }
      let send = outgoing.send_with(|| channel.encrypt(&reply.frame.on_stream(reply.id)));
      tokio::pin!(send);
      loop {
        tokio::select! {
          biased;
          _ = policy.tick() => peer.verify()?,
          result = &mut send => { result?; break; }
        }
      }
      if let Some(peer) = reply.peer {
        peer.start();
      }
    }
  }
  .await;
  drop(requests);
  if let Err(message) = &result
    && let Ok(record) = channel.encrypt(&error(message))
  {
    let _ = outgoing.send(record).await;
  }
  result
}

fn unpack(frame: InnerMessage) -> Result<(u32, InnerMessage), String> {
  let InnerMessage::Stream { stream_id, frame } = frame else {
    return Err("Expected an encrypted viewer stream".into());
  };
  Ok((stream_id, *frame))
}

async fn read_body(input: &mut mpsc::Receiver<InnerMessage>) -> Result<Vec<u8>, String> {
  let mut body = Vec::new();
  let mut records = 0;
  while let Some(frame) = input.recv().await {
    match frame {
      InnerMessage::RequestBody { data } => {
        records += 1;
        if records > protocol::MAX_BODY.div_ceil(protocol::CHUNK_SIZE) {
          return Err("Too many request body records".into());
        }
        let bytes = protocol::decode(&data, protocol::CHUNK_SIZE)?;
        if body.len() + bytes.len() > protocol::MAX_BODY {
          return Err("Encrypted request body is too large".into());
        }
        body.extend_from_slice(&bytes);
      }
      InnerMessage::RequestEnd {} => return Ok(body),
      _ => return Err("Expected encrypted request body".into()),
    }
  }
  Err("Encrypted request ended prematurely".into())
}

#[allow(clippy::too_many_arguments)]
async fn respond(
  config: &ConnectorConfig,
  client: &reqwest::Client,
  method: &str,
  path: &str,
  body: Vec<u8>,
  output: &Output,
  pending: &mut mpsc::Receiver<InnerMessage>,
) -> Result<(), String> {
  let window = Semaphore::new(protocol::RESPONSE_WINDOW);
  let forward = async {
    let response = forward_device(config, client, method, path, body).await?;
    if response.status().is_redirection() {
      return Err("Local API redirects are forbidden".into());
    }
    let live = path == "/api/v1/events";
    output
      .send(InnerMessage::Response {
        status: response.status().as_u16(),
        content_type: Some(if live { "text/event-stream" } else { "application/json" }.into()),
      })
      .await?;
    let mut stream = response.bytes_stream();
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
      let chunk = chunk.map_err(|_| "Local API response stream failed")?;
      total = total.saturating_add(chunk.len());
      if !live && total > 128 * 1024 * 1024 {
        return Err("Local API response exceeds 128 MiB".into());
      }
      for part in chunk.chunks(protocol::CHUNK_SIZE) {
        window
          .acquire()
          .await
          .map_err(|_| "Encrypted response cancelled")?
          .forget();
        output
          .send(InnerMessage::Chunk {
            data: protocol::encode(part),
          })
          .await?;
      }
    }
    Ok(())
  };
  let timeout = tokio::time::sleep(Duration::from_secs(120));
  tokio::pin!(forward, timeout);
  loop {
    tokio::select! {
      _ = &mut timeout, if path != "/api/v1/events" =>
        return Err("Encrypted request timed out; delivery may be uncertain and is never retried".into()),
      frame = pending.recv() => match frame {
        Some(InnerMessage::Window { credits }) => window.add_permits(credits),
        _ => return Err("Invalid encrypted response window".into()),
      },
      result = &mut forward => return result,
    }
  }
}
