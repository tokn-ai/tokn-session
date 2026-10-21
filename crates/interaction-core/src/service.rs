use crate::*;
use sha2::{Digest, Sha256};
use std::{
  collections::{BTreeMap, HashMap, HashSet},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
};
use tokio::sync::{broadcast, watch};

const MAX_RUNTIMES: usize = 64;
const MAX_SESSIONS: usize = 1024;
const MAX_INTERACTIONS: usize = 256;

type Outcome = Result<Reply, InteractionError>;
type InteractionKey = (Option<String>, String);
type ShutdownCompletion = watch::Receiver<Option<Vec<InteractionError>>>;

#[derive(Clone, Debug, serde::Serialize)]
pub struct RuntimeSnapshot {
  pub descriptor: RuntimeDescriptor,
  pub connected: bool,
  pub recovery_required: bool,
  /// Recovery coverage exceeded its bound; mutations require a fresh attachment.
  pub reattach_required: bool,
  pub last_sequence: Option<u64>,
  pub states: Vec<SessionState>,
  pub pending_interactions: Vec<PendingInteraction>,
}

struct Cache {
  generation: u64,
  connected: bool,
  recovery_required: bool,
  reattach_required: bool,
  last_sequence: Option<u64>,
  states: BTreeMap<Option<String>, SessionState>,
  pending: BTreeMap<InteractionKey, PendingInteraction>,
  claims: HashMap<InteractionKey, String>,
  required_scopes: HashSet<Option<String>>,
  state_baselines: HashSet<Option<String>>,
  pending_baselines: HashSet<Option<String>>,
}
impl Default for Cache {
  fn default() -> Self {
    Self {
      generation: 0,
      connected: false,
      recovery_required: true,
      reattach_required: false,
      last_sequence: None,
      states: BTreeMap::new(),
      pending: BTreeMap::new(),
      claims: HashMap::new(),
      required_scopes: HashSet::new(),
      state_baselines: HashSet::new(),
      pending_baselines: HashSet::new(),
    }
  }
}
impl Cache {
  fn invalidate(&mut self) {
    self.generation = self.generation.wrapping_add(1);
    self.recovery_required = true;
    for scope in self
      .states
      .keys()
      .chain(self.pending.keys().map(|(session, _)| session))
      .chain(self.claims.keys().map(|(session, _)| session))
    {
      if !self.required_scopes.contains(scope) && self.required_scopes.len() >= MAX_SESSIONS {
        // Truncated recovery coverage cannot safely authorize later mutations.
        self.reattach_required = true;
        break;
      }
      self.required_scopes.insert(scope.clone());
    }
    self.state_baselines.clear();
    self.pending_baselines.clear();
    self.pending.clear();
    for state in self.states.values_mut() {
      state.status = ExecutionState::Unknown;
      state.revision = None;
    }
  }
  fn update_recovery(&mut self) {
    if self.reattach_required {
      self.recovery_required = true;
      return;
    }
    let recovered = |scope: &Option<String>| {
      self.state_baselines.contains(scope)
        && (self.pending_baselines.contains(&None) || self.pending_baselines.contains(scope))
    };
    self.recovery_required = if self.required_scopes.is_empty() {
      !self.state_baselines.iter().any(recovered)
    } else {
      !self.required_scopes.iter().all(recovered)
    };
  }
}

