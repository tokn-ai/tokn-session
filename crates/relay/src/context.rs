use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokn_session_core::{Provider, SessionRef};

use crate::project::ProjectCatalog;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProjectContext {
  pub id: Option<String>,
  pub name: Option<String>,
  pub project_name: Option<String>,
  pub folder: Option<String>,
  pub folder_name: Option<String>,
  pub repository_name: Option<String>,
  pub repository_url: Option<String>,
  pub branch: Option<String>,
  pub commit_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionContext {
  pub provider: Provider,
  pub session_id: String,
  pub parent_session_id: Option<String>,
  pub agent_path: Option<String>,
  pub agent_nickname: Option<String>,
  pub agent_role: Option<String>,
  pub title: Option<String>,
  pub cwd: Option<String>,
  pub started_at: Option<String>,
  pub project: Option<ProjectContext>,
}

impl SessionContext {
  pub(crate) fn from_path(provider: Provider, path: &Path) -> Self {
    Self {
      provider,
      session_id: session_id_from_path(path),
      parent_session_id: None,
      agent_path: None,
      agent_nickname: None,
      agent_role: None,
      title: None,
      cwd: None,
      started_at: None,
      project: None,
    }
  }

  pub fn from_session_ref(provider: Provider, reference: &SessionRef) -> Self {
    let project = project_context(reference.cwd.as_deref(), None);
    Self {
      provider,
      session_id: reference.id.clone(),
      parent_session_id: reference.parent_session_id.clone(),
      agent_path: reference.agent_path.clone(),
      agent_nickname: reference.agent_nickname.clone(),
      agent_role: reference.agent_role.clone(),
      title: None,
      cwd: reference.cwd.clone(),
      started_at: reference.timestamp.clone(),
      project,
    }
  }

  pub(crate) fn update(&mut self, value: &Value) {
    match self.provider {
      Provider::Codex => self.update_codex(value),
      Provider::Pi => self.update_pi(value),
      Provider::OpenCode | Provider::ZCode | Provider::Qoder | Provider::WorkBuddy | Provider::Dsh => {}
    }
  }

  pub(crate) fn resolve_project_name(&mut self, projects: &ProjectCatalog) {
    let project_name = projects
      .resolve(&self.session_id, self.parent_session_id.as_deref(), self.cwd.as_deref())
      .map(|identity| identity.name);
    if project_name.is_some() {
      self.project.get_or_insert_with(empty_project_context).project_name = project_name;
    } else if let Some(project) = &mut self.project {
      project.project_name = None;
    }
  }

  fn update_codex(&mut self, value: &Value) {
    let Some(payload) = value.get("payload") else {
      return;
    };

    match (
      value.get("type").and_then(Value::as_str),
      payload.get("type").and_then(Value::as_str),
    ) {
      (Some("session_meta"), _) => self.update_codex_session_meta(value, payload),
      (Some("event_msg"), Some("thread_settings_applied")) => {
        self.update_codex_thread_settings(payload);
      }
      _ => {}
    }
  }

  fn update_codex_session_meta(&mut self, value: &Value, payload: &Value) {
    if self.started_at.is_some() {
      return;
    }

    if let Some(id) = string_field(payload, "id") {
      self.session_id = id;
    }
    // A user fork is a new root session. Only Codex's explicit parent-thread
    // relationship identifies a session that belongs in the subagent tree.
    self.parent_session_id = string_field(payload, "parent_thread_id");
    let thread_spawn = payload
      .get("source")
      .and_then(|source| source.get("subagent"))
      .and_then(|subagent| subagent.get("thread_spawn"));
    self.agent_path = string_field(payload, "agent_path")
      .or_else(|| thread_spawn.and_then(|thread_spawn| string_field(thread_spawn, "agent_path")));
    self.agent_nickname = string_field(payload, "agent_nickname")
      .or_else(|| thread_spawn.and_then(|thread_spawn| string_field(thread_spawn, "agent_nickname")));
    self.agent_role = first_string_field(payload, &["agent_role", "agent_type"]).or_else(|| {
      thread_spawn.and_then(|thread_spawn| first_string_field(thread_spawn, &["agent_role", "agent_type"]))
    });
    self.title = string_field(payload, "title");
    self.cwd = string_field(payload, "cwd");
    self.started_at = string_field(payload, "timestamp").or_else(|| string_field(value, "timestamp"));
    self.project = project_context(self.cwd.as_deref(), payload.get("git"));
  }

  fn update_codex_thread_settings(&mut self, payload: &Value) {
    if let Some(cwd) = payload
      .get("thread_settings")
      .and_then(|settings| string_field(settings, "cwd"))
    {
      self.cwd = Some(cwd);
    }
  }

  fn update_pi(&mut self, value: &Value) {
    if value.get("type").and_then(Value::as_str) != Some("session") {
      return;
    }

    if let Some(id) = string_field(value, "id") {
      self.session_id = id;
    }
    self.title = string_field(value, "title");
    self.cwd = string_field(value, "cwd");
    self.started_at = string_field(value, "timestamp");
    self.project = project_context(self.cwd.as_deref(), None);
  }
}

fn project_context(cwd: Option<&str>, git: Option<&Value>) -> Option<ProjectContext> {
  let repository_url = git.and_then(|git| string_field(git, "repository_url"));
  let folder = cwd.map(str::to_string);
  let folder_name = cwd.and_then(project_name_from_folder);
  let repository_name = repository_url.as_deref().and_then(project_name_from_repository);
  let name = repository_name.clone().or_else(|| folder_name.clone());
  let branch = git.and_then(|git| string_field(git, "branch"));
  let commit_hash = git.and_then(|git| string_field(git, "commit_hash"));

  if repository_url.is_none() && folder.is_none() && branch.is_none() && commit_hash.is_none() {
    return None;
  }

  Some(ProjectContext {
    id: repository_url.clone(),
    name,
    project_name: None,
    folder,
    folder_name,
    repository_name,
    repository_url,
    branch,
    commit_hash,
  })
}

fn empty_project_context() -> ProjectContext {
  ProjectContext {
    id: None,
    name: None,
    project_name: None,
    folder: None,
    folder_name: None,
    repository_name: None,
    repository_url: None,
    branch: None,
    commit_hash: None,
  }
}

fn project_name_from_repository(repository_url: &str) -> Option<String> {
  repository_url
    .trim_end_matches('/')
    .rsplit(['/', ':'])
    .next()
    .map(|name| name.trim_end_matches(".git"))
    .filter(|name| !name.is_empty())
    .map(str::to_string)
}

fn project_name_from_folder(folder: &str) -> Option<String> {
  Path::new(folder)
    .file_name()
    .and_then(|name| name.to_str())
    .filter(|name| !name.is_empty())
    .map(str::to_string)
}

pub(crate) fn session_id_from_path(path: &Path) -> String {
  path
    .file_stem()
    .and_then(|value| value.to_str())
    .unwrap_or("unknown")
    .rsplit(['-', '_'])
    .next()
    .unwrap_or("unknown")
    .to_string()
}

fn first_string_field(value: &Value, fields: &[&str]) -> Option<String> {
  fields.iter().find_map(|field| string_field(value, field))
}

fn string_field(value: &Value, field: &str) -> Option<String> {
  value
    .get(field)
    .and_then(Value::as_str)
    .filter(|value| !value.is_empty())
    .map(str::to_string)
}

#[cfg(test)]
mod tests {
  use std::path::Path;

  use serde_json::json;
  use tokn_session_core::Provider;

  use super::SessionContext;

  #[test]
  fn extracts_codex_session_and_project_context() {
    let mut context = SessionContext::from_path(Provider::Codex, Path::new("fallback.jsonl"));
    context.update(&json!({
      "timestamp": "2026-06-04T00:00:00Z",
      "type": "session_meta",
      "payload": {
        "id": "session-1",
        "parent_thread_id": "parent-1",
        "agent_path": "/root/researcher",
        "agent_nickname": "Hubble",
        "agent_role": "explorer",
        "title": "Investigate relay",
        "cwd": "/tmp/worktree",
        "git": {
          "repository_url": "git@github.com:agentic-rs/tokn-session.git",
          "branch": "main",
          "commit_hash": "abcdef"
        }
      }
    }));

    assert_eq!(context.session_id, "session-1");
    assert_eq!(context.parent_session_id.as_deref(), Some("parent-1"));
    assert_eq!(context.agent_path.as_deref(), Some("/root/researcher"));
    assert_eq!(context.agent_nickname.as_deref(), Some("Hubble"));
    assert_eq!(context.agent_role.as_deref(), Some("explorer"));
    assert_eq!(context.title.as_deref(), Some("Investigate relay"));
    assert_eq!(context.started_at.as_deref(), Some("2026-06-04T00:00:00Z"));
    let project = context.project.unwrap();
    assert_eq!(project.name.as_deref(), Some("tokn-session"));
    assert_eq!(project.project_name, None);
    assert_eq!(project.folder.as_deref(), Some("/tmp/worktree"));
    assert_eq!(project.folder_name.as_deref(), Some("worktree"));
    assert_eq!(project.repository_name.as_deref(), Some("tokn-session"));
  }

  #[test]
  fn updates_effective_cwd_from_codex_thread_settings() {
    let mut context = SessionContext::from_path(Provider::Codex, Path::new("fallback.jsonl"));
    context.update(&json!({
      "type": "session_meta",
      "payload": {
        "id": "session-1",
        "cwd": "/tmp/project"
      }
    }));
    context.update(&json!({
      "type": "event_msg",
      "payload": {
        "type": "thread_settings_applied",
        "thread_settings": {
          "cwd": "/tmp/project/subdir"
        }
      }
    }));

    assert_eq!(context.cwd.as_deref(), Some("/tmp/project/subdir"));
    assert_eq!(
      context.project.as_ref().and_then(|project| project.folder.as_deref()),
      Some("/tmp/project")
    );
  }

  #[test]
  fn keeps_first_codex_session_meta_as_the_owning_session() {
    let mut context = SessionContext::from_path(Provider::Codex, Path::new("fallback.jsonl"));
    context.update(&json!({
      "type": "session_meta",
      "payload": {
        "id": "child-session",
        "parent_thread_id": "root-session",
        "timestamp": "2026-07-24T17:52:40Z",
        "cwd": "/tmp/child"
      }
    }));
    context.update(&json!({
      "type": "session_meta",
      "payload": {
        "id": "root-session",
        "timestamp": "2026-07-15T10:00:00Z",
        "cwd": "/tmp/root"
      }
    }));

    assert_eq!(context.session_id, "child-session");
    assert_eq!(context.parent_session_id.as_deref(), Some("root-session"));
    assert_eq!(context.agent_path, None);
    assert_eq!(context.cwd.as_deref(), Some("/tmp/child"));
    assert_eq!(context.started_at.as_deref(), Some("2026-07-24T17:52:40Z"));
  }

  #[test]
  fn keeps_codex_user_forks_out_of_the_subagent_tree() {
    let mut context = SessionContext::from_path(Provider::Codex, Path::new("fork.jsonl"));
    context.update(&json!({
      "type": "session_meta",
      "payload": {
        "id": "forked-session",
        "forked_from_id": "root-session",
        "thread_source": "user",
        "timestamp": "2026-08-03T06:56:15Z",
        "cwd": "/tmp/fork"
      }
    }));

    assert_eq!(context.session_id, "forked-session");
    assert_eq!(context.parent_session_id, None);
  }

  #[test]
  fn reads_agent_metadata_from_thread_spawn_source() {
    let mut context = SessionContext::from_path(Provider::Codex, Path::new("fallback.jsonl"));
    context.update(&json!({
      "type": "session_meta",
      "payload": {
        "id": "child-session",
        "timestamp": "2026-07-24T17:52:40Z",
        "cwd": "/tmp/child",
        "source": {
          "subagent": {
            "thread_spawn": {
              "agent_path": "/root/researcher",
              "agent_nickname": "Hubble",
              "agent_role": "explorer"
            }
          }
        }
      }
    }));

    assert_eq!(context.agent_path.as_deref(), Some("/root/researcher"));
    assert_eq!(context.agent_nickname.as_deref(), Some("Hubble"));
    assert_eq!(context.agent_role.as_deref(), Some("explorer"));
  }

  #[test]
  fn leaves_unavailable_root_and_subagent_paths_null() {
    let mut root = SessionContext::from_path(Provider::Codex, Path::new("root.jsonl"));
    root.update(&json!({
      "type": "session_meta",
      "payload": {
        "id": "root-session",
        "timestamp": "2026-07-24T17:52:40Z",
        "cwd": "/tmp/root",
        "source": "vscode"
      }
    }));

    let mut guardian = SessionContext::from_path(Provider::Codex, Path::new("guardian.jsonl"));
    guardian.update(&json!({
      "type": "session_meta",
      "payload": {
        "id": "guardian-session",
        "parent_thread_id": "root-session",
        "timestamp": "2026-07-24T17:52:40Z",
        "cwd": "/tmp/root",
        "source": {
          "subagent": {
            "other": "guardian"
          }
        }
      }
    }));

    assert_eq!(root.agent_path, None);
    assert_eq!(guardian.agent_path, None);
  }
}
