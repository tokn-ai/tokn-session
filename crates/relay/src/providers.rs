use std::path::PathBuf;

use tokn_session_client::{AgentClient, Source};
use tokn_session_core::Provider;
use tokn_session_dsh::DshSessionSource;
use tokn_session_opencode::OpenCodeSessionSource;
use tokn_session_qoder::QoderSessionSource;
use tokn_session_workbuddy::WorkBuddySessionSource;

use crate::ProviderRoot;

pub const PROVIDERS: [Provider; 7] = [
  Provider::Codex,
  Provider::Pi,
  Provider::OpenCode,
  Provider::ZCode,
  Provider::Qoder,
  Provider::WorkBuddy,
  Provider::Dsh,
];

pub fn source(provider: Provider) -> Source {
  match provider {
    Provider::Codex => Source::Codex,
    Provider::Pi => Source::Pi,
    Provider::OpenCode => Source::OpenCode,
    Provider::ZCode => Source::ZCode,
    Provider::Qoder => Source::Qoder,
    Provider::WorkBuddy => Source::WorkBuddy,
    Provider::Dsh => Source::Dsh,
  }
}

pub fn database(provider: Provider, path: Option<PathBuf>) -> OpenCodeSessionSource {
  match provider {
    Provider::OpenCode => OpenCodeSessionSource::new(path),
    Provider::ZCode => OpenCodeSessionSource::for_zcode(path),
    _ => unreachable!("provider does not use an OpenCode-compatible database"),
  }
}

/// Resolve provider-owned storage configuration for both CLI and managed Relay.
pub fn provider_roots(provider: Provider, path: Option<PathBuf>) -> Result<Vec<ProviderRoot>, String> {
  let paths = match provider {
    Provider::Codex | Provider::Pi => AgentClient::file_session_roots(source(provider), path)?,
    Provider::OpenCode | Provider::ZCode => vec![database(provider, path).database_path()?],
    Provider::WorkBuddy => vec![WorkBuddySessionSource::new(path).config_dir()?],
    Provider::Dsh => vec![DshSessionSource::new(path).session_root()?],
    Provider::Qoder => vec![QoderSessionSource::new(path).session_root()?],
  };
  Ok(
    paths
      .into_iter()
      .map(|path| ProviderRoot::new(provider, path))
      .collect(),
  )
}