struct RuntimeEntry {
  descriptor: RuntimeDescriptor,
  closing: AtomicBool,
  adapter: Arc<dyn InteractionAdapter>,
  cache: Mutex<Cache>,
  pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for RuntimeEntry {
  fn drop(&mut self) {
    if let Some(pump) = self.pump.get_mut().unwrap_or_else(|error| error.into_inner()).take() {
      pump.abort();
    }
  }
}
struct Admission {
  receipts: ReceiptLedger<Outcome>,
  messages: HashSet<String>,
}
struct Inner {
  stopping: AtomicBool,
  runtimes: Mutex<HashMap<String, Arc<RuntimeEntry>>>,
  admission: Mutex<Admission>,
  events: broadcast::Sender<RuntimeEvent>,
  shutdown: Mutex<Option<ShutdownCompletion>>,
}

/// Host-side orchestration shared by native, CLI, and remote frontends.
/// Register adapters explicitly; inspecting a snapshot never spawns a process.
#[derive(Clone)]
pub struct InteractionService(Arc<Inner>);

impl Default for InteractionService {
  fn default() -> Self {
    Self(Arc::new(Inner {
      stopping: AtomicBool::new(false),
      runtimes: Mutex::new(HashMap::new()),
      admission: Mutex::new(Admission {
        receipts: ReceiptLedger::new(1024),
        messages: HashSet::new(),
      }),
      events: broadcast::channel(256).0,
      shutdown: Mutex::new(None),
    }))
  }
}

impl InteractionService {
  pub fn register(&self, adapter: Arc<dyn InteractionAdapter>) -> Result<RuntimeDescriptor, InteractionError> {
    let descriptor = adapter.descriptor();
    validate_id(&descriptor.runtime_id)?;
    validate_id(&descriptor.instance_id)?;
    let handle = tokio::runtime::Handle::try_current()
      .map_err(|_| InteractionError::unavailable("Register runtimes inside an async runtime"))?;
    let mut events = adapter.subscribe();
    let entry = Arc::new(RuntimeEntry {
      descriptor: descriptor.clone(),
      closing: AtomicBool::new(false),
      adapter,
      cache: Mutex::new(Cache::default()),
      pump: Mutex::new(None),
    });
    {
      let mut runtimes = self.0.runtimes.lock().unwrap_or_else(|error| error.into_inner());
      if self.0.stopping.load(Ordering::Acquire) {
        return Err(InteractionError::unavailable("Interaction service is shutting down"));
      }
      if runtimes.contains_key(&descriptor.runtime_id) {
        return Err(InteractionError::new(
          ErrorKind::Conflict,
          "Detach the existing runtime before replacing it",
        ));
      }
      if runtimes.len() >= MAX_RUNTIMES {
        return Err(InteractionError::unavailable("Runtime capacity reached"));
      }
      runtimes.insert(descriptor.runtime_id.clone(), entry.clone());
    }
    let weak_entry = Arc::downgrade(&entry);
    let output = self.0.events.clone();
    let pump = handle.spawn(async move {
      loop {
        match events.recv().await {
          Ok(event) => {
            let Some(entry) = weak_entry.upgrade() else {
              break;
            };
            if apply_event(&entry, &event) {
              let _ = output.send(event);
            }
          }
          Err(broadcast::error::RecvError::Lagged(_)) => {
            let Some(entry) = weak_entry.upgrade() else {
              break;
            };
            entry
              .cache
              .lock()
              .unwrap_or_else(|error| error.into_inner())
              .invalidate();
          }
          Err(broadcast::error::RecvError::Closed) => {
            if let Some(entry) = weak_entry.upgrade() {
              let mut cache = entry.cache.lock().unwrap_or_else(|error| error.into_inner());
              cache.invalidate();
              cache.connected = false;
            }
            break;
          }
        }
      }
    });
    *entry.pump.lock().unwrap_or_else(|error| error.into_inner()) = Some(pump);
    Ok(descriptor)
  }

  pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
    self.0.events.subscribe()
  }

  pub fn snapshots(&self) -> Vec<RuntimeSnapshot> {
    let entries = self
      .0
      .runtimes
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .values()
      .cloned()
      .collect::<Vec<_>>();
    let mut snapshots = entries.iter().map(snapshot).collect::<Vec<_>>();
    snapshots.sort_by(|a, b| a.descriptor.runtime_id.cmp(&b.descriptor.runtime_id));
    snapshots
  }

  pub fn snapshot(&self, runtime_id: &str) -> Option<RuntimeSnapshot> {
    self
      .0
      .runtimes
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(runtime_id)
      .map(snapshot)
  }

