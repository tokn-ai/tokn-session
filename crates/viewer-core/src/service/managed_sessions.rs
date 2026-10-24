use super::*;
use crate::workspace::{
  CreateProjectSessionRequest, ManagedSessionDialogResponse, ManagedSessionRequest, ManagedSessionSnapshot,
  SessionCreationError,
};

impl ViewerService {
  pub(super) fn check_managed_workspace(&self) -> Result<(), String> {
    if self.session_scope.is_some() {
      return Err("Managed sessions are unavailable in a session share".into());
    }
    if self.relay.status().settings.mode == crate::relay::RelayMode::External {
      return Err("Create and manage sessions on this machine's local connection.".into());
    }
    Ok(())
  }

  pub async fn create_project_session(
    &self,
    request: CreateProjectSessionRequest,
  ) -> Result<ManagedSessionSnapshot, SessionCreationError> {
    self
      .check_managed_workspace()
      .map_err(SessionCreationError::not_started)?;
    tokn_interaction_core::managed_sessions::validate_creation_request(&request.pi_request())
      .map_err(SessionCreationError::not_started)?;
    if uuid::Uuid::parse_str(&request.request_id)
      .map(|id| id.to_string())
      .as_deref()
      != Ok(request.request_id.as_str())
    {
      return Err(SessionCreationError::not_started(
        "Use a canonical UUID for the creation request ID",
      ));
    }
    let _admission = self.sessions.creation_gate(&request.request_id).await;
    if request.provider == tokn_interaction_core::Provider::Pi {
      if let Ok(manager) = self.creation_manager() {
        if manager
          .get(&request.request_id)
          .map_err(SessionCreationError::not_started)?
          .is_some()
        {
          return Err(SessionCreationError::not_started(
            "This creation request ID belongs to another agent",
          ));
        }
      }
    } else if self
      .sessions
      .managed
      .get(&ManagedSessionRequest {
        request_id: Some(request.request_id.clone()),
        session_key: None,
      })
      .map_err(SessionCreationError::not_started)?
      .is_some()
    {
      return Err(SessionCreationError::not_started(
        "This creation request ID belongs to a Pi session",
      ));
    }
    if request.provider != tokn_interaction_core::Provider::Pi {
      if let Some(snapshot) = self.existing_rpc_creation(&request).await? {
        return Ok(snapshot);
      }
    } else {
      if request.runtime_profile_id.is_some() || request.runtime_revision.is_some() {
        return Err(SessionCreationError::not_started(
          "Pi uses its managed-session configuration; choose Automatic",
        ));
      }
      self
        .sessions
        .managed
        .available()
        .map_err(SessionCreationError::not_started)?;
    }
    if let Some(snapshot) = (request.provider == tokn_interaction_core::Provider::Pi)
      .then(|| self.sessions.managed.creation_receipt(&request))
      .transpose()
      .map_err(SessionCreationError::uncertain)?
      .flatten()
    {
      self
        .claim_creation_route(&request)
        .await
        .map_err(SessionCreationError::not_started)?;
      return self
        .enrich_managed_session(snapshot)
        .await
        .map_err(SessionCreationError::uncertain);
    }
    let service = self.clone();
    let project_id = request.project_id.clone();
    let project = tokio::task::spawn_blocking(move || service.validated_project(&project_id))
      .await
      .map_err(|_| SessionCreationError::not_started("Could not validate the project folder"))?
      .map_err(SessionCreationError::not_started)?;
    if request.provider != tokn_interaction_core::Provider::Pi {
      self
        .preflight_rpc_creation(&request, &project)
        .await
        .map_err(SessionCreationError::not_started)?;
    }
    let images = if request.image_ids.is_empty() {
      Vec::new()
    } else {
      let store = self.images.clone();
      let target = crate::images::ImageTarget::Project {
        project_id: request.project_id.clone(),
      };
      let ids = request.image_ids.clone();
      tokio::task::spawn_blocking(move || {
        // Managed Pi embeds bytes in its RPC prompt; the creation task owns
        // those bytes and does not require a permanent host file.
        store.resolve(&target, &ids)
      })
      .await
      .map_err(|_| SessionCreationError::not_started("Could not load image attachments"))?
      .map_err(SessionCreationError::not_started)?
    };
    self
      .claim_creation_route(&request)
      .await
      .map_err(SessionCreationError::not_started)?;
    if request.provider != tokn_interaction_core::Provider::Pi {
      return self.start_rpc_creation(request, project, images).await;
    }
    let snapshot = self
      .sessions
      .managed
      .create_with_images(request, project, images)
      .await
      .map_err(SessionCreationError::uncertain)?;
    self
      .enrich_managed_session(snapshot)
      .await
      .map_err(SessionCreationError::uncertain)
  }

