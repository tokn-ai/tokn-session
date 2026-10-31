//! Device-owned configuration. These files never follow the selected machine.
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokn_session_storage::{SessionPaths, read_private_json, update_private_json};

const VIEWER_LIMIT: u64 = 4096;
const MACHINES_LIMIT: u64 = 32_768;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SidebarOrder {
  Time,
  Project,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundScope {
  All,
  Sidebar,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundLevel {
  Final,
  Messages,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationLevel {
  Final,
  Messages,
  Steps,
  All,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ViewerPreferences {
  pub version: u8,
  pub sidebar_order: SidebarOrder,
  pub background_updates_scope: BackgroundScope,
  pub background_update_level: BackgroundLevel,
  pub conversation_update_level: ConversationLevel,
  pub show_lifecycle: bool,
}

impl Default for ViewerPreferences {
  fn default() -> Self {
    Self {
      version: 1,
      sidebar_order: SidebarOrder::Time,
      background_updates_scope: BackgroundScope::All,
      background_update_level: BackgroundLevel::Messages,
      conversation_update_level: ConversationLevel::Steps,
      show_lifecycle: false,
    }
  }
}

impl ViewerPreferences {
  fn validate(&self) -> Result<(), String> {
    if self.version != 1 {
      return Err("Unsupported viewer preferences version; restore a compatible preferences.json".into());
    }
    Ok(())
  }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MachineSelection {
  Local,
  Hub { hub_url: String, host_id: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MachinePreferences {
  pub version: u8,
  pub selected_machine: Option<MachineSelection>,
  pub hub_urls: Vec<String>,
}

impl Default for MachinePreferences {
  fn default() -> Self {
    Self {
      version: 1,
      selected_machine: None,
      hub_urls: Vec::new(),
    }
  }
}

fn canonical_hub(value: &str) -> Result<String, String> {
  if value.len() > 4096 {
    return Err("Saved Hub address exceeds its size limit".into());
  }
  Ok(tokn_hub_remote::canonical_hub(value)?.origin().ascii_serialization())
}

impl MachinePreferences {
  fn validated(mut self) -> Result<Self, String> {
    if self.version != 1 {
      return Err("Unsupported machine preferences version; restore a compatible machines.json".into());
    }
    if self.hub_urls.len() > 16 {
      return Err("Saved machine preferences exceed the Hub count limit".into());
    }
    let mut hubs = Vec::new();
    for value in &self.hub_urls {
      let value = canonical_hub(value)?;
      if !hubs.contains(&value) {
        hubs.push(value);
      }
    }
    if let Some(MachineSelection::Hub { hub_url, host_id }) = &mut self.selected_machine {
      if !tokn_hub_client_core::protocol::valid_host_uuid(host_id) {
        return Err("Saved machine ID must be a canonical version-4 UUID".into());
      }
      *hub_url = canonical_hub(hub_url)?;
      hubs.retain(|value| value != hub_url);
      hubs.insert(0, hub_url.clone());
      hubs.truncate(16);
    }
    self.hub_urls = hubs;
    Ok(self)
  }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeDevicePreferencesRequest {
  pub viewer_preferences: Option<ViewerPreferences>,
  pub machine_preferences: Option<MachinePreferences>,
}

#[derive(Debug, Serialize)]
pub struct DevicePreferencesSnapshot {
  pub version: u8,
  pub revision: u64,
  pub viewer_preferences: ViewerPreferences,
  pub machine_preferences: MachinePreferences,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportState {
  version: u8,
}

impl ImportState {
  fn validate(&self) -> Result<(), String> {
    if self.version != 1 {
      return Err("Unsupported device preference import state; restore import-state.json".into());
    }
    Ok(())
  }
}

/// Serialize persistence and event publication together within this app. The
/// storage helper additionally locks each file across independent processes.
pub struct DevicePreferences {
  pub gate: tokio::sync::Mutex<u64>,
  paths: Result<SessionPaths, String>,
}

impl Default for DevicePreferences {
  fn default() -> Self {
    Self {
      gate: tokio::sync::Mutex::new(0),
      paths: SessionPaths::discover(),
    }
  }
}

impl DevicePreferences {
  #[cfg(mobile)]
  pub fn new(paths: SessionPaths) -> Self {
    Self {
      gate: tokio::sync::Mutex::new(0),
      paths: Ok(paths),
    }
  }

  pub fn paths(&self) -> Result<SessionPaths, String> {
    self.paths.clone()
  }
}

pub fn initialize(
  paths: &SessionPaths,
  request: InitializeDevicePreferencesRequest,
) -> Result<DevicePreferencesSnapshot, String> {
  let viewer = paths.viewer_dir().join("preferences.json");
  let machines = paths.viewer_dir().join("machines.json");
  let import_state = paths.viewer_dir().join("import-state.json");
  let imported = read_private_json::<ImportState>(&import_state, 256)?;
  if let Some(imported) = &imported {
    imported.validate()?;
  }
  let request = if imported.is_some() {
    InitializeDevicePreferencesRequest {
      viewer_preferences: None,
      machine_preferences: None,
    }
  } else {
    request
  };
  let viewer_preferences = update_private_json(&viewer, VIEWER_LIMIT, |previous| {
    let next: ViewerPreferences = previous.unwrap_or_else(|| request.viewer_preferences.unwrap_or_default());
    next.validate()?;
    Ok(next)
  })?;
  let machine_preferences = update_private_json(&machines, MACHINES_LIMIT, |previous: Option<MachinePreferences>| {
    previous
      .unwrap_or_else(|| request.machine_preferences.unwrap_or_default())
      .validated()
  })?;
  update_private_json(&import_state, 256, |previous: Option<ImportState>| {
    let state = previous.unwrap_or(ImportState { version: 1 });
    state.validate()?;
    Ok(state)
  })?;
  Ok(DevicePreferencesSnapshot {
    version: 1,
    revision: 0,
    viewer_preferences,
    machine_preferences,
  })
}

pub fn save_viewer(path: &Path, preferences: ViewerPreferences) -> Result<ViewerPreferences, String> {
  preferences.validate()?;
  update_private_json(path, VIEWER_LIMIT, |previous: Option<ViewerPreferences>| {
    if let Some(previous) = previous {
      previous.validate()?;
    }
    Ok(preferences)
  })
}

pub fn save_machines(path: &Path, preferences: MachinePreferences) -> Result<MachinePreferences, String> {
  let preferences = preferences.validated()?;
  update_private_json(path, MACHINES_LIMIT, |previous: Option<MachinePreferences>| {
    if let Some(previous) = previous {
      previous.validated()?;
    }
    Ok(preferences)
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn request() -> InitializeDevicePreferencesRequest {
    InitializeDevicePreferencesRequest {
      viewer_preferences: Some(ViewerPreferences {
        sidebar_order: SidebarOrder::Project,
        ..Default::default()
      }),
      machine_preferences: Some(MachinePreferences {
        selected_machine: Some(MachineSelection::Local),
        ..Default::default()
      }),
    }
  }

  #[test]
  fn imports_once_and_existing_files_win_without_touching_browser_trust() {
    let directory = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(directory.path());
    let first = initialize(&paths, request()).unwrap();
    assert_eq!(first.viewer_preferences.sidebar_order, SidebarOrder::Project);
    assert_eq!(
      first.machine_preferences.selected_machine,
      Some(MachineSelection::Local)
    );
    let second = initialize(
      &paths,
      InitializeDevicePreferencesRequest {
        viewer_preferences: None,
        machine_preferences: None,
      },
    )
    .unwrap();
    assert_eq!(second.viewer_preferences, first.viewer_preferences);
    assert_eq!(second.machine_preferences, first.machine_preferences);
    assert!(!paths.hub_dir().exists());
    assert!(!paths.index_path().exists());
  }

  #[test]
  fn deleting_a_native_file_does_not_resurrect_legacy_webview_preferences() {
    let directory = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(directory.path());
    initialize(&paths, request()).unwrap();
    std::fs::remove_file(paths.viewer_dir().join("preferences.json")).unwrap();
    std::fs::remove_file(paths.viewer_dir().join("machines.json")).unwrap();
    let restored = initialize(&paths, request()).unwrap();
    assert_eq!(restored.viewer_preferences, ViewerPreferences::default());
    assert_eq!(restored.machine_preferences, MachinePreferences::default());
  }

  #[test]
  fn malformed_or_future_native_files_are_preserved_on_initialize_and_write() {
    let directory = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(directory.path());
    initialize(&paths, request()).unwrap();
    let path = paths.viewer_dir().join("preferences.json");
    for value in [
      "{invalid".to_owned(),
      serde_json::to_string(&ViewerPreferences {
        version: 2,
        ..Default::default()
      })
      .unwrap(),
    ] {
      std::fs::write(&path, &value).unwrap();
      assert!(initialize(&paths, request()).is_err());
      assert!(save_viewer(&path, ViewerPreferences::default()).is_err());
      assert_eq!(std::fs::read_to_string(&path).unwrap(), value);
    }
  }

  #[test]
  fn saves_view_preferences_separately_from_machine_navigation() {
    let directory = tempfile::tempdir().unwrap();
    let paths = SessionPaths::from_home(directory.path());
    initialize(&paths, request()).unwrap();
    let expected = ViewerPreferences {
      show_lifecycle: true,
      ..Default::default()
    };
    save_viewer(&paths.viewer_dir().join("preferences.json"), expected.clone()).unwrap();
    let restored = initialize(&paths, request()).unwrap();
    assert_eq!(restored.viewer_preferences, expected);
    assert_eq!(
      restored.machine_preferences.selected_machine,
      Some(MachineSelection::Local)
    );
    let raw: serde_json::Value = read_private_json(&paths.viewer_dir().join("preferences.json"), VIEWER_LIMIT)
      .unwrap()
      .unwrap();
    assert_eq!(raw.as_object().unwrap().len(), 6);
  }

  #[test]
  fn machine_bookmarks_do_not_accept_credentials_or_noncanonical_identities() {
    let valid = MachinePreferences {
      version: 1,
      selected_machine: Some(MachineSelection::Hub {
        hub_url: "https://hub.example/".into(),
        host_id: "550e8400-e29b-41d4-a716-446655440000".into(),
      }),
      hub_urls: vec!["https://hub.example".into(), "https://other.example/".into()],
    };
    assert_eq!(
      valid.clone().validated().unwrap().hub_urls,
      ["https://hub.example", "https://other.example"]
    );
    for bad in [
      "https://name:secret@hub.example",
      "https://hub.example/path",
      "http://remote.example",
    ] {
      assert!(
        MachinePreferences {
          hub_urls: vec![bad.into()],
          ..Default::default()
        }
        .validated()
        .is_err()
      );
    }
    assert!(
      MachinePreferences {
        hub_urls: vec!["https://hub.example".into(); 17],
        ..Default::default()
      }
      .validated()
      .is_err()
    );
    let mut invalid = valid;
    if let Some(MachineSelection::Hub { host_id, .. }) = &mut invalid.selected_machine {
      *host_id = "other-machine".into();
    }
    assert!(invalid.validated().is_err());
  }
}