  /// Reserve the runtime ID until its owned child has stopped or its endpoint is detached.
  pub async fn detach(&self, runtime_id: &str, expected_instance_id: &str) -> Result<(), InteractionError> {
    let entry = {
      let runtimes = self.0.runtimes.lock().unwrap_or_else(|error| error.into_inner());
      let entry = runtimes
        .get(runtime_id)
        .ok_or_else(|| InteractionError::unavailable("Unknown runtime"))?;
      if entry.descriptor.instance_id != expected_instance_id {
        return Err(stale());
      }
      if entry.closing.swap(true, Ordering::AcqRel) {
        return Err(InteractionError::new(ErrorKind::Conflict, "Runtime is already closing"));
      }
      entry.clone()
    };
    let service = self.clone();
    let runtime_id = runtime_id.to_owned();
    tokio::spawn(async move {
      if let Some(pump) = entry.pump.lock().unwrap_or_else(|error| error.into_inner()).take() {
        pump.abort();
      }
      let result = tokio::time::timeout(std::time::Duration::from_secs(10), entry.adapter.close())
        .await
        .unwrap_or_else(|_| Err(InteractionError::uncertain("Runtime detach timed out")));
      if result.is_ok() {
        service
          .0
          .runtimes
          .lock()
          .unwrap_or_else(|error| error.into_inner())
          .remove(&runtime_id);
      }
      result
    })
    .await
    .unwrap_or_else(|_| Err(InteractionError::uncertain("Runtime detach was interrupted")))
  }