  pub async fn get_managed_session(
    &self,
    request: ManagedSessionRequest,
  ) -> Result<Option<ManagedSessionSnapshot>, String> {
    self.check_managed_workspace()?;
    if let Some(receipt) = self.rpc_creation(&request)? {
      let snapshot = self
        .creation_manager()?
        .refresh(&receipt.request_id)
        .await?
        .unwrap_or(receipt);
      return self.creation_snapshot(snapshot).map(Some);
    }
    match self.sessions.managed.get(&request)? {
      Some(snapshot) => self.enrich_managed_session(snapshot).await.map(Some),
      None => Ok(None),
    }
  }

  pub async fn stop_managed_session(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    self.check_managed_workspace()?;
    if let Some(receipt) = self.rpc_creation(&request)? {
      let snapshot = self
        .creation_manager()?
        .cancel(&receipt.request_id)
        .await?
        .ok_or("Creation receipt disappeared")?;
      return self.creation_snapshot(snapshot);
    }
    let snapshot = self.sessions.managed.stop(request).await?;
    self.enrich_managed_session(snapshot).await
  }

  pub async fn resume_managed_session(&self, request: ManagedSessionRequest) -> Result<ManagedSessionSnapshot, String> {
    self.check_managed_workspace()?;
    if let Some(receipt) = self.rpc_creation(&request)? {
      return self.creation_snapshot(receipt);
    }
    let previous = self
      .sessions
      .managed
      .get(&request)?
      .ok_or("This session is not managed by the viewer")?;
    let service = self.clone();
    tokio::task::spawn_blocking(move || service.validated_project(&previous.project_id))
      .await
      .map_err(|_| "Could not validate the project folder")??;
    let snapshot = self.sessions.managed.resume(request).await?;
    self.enrich_managed_session(snapshot).await
  }

  pub async fn respond_managed_session_dialog(
    &self,
    response: ManagedSessionDialogResponse,
  ) -> Result<ManagedSessionSnapshot, String> {
    self.check_managed_workspace()?;
    let snapshot = self.sessions.managed.respond_dialog(response).await?;
    self.enrich_managed_session(snapshot).await
  }

  pub async fn shutdown_managed_sessions(&self) {
    self.sessions.shutdown().await;
  }

  /// Pi owns the file and identity. Register only that verified source rather
  /// than rescanning unrelated providers when its first response is persisted.
  pub(crate) async fn enrich_managed_session(
    &self,
    mut snapshot: ManagedSessionSnapshot,
  ) -> Result<ManagedSessionSnapshot, String> {
    let Some(key) = snapshot.session_key.clone() else {
      return Ok(snapshot);
    };
    let service = self.clone();
    let expected_id = snapshot.session_id.clone();
    let project_id = snapshot.project_id.clone();
    snapshot.session =
      tokio::task::spawn_blocking(move || service.index_managed_pi_session(&key, expected_id.as_deref(), &project_id))
        .await
        .map_err(|_| "Could not register the new Pi session")??;
    Ok(snapshot)
  }

