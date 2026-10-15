//! Public machine configuration metadata. Viewer preferences and credentials
//! belong to their respective owners and are never part of this projection.
use serde::Serialize;
use std::path::PathBuf;
use tokn_session_core::Provider;
use tokn_session_relay::{ProviderRoot, providers::provider_roots};

use crate::{ViewerService, model::ViewerProvider, relay::RelayStatus};

#[derive(Clone, Debug, Serialize)]
pub struct MachineSettingsSnapshot {
  pub version: u32,
  pub reader: MachineReaderSettings,
  pub sources: Vec<MachineSourceSettings>,
  pub sources_editable: bool,
  pub sources_edit_reason: Option<String>,
  pub hosting: MachineHostingCapability,
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineReaderSettings {
  pub status: RelayStatus,
  pub editable: bool,
  pub edit_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineSourceSettings {
  pub provider: ViewerProvider,
  /// Host-environment discovery locations. An explicitly external reader may
  /// use a different service; these paths do not claim that service's roots.
  pub locations: Vec<MachineSourceLocation>,
  pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineSourceLocation {
  pub path: PathBuf,
  pub kind: SourceLocationKind,
  pub exists: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLocationKind {
  Directory,
  Database,
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineHostingCapability {
  pub available: bool,
  pub editable: bool,
  pub reason: Option<String>,
}

impl ViewerService {
  /// Resolves discovery configuration without listing sessions, parsing
  /// histories, or opening provider databases. Machine-wide configuration is
  /// intentionally unavailable to session-scoped sharing grants.
  pub fn get_machine_settings(&self) -> Result<MachineSettingsSnapshot, String> {
    if self.session_scope.is_some() {
      return Err("Machine settings are unavailable in a session share".into());
    }
    Ok(MachineSettingsSnapshot {
      version: 1,
      reader: MachineReaderSettings {
        status: self.relay.status(),
        editable: false,
        edit_reason: Some("Reader configuration can be changed in this machine's desktop app.".into()),
      },
      sources: tokn_session_relay::PROVIDERS
        .into_iter()
        .map(|provider| {
          source_settings(
            crate::service::viewer_provider(provider),
            provider_roots(provider, None),
          )
        })
        .collect(),
      sources_editable: false,
      sources_edit_reason: Some(
        "Provider locations are detected from this machine's environment. Source overrides are not supported.".into(),
      ),
      hosting: MachineHostingCapability {
        available: false,
        editable: false,
        reason: Some("Hosting settings are available in this machine's desktop app.".into()),
      },
    })
  }
}

fn source_settings(provider: ViewerProvider, roots: Result<Vec<ProviderRoot>, String>) -> MachineSourceSettings {
  let mut source = MachineSourceSettings {
    provider,
    locations: Vec::new(),
    error: None,
  };
  match roots {
    Ok(roots) => {
      for root in roots {
        let exists = match root.path.try_exists() {
          Ok(exists) => exists,
          Err(error) => {
            source.error = Some(format!("Could not check {}: {error}", root.path.display()));
            false
          }
        };
        source.locations.push(MachineSourceLocation {
          path: root.path,
          kind: if matches!(root.provider, Provider::OpenCode | Provider::ZCode) {
            SourceLocationKind::Database
          } else {
            SourceLocationKind::Directory
          },
          exists,
        });
      }
    }
    Err(error) => source.error = Some(error),
  }
  source
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{model::SessionLocator, model::encode_session_key};

  #[test]
  fn machine_metadata_is_versioned_read_only_and_does_not_queue_history() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let progress = service.session_index_progress();
    let value = serde_json::to_value(service.get_machine_settings().unwrap()).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["reader"]["editable"], false);
    assert_eq!(value["sources_editable"], false);
    assert_eq!(value["hosting"]["available"], false);
    assert_eq!(value["hosting"]["editable"], false);
    assert_eq!(value["sources"].as_array().unwrap().len(), 6);
    assert_eq!(
      value["reader"]["status"],
      serde_json::to_value(service.relay.status()).unwrap()
    );
    assert_eq!(
      service.session_index_progress().body.pending_jobs,
      progress.body.pending_jobs
    );
    assert_eq!(service.session_index_progress().revision, progress.revision);
  }

  #[test]
  fn source_detection_never_opens_provider_history_or_databases() {
    let directory = tempfile::tempdir().unwrap();
    let broken = directory.path().join("invalid.jsonl");
    std::fs::write(&broken, "this is not a session or a database").unwrap();
    for provider in tokn_session_relay::PROVIDERS {
      let location = if matches!(provider, Provider::OpenCode | Provider::ZCode) {
        broken.clone()
      } else {
        directory.path().to_path_buf()
      };
      let viewer = crate::service::viewer_provider(provider);
      let source = source_settings(viewer, provider_roots(provider, Some(location)));
      assert!(source.error.is_none(), "{:?}: {:?}", source.provider, source.error);
      assert!(!source.locations.is_empty());
      assert!(source.locations[0].exists);
    }
    let missing = directory.path().join("missing");
    let source = source_settings(
      ViewerProvider::Codex,
      Ok(vec![ProviderRoot::new(Provider::Codex, missing.clone())]),
    );
    assert!(!source.locations[0].exists);
    assert_eq!(source.locations[0].path, missing);
    assert!(source.error.is_none());
  }

  #[test]
  fn machine_configuration_is_not_a_session_share_capability() {
    let directory = tempfile::tempdir().unwrap();
    let service = ViewerService::native(directory.path().join("index.sqlite")).unwrap();
    let key = encode_session_key(&SessionLocator {
      version: 1,
      provider: ViewerProvider::Codex,
      session_id: "shared".into(),
      source_path: directory.path().join("shared.jsonl"),
    })
    .unwrap();
    let shared = service.scoped_to_sessions(&[key]).unwrap();
    assert_eq!(
      shared.get_machine_settings().unwrap_err(),
      "Machine settings are unavailable in a session share"
    );
  }
}