  pub async fn execute(&self, request: InteractionRequest) -> Outcome {
    validate_id(&request.request_id)?;
    validate_id(&request.target.runtime_id)?;
    validate_id(&request.target.instance_id)?;
    if let Some(id) = &request.target.session_id {
      validate_id(id)?;
    }
    let entry = self
      .0
      .runtimes
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(&request.target.runtime_id)
      .cloned()
      .ok_or_else(|| InteractionError::unavailable("Unknown runtime"))?;
    if entry.descriptor.instance_id != request.target.instance_id {
      return Err(stale());
    }
    if request.command.is_mutation() {
      let scope =
        serde_json::to_string(&request.target).map_err(|_| InteractionError::invalid("Invalid runtime target"))?;
      let fingerprint: [u8; 32] =
        Sha256::digest(serde_json::to_vec(&request.command).map_err(|_| InteractionError::invalid("Invalid command"))?)
          .into();
      if let Some(response) = self
        .0
        .admission
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .receipts
        .lookup(&request.request_id, &scope, &fingerprint)
        .map_err(receipt_error)?
      {
        return response;
      }
    }
    if self.0.stopping.load(Ordering::Acquire) || entry.closing.load(Ordering::Acquire) {
      return Err(InteractionError::unavailable("Runtime is closing"));
    }
    let descriptor = current_descriptor(&entry)?;
    let operation = request.command.operation();
    if !descriptor.capabilities.supports(operation) {
      let reason = descriptor
        .capabilities
        .operations
        .get(&operation)
        .and_then(|capability| capability.reason.clone())
        .unwrap_or_else(|| format!("{operation:?} is unavailable on this runtime"));
      return Err(InteractionError::unsupported(reason));
    }
    if let Command::Message { input, .. } = &request.command {
      if input.text.trim().is_empty() && input.images.is_empty() {
        return Err(InteractionError::invalid("Message cannot be empty"));
      }
      if input.model.is_some() && !descriptor.capabilities.atomic_model_on_send {
        return Err(InteractionError::unsupported(
          "This runtime cannot atomically apply model intent only when starting a turn",
        ));
      }
    }
    if !request.command.is_mutation() {
      let generation = entry.cache.lock().unwrap_or_else(|error| error.into_inner()).generation;
      let reply = entry.adapter.execute(request.clone()).await?;
      validate_reply(&request, &reply)?;
      let mut cache = entry.cache.lock().unwrap_or_else(|error| error.into_inner());
      if matches!(reply, Reply::State(_) | Reply::Interactions(_)) && cache.generation != generation {
        return Err(InteractionError::unavailable(
          "Live state advanced during the query; refresh its baseline",
        ));
      }
      apply_reply(&mut cache, &request, &reply);
      return Ok(reply);
    }
    if request.target.session_id.is_none() {
      return Err(InteractionError::invalid(
        "Mutations require an explicit native session ID",
      ));
    }
    let scope =
      serde_json::to_string(&request.target).map_err(|_| InteractionError::invalid("Invalid runtime target"))?;
    let fingerprint: [u8; 32] =
      Sha256::digest(serde_json::to_vec(&request.command).map_err(|_| InteractionError::invalid("Invalid command"))?)
        .into();
    let message = matches!(request.command, Command::Message { .. });
    {
      let mut admission = self.0.admission.lock().unwrap_or_else(|error| error.into_inner());
      if let Some(response) = admission
        .receipts
        .lookup(&request.request_id, &scope, &fingerprint)
        .map_err(receipt_error)?
      {
        return response;
      }
      if entry
        .cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .reattach_required
      {
        return Err(InteractionError::unavailable(
          "Runtime recovery exceeded its session bound; detach and attach again before writing",
        ));
      }
      if message && admission.messages.contains(&scope) {
        return Err(InteractionError::new(
          ErrorKind::Conflict,
          "A message is already awaiting native admission",
        ));
      }
      if let Command::Respond { interaction_id, .. } = &request.command {
        validate_id(interaction_id)?;
        let cache = entry.cache.lock().unwrap_or_else(|error| error.into_inner());
        let key = (request.target.session_id.clone(), interaction_id.clone());
        if !cache.pending.contains_key(&key) {
          return Err(InteractionError::new(
            ErrorKind::StaleTarget,
            "This interaction is no longer known to be pending; refresh live requests",
          ));
        }
        if cache.claims.contains_key(&key) {
          return Err(InteractionError::new(
            ErrorKind::Conflict,
            "This interaction already has an answer submission",
          ));
        }
      }
      admission
        .receipts
        .admit(
          request.request_id.clone(),
          scope.clone(),
          fingerprint,
          Ok(Reply::Pending),
        )
        .map_err(receipt_error)?;
      if let Command::Respond {
        interaction_id,
        response,
      } = &request.command
      {
        let mut cache = entry.cache.lock().unwrap_or_else(|error| error.into_inner());
        let key = (request.target.session_id.clone(), interaction_id.clone());
        let validation = cache
          .pending
          .get(&key)
          .ok_or_else(|| InteractionError::new(ErrorKind::StaleTarget, "Interaction ended before response admission"))
          .and_then(|pending| validate_response(pending, response));
        if let Err(error) = validation {
          admission.receipts.complete(&request.request_id, Err(error.clone()));
          return Err(error);
        }
        cache.claims.insert(key, request.request_id.clone());
      }
      if message {
        admission.messages.insert(scope.clone());
      }
    }
    let service = self.clone();
    // Admission survives a caller's disconnect. No uncertain operation is replayed.
    tokio::spawn(async move {
      let adapter = entry.adapter.clone();
      let mutation = request.clone();
      let result = tokio::spawn(async move { adapter.execute(mutation).await })
        .await
        .unwrap_or_else(|_| {
          Err(InteractionError::uncertain(
            "Native operation was interrupted; inspect state before another attempt",
          ))
        });
      let result = result.and_then(|reply| {
        validate_reply(&request, &reply)?;
        Ok(reply)
      });
      match &result {
        Ok(reply) => apply_reply(
          &mut entry.cache.lock().unwrap_or_else(|error| error.into_inner()),
          &request,
          reply,
        ),
        Err(error) if error.kind != ErrorKind::Uncertain => {
          if let Command::Respond { interaction_id, .. } = &request.command {
            entry
              .cache
              .lock()
              .unwrap_or_else(|error| error.into_inner())
              .claims
              .remove(&(request.target.session_id.clone(), interaction_id.clone()));
          }
        }
        _ => {}
      }
      let mut admission = service.0.admission.lock().unwrap_or_else(|error| error.into_inner());
      if message {
        admission.messages.remove(&scope);
      }
      if result.as_ref().is_err_and(|error| error.kind == ErrorKind::Uncertain) {
        admission.receipts.retain_outcome(&request.request_id, result.clone());
      } else {
        admission.receipts.complete(&request.request_id, result.clone());
      }
      result
    })
    .await
    .unwrap_or_else(|_| Err(InteractionError::uncertain("Operation admission was interrupted")))
  }