  fn index_managed_pi_session(
    &self,
    key: &str,
    expected_id: Option<&str>,
    project_id: &str,
  ) -> Result<Option<SessionSummary>, String> {
    let locator = decode_session_key(key)?;
    if locator.provider != ViewerProvider::Pi || expected_id != Some(locator.session_id.as_str()) {
      return Err("The managed Pi session identity does not match its history".into());
    }
    let project = self
      .session_index
      .get_registered_project(project_id)
      .map_err(|error| error.to_string())?
      .ok_or("The session's registered project is unavailable")?;
    let _gate = self
      .index_refresh_gate
      .lock()
      .map_err(|_| "Session index is unavailable")?;
    let source_key = index_source_key_for_path(ViewerProvider::Pi, &locator.source_path)?;
    let session_key = index_session_key(&locator)?;
    // Capture the optimistic checkpoint before provider I/O. Reading it
    // afterwards could treat a concurrent winner as our own precondition.
    let previous = self
      .session_index
      .source_state(&source_key)
      .map_err(|error| error.to_string())?;
    let existing = self
      .session_index
      .session(&session_key)
      .map_err(|error| error.to_string())?;
    if previous
      .as_ref()
      .is_some_and(|state| state.cursor != INDEX_MISSING_SOURCE_CURSOR)
    {
      let member = self
        .session_index
        .unique_present_session_for_source(&source_key)
        .map_err(|error| error.to_string())?;
      if member.as_ref().is_none_or(|row| row.key != session_key) {
        return Err("Pi's source path already belongs to a different session identity".into());
      }
    }
    let before = match source_cursor(ViewerProvider::Pi, &locator.source_path) {
      Ok(cursor) => cursor,
      Err(_) => return Ok(None),
    };
    let header = self
      .repository
      .session_header_at_path(ViewerProvider::Pi, &locator.source_path)?;
    if header.id != locator.session_id || header.path != locator.source_path {
      return Err("Pi's history file no longer matches the created session".into());
    }
    let cwd = header
      .cwd
      .as_deref()
      .ok_or("Pi's session does not identify its project folder")?;
    if Path::new(cwd).canonicalize().unwrap_or_else(|_| PathBuf::from(cwd)) != Path::new(&project.folder_path) {
      return Err("Pi's session belongs to a different project folder".into());
    }
    if source_cursor(ViewerProvider::Pi, &locator.source_path)? != before {
      return Ok(None);
    }
    let source_changed = previous
      .as_ref()
      .and_then(|state| indexed_source_raw_cursor(&state.cursor))
      != Some(before.as_str());
    if self
      .session_index
      .has_other_present_session_source(&session_key)
      .map_err(|error| error.to_string())?
    {
      return Err("Another Pi history file claims this session identity".into());
    }
    if source_changed || existing.as_ref().is_none_or(|row| !row.present) {
      let ready = self
        .session_index
        .source_state(&index_catalog_source_key(ViewerProvider::Pi))
        .map_err(|error| error.to_string())?
        .is_some();
      let metadata = catalog_session_metadata(&source_key, header.clone(), existing.as_ref(), ready, true)?;
      let replacement = SourceReplacement::new(
        SourceState::new(source_key, pending_body_cursor(&before), current_time_ms()),
        vec![metadata],
      )
      .with_source_cursor_precondition(source_cursor_precondition(previous.as_ref()));
      match self.session_index.replace_sources(&[replacement]) {
        Ok(_) => {}
        Err(SessionIndexError::SourceCursorConflict { .. }) => return Ok(None),
        Err(error) => return Err(format!("Could not index the created Pi session: {error}")),
      }
      if let Some(sender) = self
        .index_retry_sender
        .lock()
        .map_err(|_| "Session scheduler is unavailable")?
        .as_ref()
      {
        let _ = sender.send(SessionIndexWake::ChangedFiles(BTreeMap::from([(
          ViewerProvider::Pi,
          BTreeSet::from([locator.source_path.clone()]),
        )])));
      }
    }
    self
      .session_index
      .admit_managed_session(&session_key, project_id)
      .map_err(|error| error.to_string())?;
    let indexed = self
      .session_index
      .session(&session_key)
      .map_err(|error| error.to_string())?;
    let attention = indexed.as_ref().map(SessionAttention::from_index).unwrap_or_default();
    let mut summary = session_summary_with_child_count(ViewerProvider::Pi, header, 0, false, attention)?;
    let groups = self
      .session_index
      .project_groups(&[project.folder_path], current_time_ms())
      .map_err(|error| error.to_string())?;
    apply_project_group(&mut summary, &groups);
    Ok(Some(summary))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::workspace::RegisterProjectRequest;

  fn fixture() -> (
    tempfile::TempDir,
    ViewerService,
    crate::workspace::RegisteredProject,
    SessionLocator,
  ) {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("project");
    std::fs::create_dir(&folder).unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let project = service
      .register_project(RegisterProjectRequest {
        folder_path: folder.to_str().unwrap().into(),
        name: None,
      })
      .unwrap();
    let path = directory.path().join("created.jsonl");
    std::fs::write(
      &path,
      format!(
        "{}\n",
        json!({
          "type":"session", "version":3, "id":"created-pi-session",
          "timestamp":"2026-10-19T00:00:00Z", "cwd":project.folder_path,
        })
      ),
    )
    .unwrap();
    let locator = SessionLocator {
      version: 1,
      provider: ViewerProvider::Pi,
      session_id: "created-pi-session".into(),
      source_path: path,
    };
    (directory, service, project, locator)
  }

  #[test]
  fn created_session_binding_reads_one_header_and_preserves_a_duplicate_revision() {
    let (_directory, service, project, locator) = fixture();
    let key = encode_session_key(&locator).unwrap();
    let summary = service
      .index_managed_pi_session(&key, Some(&locator.session_id), &project.project_id)
      .unwrap()
      .unwrap();
    assert_eq!(summary.session_key, key);
    assert_eq!(summary.project_key.as_deref(), Some(project.group_key.as_str()));
    let first = service
      .session_index
      .session(&index_session_key(&locator).unwrap())
      .unwrap()
      .unwrap();
    assert!(!first.attention_baselined);
    assert_eq!(service.session_index.list_present_sessions().unwrap().len(), 1);
    // Explicit creation proves this record without claiming complete provider
    // discovery or walking any unrelated provider history.
    assert!(
      service
        .session_index
        .source_state(&index_catalog_source_key(ViewerProvider::Pi))
        .unwrap()
        .is_none()
    );
    service
      .index_managed_pi_session(&key, Some(&locator.session_id), &project.project_id)
      .unwrap()
      .unwrap();
    let second = service
      .session_index
      .session(&index_session_key(&locator).unwrap())
      .unwrap()
      .unwrap();
    assert_eq!(first.source_generation, second.source_generation);
    assert_eq!(first.source_cursor, second.source_cursor);
  }

  #[test]
  fn cold_managed_file_appends_do_not_request_a_complete_catalog() {
    use std::io::Write;
    let (_directory, service, project, locator) = fixture();
    let key = encode_session_key(&locator).unwrap();
    service
      .index_managed_pi_session(&key, Some(&locator.session_id), &project.project_id)
      .unwrap()
      .unwrap();
    std::fs::OpenOptions::new()
      .append(true)
      .open(&locator.source_path)
      .unwrap()
      .write_all(b"\n")
      .unwrap();
    let refresh = service
      .refresh_changed_file_catalogs(BTreeMap::from([(
        ViewerProvider::Pi,
        BTreeSet::from([locator.source_path.clone()]),
      )]))
      .unwrap();
    assert!(refresh.changed);
    assert!(!refresh.retry_catalog_soon);
    assert!(!refresh.background_reconciliation_required);
    assert!(
      service
        .session_index
        .has_managed_session_admission(&index_session_key(&locator).unwrap())
        .unwrap()
    );
    assert!(
      service
        .session_index
        .source_state(&index_catalog_source_key(ViewerProvider::Pi))
        .unwrap()
        .is_none()
    );
  }

  #[tokio::test]
  async fn a_cold_created_session_opens_through_the_existing_snapshot_reader() {
    let (_directory, service, project, locator) = fixture();
    let key = encode_session_key(&locator).unwrap();
    let reader = service.clone();
    let expected = key.clone();
    tokio::task::spawn_blocking(move || {
      reader
        .index_managed_pi_session(&key, Some(&locator.session_id), &project.project_id)
        .unwrap()
        .unwrap();
      let request = serde_json::from_value::<crate::updates::SessionUpdatesRequest>(json!({
        "subscription_id":"created-pi", "session_key":key, "level":"steps",
      }))
      .unwrap();
      reader.subscribe_session(request.clone()).unwrap();
      let page = reader.load_session_backward(request).unwrap();
      assert_eq!(page.session_key, expected);
      assert!(page.snapshot);
    })
    .await
    .unwrap();
    service.relay.shutdown().await;
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn owned_creation_input_stop_and_restart_send_share_one_real_identity() {
    use crate::model::{InputDeliveryStatus, SessionInputStatusRequest, SubmitSessionInputRequest};
    use crate::workspace::ManagedSessionStatus;
    use std::os::unix::fs::PermissionsExt;

    let (directory, mut service, project, _) = fixture();
    let executable = directory.path().join("synthetic-pi");
    // Exercise service routing and persistence without importing Pi or making
    // a model request. This process writes only its own temporary native file.
    std::fs::write(&executable, r#"#!/usr/bin/env python3
import json, pathlib, sys
folder = pathlib.Path.cwd()
history = folder / 'owned-history.jsonl'
active = False
for line in sys.stdin:
    command = json.loads(line)
    kind = command['type']
    if kind == 'prompt':
        if not history.exists():
            history.write_text(json.dumps({'type':'session', 'version':3, 'id':'owned-pi', 'timestamp':'2026-10-19T00:00:00Z', 'cwd':str(folder)}) + '\n')
        with (folder / 'prompts.jsonl').open('a') as log:
            log.write(json.dumps(command['message']) + '\n')
        active = True
    elif kind == 'abort':
        active = False
        print(json.dumps({'type':'agent_settled'}), flush=True)
    response = {'type':'response', 'id':command['id'], 'command':kind, 'success':True}
    if kind == 'get_state':
        response['data'] = {'sessionId':'owned-pi', 'sessionFile':str(history), 'isStreaming':active, 'isCompacting':False, 'pendingMessageCount':0}
    print(json.dumps(response), flush=True)
    if kind == 'prompt':
        print(json.dumps({'type':'agent_start'}), flush=True)
"#).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let receipts = directory.path().join("viewer/owned-test.json");
    service.sessions.managed =
      crate::managed_sessions::ManagedSessions::with_test_executable(Some(receipts.clone()), executable.clone())
        .unwrap();
    let creation = CreateProjectSessionRequest {
      provider: tokn_interaction_core::Provider::Pi,
      runtime_profile_id: None,
      runtime_revision: None,
      request_id: uuid::Uuid::new_v4().to_string(),
      project_id: project.project_id,
      prompt: "first prompt".into(),
      image_ids: Vec::new(),
    };
    service.create_project_session(creation.clone()).await.unwrap();
    let identity = ManagedSessionRequest {
      request_id: Some(creation.request_id.clone()),
      session_key: None,
    };
    let created = tokio::time::timeout(std::time::Duration::from_secs(8), async {
      loop {
        let snapshot = service.get_managed_session(identity.clone()).await.unwrap().unwrap();
        if snapshot.session.is_some() {
          break snapshot;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
      }
    })
    .await
    .unwrap();
    let key = created.session_key.unwrap();
    assert_eq!(created.session.unwrap().session_key, key);
    let status_request = SessionInputStatusRequest {
      session_key: key.clone(),
    };
    assert!(
      service
        .get_session_input_status(status_request.clone())
        .await
        .unwrap()
        .available
    );
    let follow_up = SubmitSessionInputRequest {
      model_selection: None,
      session_key: key.clone(),
      request_id: uuid::Uuid::new_v4().to_string(),
      text: "follow-up".into(),
      image_ids: Vec::new(),
    };
    for _ in 0..2 {
      assert_eq!(
        service.submit_session_input(follow_up.clone()).await.unwrap().status,
        InputDeliveryStatus::Accepted
      );
    }
    let prompts = std::fs::read_to_string(Path::new(&project.folder_path).join("prompts.jsonl")).unwrap();
    assert_eq!(prompts.lines().count(), 2, "a retried follow-up must be delivered once");
    service.stop_managed_session(identity.clone()).await.unwrap();
    assert_eq!(
      service
        .get_managed_session(identity.clone())
        .await
        .unwrap()
        .unwrap()
        .status,
      ManagedSessionStatus::Idle
    );
    service.shutdown_managed_sessions().await;
    assert!(
      !service
        .get_session_input_status(status_request.clone())
        .await
        .unwrap()
        .available
    );
    // Reloading ownership does not launch a process or replay either prompt.
    service.sessions.managed =
      crate::managed_sessions::ManagedSessions::with_test_executable(Some(receipts), executable).unwrap();
    let status = service.get_session_input_status(status_request.clone()).await.unwrap();
    assert!(status.available);
    assert!(status.resume_required);
    // The broker answers a previously accepted request before any resume or
    // filesystem work, even after its original worker has stopped.
    assert_eq!(
      service.submit_session_input(follow_up).await.unwrap().status,
      InputDeliveryStatus::Accepted
    );
    assert_eq!(
      service
        .get_managed_session(identity.clone())
        .await
        .unwrap()
        .unwrap()
        .status,
      ManagedSessionStatus::Stopped
    );
    let next = SubmitSessionInputRequest {
      model_selection: None,
      session_key: key.clone(),
      request_id: uuid::Uuid::new_v4().to_string(),
      text: "after restart".into(),
      image_ids: Vec::new(),
    };
    assert_eq!(
      service.submit_session_input(next).await.unwrap().status,
      InputDeliveryStatus::Accepted
    );
    let resumed = service.get_managed_session(identity).await.unwrap().unwrap();
    assert_eq!(resumed.session_key.as_deref(), Some(key.as_str()));
    let status = service.get_session_input_status(status_request).await.unwrap();
    assert!(status.available);
    assert!(!status.resume_required);
    service.shutdown_managed_sessions().await;
    assert_eq!(
      std::fs::read_to_string(Path::new(&project.folder_path).join("prompts.jsonl")).unwrap(),
      format!("{prompts}\"after restart\"\n")
    );
    service.relay.shutdown().await;
  }

  #[test]
  fn creation_rechecks_identity_and_folder_but_old_history_survives_folder_removal() {
    let (_directory, service, project, locator) = fixture();
    let key = encode_session_key(&locator).unwrap();
    assert!(
      service
        .index_managed_pi_session(&key, Some("another-id"), &project.project_id)
        .is_err()
    );
    let other = service
      .register_project(RegisterProjectRequest {
        folder_path: locator.source_path.parent().unwrap().to_str().unwrap().into(),
        name: None,
      })
      .unwrap();
    assert!(
      service
        .index_managed_pi_session(&key, Some(&locator.session_id), &other.project_id)
        .is_err()
    );
    service
      .index_managed_pi_session(&key, Some(&locator.session_id), &project.project_id)
      .unwrap()
      .unwrap();
    std::fs::remove_dir(&project.folder_path).unwrap();
    assert!(service.validated_project(&project.project_id).is_err());
    assert!(
      service
        .index_managed_pi_session(&key, Some(&locator.session_id), &project.project_id)
        .unwrap()
        .is_some()
    );
  }

  #[test]
  fn explicit_creation_cannot_retire_another_identity_at_the_same_source_path() {
    let (_directory, service, project, locator) = fixture();
    let source = index_source_key_for_path(ViewerProvider::Pi, &locator.source_path).unwrap();
    let mut other = self::SessionMetadata::new(
      IndexedSessionKey::new("pi", source.source_key.clone(), "another-session"),
      locator.source_path.to_str().unwrap(),
    );
    other.cwd = Some(project.folder_path.clone());
    service
      .session_index
      .replace_sources(&[SourceReplacement::new(
        SourceState::new(source, "another-owner", 0),
        vec![other],
      )])
      .unwrap();
    let key = encode_session_key(&locator).unwrap();
    assert!(
      service
        .index_managed_pi_session(&key, Some(&locator.session_id), &project.project_id)
        .is_err()
    );
    let present = service.session_index.list_present_sessions().unwrap();
    assert_eq!(present.len(), 1);
    assert_eq!(present[0].key.session_id, "another-session");
  }

  #[test]
  fn a_competing_index_commit_during_header_read_wins_the_creation_binding_race() {
    struct BlockingHeader {
      header: SessionHeader,
      started: std::sync::mpsc::Sender<()>,
      resume: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl ViewerRepository for BlockingHeader {
      fn list_session_headers(&self, _: ViewerProvider) -> Result<Vec<SessionHeader>, String> {
        panic!("creation must not enumerate provider history")
      }
      fn session_header_at_path(&self, _: ViewerProvider, _: &Path) -> Result<SessionHeader, String> {
        self.started.send(()).unwrap();
        self.resume.lock().unwrap().recv().unwrap();
        Ok(self.header.clone())
      }
      fn load_session(&self, _: &SessionLocator) -> Result<LoadedSession, String> {
        panic!("creation binding must not load bodies")
      }
    }
    let (directory, service, project, locator) = fixture();
    let header = service
      .repository
      .session_header_at_path(ViewerProvider::Pi, &locator.source_path)
      .unwrap();
    let (started, waiter) = std::sync::mpsc::channel();
    let (resume, paused) = std::sync::mpsc::channel();
    let service = ViewerService::new_with_index(
      Arc::new(BlockingHeader {
        header: header.clone(),
        started,
        resume: Mutex::new(paused),
      }),
      service.session_index.clone(),
    );
    let key = encode_session_key(&locator).unwrap();
    let project_id = project.project_id.clone();
    let session_id = locator.session_id.clone();
    let pending = std::thread::spawn(move || service.index_managed_pi_session(&key, Some(&session_id), &project_id));
    waiter.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let winner = SessionIndex::open(directory.path().join("index.sqlite")).unwrap();
    let source = index_source_key_for_path(ViewerProvider::Pi, &locator.source_path).unwrap();
    let mut metadata = session_metadata_from_header(&source, header, None, false).unwrap();
    metadata.title = Some("Winner metadata".into());
    winner
      .replace_sources(&[SourceReplacement::new(
        SourceState::new(source, "newer-committed-revision", 1),
        vec![metadata],
      )])
      .unwrap();
    resume.send(()).unwrap();
    assert!(pending.join().unwrap().unwrap().is_none());
    let stored = winner.session(&index_session_key(&locator).unwrap()).unwrap().unwrap();
    assert_eq!(stored.title.as_deref(), Some("Winner metadata"));
    assert_eq!(stored.source_cursor, "newer-committed-revision");
  }

  #[tokio::test]
  async fn shares_cannot_inspect_create_or_control_owned_sessions() {
    let (_directory, service, project, locator) = fixture();
    let key = encode_session_key(&locator).unwrap();
    let shared = service.scoped_to_sessions(&[key.clone()]).unwrap();
    let request = || ManagedSessionRequest {
      request_id: None,
      session_key: Some(key.clone()),
    };
    assert!(shared.interaction_service().is_err());
    assert!(shared.get_managed_session(request()).await.is_err());
    assert!(shared.stop_managed_session(request()).await.is_err());
    assert!(shared.resume_managed_session(request()).await.is_err());
    assert!(
      shared
        .create_project_session(CreateProjectSessionRequest {
          provider: tokn_interaction_core::Provider::Pi,
          runtime_profile_id: None,
          runtime_revision: None,
          request_id: uuid::Uuid::new_v4().to_string(),
          project_id: project.project_id,
          prompt: "hello".into(),
          image_ids: Vec::new(),
        })
        .await
        .is_err()
    );
    assert!(
      shared
        .respond_managed_session_dialog(ManagedSessionDialogResponse {
          request_id: uuid::Uuid::new_v4().to_string(),
          dialog_id: "dialog".into(),
          value: None,
          confirmed: Some(true),
          cancelled: false,
        })
        .await
        .is_err()
    );
  }

  #[test]
  fn broken_managed_receipts_do_not_prevent_read_only_viewer_startup() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("viewer/managed-sessions.json");
    std::fs::create_dir(state.parent().unwrap()).unwrap();
    std::fs::write(&state, "invalid managed-session state").unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    assert!(service.list_registered_projects().unwrap().projects.is_empty());
    assert!(service.session_index.list_present_sessions().unwrap().is_empty());
  }

  #[tokio::test]
  async fn invalid_first_messages_are_definite_non_starts_that_can_be_edited() {
    let (_directory, service, project, _) = fixture();
    for prompt in [
      " ".into(),
      "bad\0prompt".into(),
      "x".repeat(crate::input::MAX_INPUT_LENGTH + 1),
    ] {
      let request = CreateProjectSessionRequest {
        provider: tokn_interaction_core::Provider::Pi,
        runtime_profile_id: None,
        runtime_revision: None,
        request_id: uuid::Uuid::new_v4().to_string(),
        project_id: project.project_id.clone(),
        prompt,
        image_ids: Vec::new(),
      };
      let error = service.create_project_session(request.clone()).await.unwrap_err();
      assert!(!error.may_have_started);
      assert!(
        service
          .sessions
          .managed
          .get(&ManagedSessionRequest {
            request_id: Some(request.request_id),
            session_key: None
          })
          .unwrap()
          .is_none()
      );
    }
  }
}