  /// Stop owned children and detach external endpoints. Call before the host runtime exits.
  /// Cleanup survives cancellation; concurrent and later callers await the same result.
  pub async fn shutdown(&self) -> Vec<InteractionError> {
    let mut completion = {
      let mut shutdown = self.0.shutdown.lock().unwrap_or_else(|error| error.into_inner());
      shutdown
        .get_or_insert_with(|| {
          self.0.stopping.store(true, Ordering::Release);
          let (sender, receiver) = watch::channel(None);
          let service = self.clone();
          tokio::spawn(async move {
            let errors = service.close_all().await;
            let _ = sender.send(Some(errors));
          });
          receiver
        })
        .clone()
    };
    loop {
      if let Some(errors) = completion.borrow_and_update().clone() {
        return errors;
      }
      if completion.changed().await.is_err() {
        return vec![InteractionError::uncertain("Runtime shutdown was interrupted")];
      }
    }
  }

  async fn close_all(&self) -> Vec<InteractionError> {
    self.0.stopping.store(true, Ordering::Release);
    let entries = self
      .0
      .runtimes
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .drain()
      .map(|(_, entry)| entry)
      .collect::<Vec<_>>();
    let mut closing = tokio::task::JoinSet::new();
    for entry in entries {
      entry.closing.store(true, Ordering::Release);
      if let Some(pump) = entry.pump.lock().unwrap_or_else(|error| error.into_inner()).take() {
        pump.abort();
      }
      closing.spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(10), entry.adapter.close())
          .await
          .unwrap_or_else(|_| Err(InteractionError::uncertain("Runtime shutdown timed out")))
      });
    }
    let mut errors = Vec::new();
    while let Some(result) = closing.join_next().await {
      match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => errors.push(error),
        Err(_) => errors.push(InteractionError::uncertain("Runtime shutdown was interrupted")),
      }
    }
    errors
  }
}

fn current_descriptor(entry: &RuntimeEntry) -> Result<RuntimeDescriptor, InteractionError> {
  let descriptor = entry.adapter.descriptor();
  if descriptor.runtime_id != entry.descriptor.runtime_id
    || descriptor.instance_id != entry.descriptor.instance_id
    || descriptor.provider != entry.descriptor.provider
    || descriptor.ownership != entry.descriptor.ownership
  {
    return Err(stale());
  }
  Ok(descriptor)
}

fn validate_reply(request: &InteractionRequest, reply: &Reply) -> Result<(), InteractionError> {
  let matches = match (&request.command, reply) {
    (Command::Metadata, Reply::Metadata(_)) | (Command::Models, Reply::Models(_)) => true,
    (Command::State, Reply::State(state)) => {
      request.target.session_id.is_none() || state.session_id == request.target.session_id
    }
    (Command::PendingInteractions, Reply::Interactions(items)) => items.iter().all(|item| {
      validate_id(&item.interaction_id).is_ok()
        && (request.target.session_id.is_none() || item.session_id == request.target.session_id)
    }),
    (Command::Message { .. }, Reply::Admission(_))
    | (Command::Respond { .. } | Command::CancelTurn, Reply::Acknowledged) => true,
    _ => false,
  };
  if matches {
    Ok(())
  } else if request.command.is_mutation() {
    Err(InteractionError::uncertain(
      "Native reply did not match the admitted operation; inspect state before retrying",
    ))
  } else {
    Err(InteractionError::unavailable(
      "Native reply did not match the queried session or operation",
    ))
  }
}

fn snapshot(entry: &Arc<RuntimeEntry>) -> RuntimeSnapshot {
  let cache = entry.cache.lock().unwrap_or_else(|error| error.into_inner());
  RuntimeSnapshot {
    descriptor: current_descriptor(entry).unwrap_or_else(|_| entry.descriptor.clone()),
    connected: cache.connected,
    recovery_required: cache.recovery_required,
    reattach_required: cache.reattach_required,
    last_sequence: cache.last_sequence,
    states: cache.states.values().cloned().collect(),
    pending_interactions: cache.pending.values().cloned().collect(),
  }
}

fn apply_event(entry: &RuntimeEntry, event: &RuntimeEvent) -> bool {
  if event.runtime_id != entry.descriptor.runtime_id || event.instance_id != entry.descriptor.instance_id {
    return false;
  }
  match &event.kind {
    RuntimeEventKind::State(state) if state.session_id != event.session_id => return false,
    RuntimeEventKind::InteractionRequested(item)
      if item.session_id != event.session_id || validate_id(&item.interaction_id).is_err() =>
    {
      return false;
    }
    _ => {}
  }
  let mut cache = entry.cache.lock().unwrap_or_else(|error| error.into_inner());
  if cache.last_sequence.is_some_and(|last| event.sequence <= last) {
    return false;
  }
  if cache
    .last_sequence
    .is_some_and(|last| event.sequence > last.saturating_add(1))
  {
    cache.invalidate();
  }
  if !matches!(event.kind, RuntimeEventKind::Native(_)) {
    cache.generation = cache.generation.wrapping_add(1);
  }
  cache.last_sequence = Some(event.sequence);
  if cache.reattach_required {
    return false;
  }
  match &event.kind {
    RuntimeEventKind::State(state) => {
      if cache.states.len() < MAX_SESSIONS || cache.states.contains_key(&state.session_id) {
        cache.states.insert(state.session_id.clone(), state.clone());
      }
      cache.connected = true;
    }
    RuntimeEventKind::InteractionRequested(interaction) => {
      let key = (interaction.session_id.clone(), interaction.interaction_id.clone());
      if cache.pending.len() < MAX_INTERACTIONS || cache.pending.contains_key(&key) {
        cache.pending.insert(key, interaction.clone());
        cache.connected = true;
      } else {
        cache.invalidate();
      }
    }
    RuntimeEventKind::InteractionResolved { interaction_id } => {
      let key = (event.session_id.clone(), interaction_id.clone());
      cache.pending.remove(&key);
      cache.claims.remove(&key);
    }
    RuntimeEventKind::Disconnected { .. } => {
      cache.invalidate();
      cache.connected = false;
    }
    RuntimeEventKind::Native(_) => {}
  }
  true
}

fn apply_reply(cache: &mut Cache, request: &InteractionRequest, reply: &Reply) {
  if cache.reattach_required {
    return;
  }
  if matches!(reply, Reply::State(_) | Reply::Interactions(_))
    || matches!(
      (&request.command, reply),
      (Command::Respond { .. }, Reply::Acknowledged)
    )
  {
    cache.generation = cache.generation.wrapping_add(1);
  }
  match reply {
    Reply::State(state) => {
      if cache.states.len() < MAX_SESSIONS || cache.states.contains_key(&state.session_id) {
        cache.states.insert(state.session_id.clone(), state.clone());
        cache.state_baselines.insert(state.session_id.clone());
      }
      cache.update_recovery();
      cache.connected = true;
    }
    Reply::Interactions(interactions) => {
      let in_scope =
        |session: &Option<String>| request.target.session_id.is_none() || session == &request.target.session_id;
      cache.pending.retain(|(session, _), _| !in_scope(session));
      let available = MAX_INTERACTIONS.saturating_sub(cache.pending.len());
      for interaction in interactions.iter().take(available) {
        cache.pending.insert(
          (interaction.session_id.clone(), interaction.interaction_id.clone()),
          interaction.clone(),
        );
      }
      let pending_keys = cache.pending.keys().cloned().collect::<HashSet<_>>();
      cache
        .claims
        .retain(|(session, id), _| !in_scope(session) || pending_keys.contains(&(session.clone(), id.clone())));
      // Recovery requires state and pending-request baselines for every previously known session.
      if interactions.len() <= available && cache.pending_baselines.len() < MAX_SESSIONS {
        cache.pending_baselines.insert(request.target.session_id.clone());
        cache.update_recovery();
      } else {
        cache.invalidate();
      }
      cache.connected = true;
    }
    Reply::Acknowledged if matches!(request.command, Command::Respond { .. }) => {
      if let Command::Respond { interaction_id, .. } = &request.command {
        let key = (request.target.session_id.clone(), interaction_id.clone());
        cache.pending.remove(&key);
        cache.claims.remove(&key);
      }
    }
    _ => {}
  }
}

fn validate_response(pending: &PendingInteraction, response: &InteractionResponse) -> Result<(), InteractionError> {
  match response {
    InteractionResponse::Reject => Ok(()),
    InteractionResponse::Permission { .. } if pending.kind == InteractionKind::Permission => Ok(()),
    InteractionResponse::Dialog { .. }
      if matches!(
        pending.kind,
        InteractionKind::Confirm | InteractionKind::Select | InteractionKind::Input | InteractionKind::Editor
      ) =>
    {
      Ok(())
    }
    InteractionResponse::Answers { answers } if pending.kind == InteractionKind::Question => {
      let mut seen = HashSet::new();
      for answer in answers {
        let Some(question) = pending
          .questions
          .iter()
          .find(|question| question.question_id == answer.question_id)
        else {
          return Err(InteractionError::invalid("Unknown question in answer batch"));
        };
        if !seen.insert(&answer.question_id)
          || answer.answers.is_empty() && answer.custom.as_deref().is_none_or(|value| value.trim().is_empty())
        {
          return Err(InteractionError::invalid("Duplicate or empty question answer"));
        }
        if answer.custom.is_some() && !question.allows_free_text {
          return Err(InteractionError::invalid("This question does not allow free text"));
        }
        let choices = answer.answers.iter().collect::<HashSet<_>>();
        if choices.len() != answer.answers.len()
          || !question.multiple && choices.len() + usize::from(answer.custom.is_some()) > 1
        {
          return Err(InteractionError::invalid("Invalid number of question choices"));
        }
        if answer
          .answers
          .iter()
          .any(|answer| !question.options.iter().any(|option| &option.label == answer))
        {
          return Err(InteractionError::invalid(
            "Answer contains an unknown option; use the explicit custom field for free text",
          ));
        }
      }
      if seen.len() != pending.questions.len() {
        return Err(InteractionError::invalid("Answer the complete question batch"));
      }
      Ok(())
    }
    _ => Err(InteractionError::invalid(
      "Response kind does not match the live interaction",
    )),
  }
}

fn validate_id(id: &str) -> Result<(), InteractionError> {
  if id.is_empty() || id.len() > 512 || id.trim() != id || id.chars().any(char::is_control) {
    return Err(InteractionError::invalid("Invalid interaction identity"));
  }
  Ok(())
}
fn stale() -> InteractionError {
  InteractionError::new(ErrorKind::StaleTarget, "Runtime instance changed; refresh its binding")
}
fn receipt_error(error: ReceiptError) -> InteractionError {
  match error {
    ReceiptError::Conflict => InteractionError::new(
      ErrorKind::Conflict,
      "Request ID was already used with another target or command",
    ),
    ReceiptError::Full => InteractionError::unavailable("Too many interaction requests are pending"),
  }
}

#[cfg(test)]
mod tests;
