//! Treat the installed API and connector as one background hosting unit.
//! Inspect each registration independently; never infer ownership from a PID.
use serde::Serialize;
use std::path::Path;
use tokn_session_hub::onboarding::HostProfile;

#[derive(Clone, Default, Serialize)]
pub struct BackgroundService {
  pub registered: bool,
  pub loaded: bool,
  pub running: bool,
  pub verified: bool,
  pub error: Option<String>,
}
#[derive(Clone, Default, Serialize)]
pub struct BackgroundHosting {
  pub api: BackgroundService,
  pub connector: BackgroundService,
}
impl BackgroundHosting {
  pub fn has_loaded(&self) -> bool {
    self.api.loaded || self.connector.loaded
  }
  pub fn has_registered(&self) -> bool {
    self.api.registered || self.connector.registered
  }
  fn recognized(&self) -> bool {
    [&self.api, &self.connector]
      .iter()
      .all(|s| s.error.is_none() && (!(s.registered || s.loaded) || s.verified))
  }
  pub fn stop_supported(&self) -> bool {
    self.has_loaded() && self.recognized()
  }
  pub fn remove_supported(&self) -> bool {
    self.has_registered() && self.recognized()
  }
}

#[cfg(target_os = "macos")]
mod macos {
  use super::*;
  use serde_json::Value;
  use std::{os::unix::fs::MetadataExt, path::PathBuf, time::Duration};
  use tokio::process::Command;

  #[derive(Clone, Copy, PartialEq)]
  enum Kind {
    Connector,
    Api,
  }
  impl Kind {
    fn label(self) -> &'static str {
      match self {
        Self::Connector => "com.tokn.hub-connector",
        Self::Api => "com.tokn.viewer-api",
      }
    }
    fn argument(self) -> &'static str {
      match self {
        Self::Connector => "hub",
        Self::Api => "api",
      }
    }
  }
  struct Service {
    kind: Kind,
    plist: PathBuf,
    target: String,
    status: BackgroundService,
  }
  fn uid() -> u32 {
    // SAFETY: geteuid reads the current effective user without accessing Rust memory.
    unsafe { libc::geteuid() }
  }
  async fn owned_file(path: &Path) -> Result<(), String> {
    let metadata = tokio::fs::symlink_metadata(path)
      .await
      .map_err(|_| "Service files are unavailable")?;
    if !metadata.is_file() || metadata.uid() != uid() || metadata.mode() & 0o022 != 0 || metadata.len() > 64 * 1024 {
      return Err("Service files must be owned by this user and not writable by others".into());
    }
    Ok(())
  }
  async fn output(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    tokio::time::timeout(
      Duration::from_secs(3),
      Command::new(program).args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| "Background service check timed out")?
    .map_err(|_| "Could not inspect background service".to_string())
  }
  fn matches_definition(value: &Value, runner: &Path, kind: Kind) -> bool {
    value["Label"].as_str() == Some(kind.label())
      && value["ProgramArguments"].as_array().is_some_and(|args| {
        args.len() == 2 && args[0].as_str() == runner.to_str() && args[1].as_str() == Some(kind.argument())
      })
      && value
        .get("Program")
        .is_none_or(|program| program.as_str() == runner.to_str())
      && value.get("EnvironmentVariables").is_none_or(|env| {
        // Alternate HOME/state overrides would change which machine is served.
        env.as_object().is_some_and(|env| env.keys().all(|key| key == "PATH"))
      })
  }
  fn field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| line.trim().strip_prefix(name))
  }
  fn matches_loaded(text: &str, plist: &Path, runner: &Path, kind: Kind) -> bool {
    let arguments = text
      .split("arguments = {\n")
      .nth(1)
      .and_then(|body| body.split('}').next());
    field(text, "path = ") == plist.to_str()
      && field(text, "program = ") == runner.to_str()
      && arguments.is_some_and(|args| {
        let args: Vec<_> = args.lines().map(str::trim).filter(|arg| !arg.is_empty()).collect();
        args.len() == 2 && Some(args[0]) == runner.to_str() && args[1] == kind.argument()
      })
  }
  fn matches_launcher(script: &str, root: &Path, kind: Kind, profile: Option<&HostProfile>) -> bool {
    let expected = match kind {
      Kind::Api => format!(
        "api) exec {} --bind 127.0.0.1:5558 --api-only ;;",
        root.join("current/tokn-viewer-api").display()
      ),
      Kind::Connector => {
        let Some(profile) = profile else { return false };
        let name = profile
          .name
          .replace('\\', "\\\\")
          .replace('"', "\\\"")
          .replace('$', "\\$")
          .replace('`', "\\`");
        format!(
          "hub) exec {} connect --hub {} --name \"{}\" --viewer-url {}{} ;;",
          root.join("current/tokn-session-hub").display(),
          profile.hub_url.trim_end_matches('/'),
          name,
          profile.viewer_url.trim_end_matches('/'),
          if profile.allow_control { " --allow-control" } else { "" }
        )
      }
    };
    // Recognize the entire small deployed script, not a matching line hidden
    // among extra shell instructions that could launch a different service.
    let Some(home) = root.parent().and_then(Path::parent).and_then(Path::parent) else {
      return false;
    };
    let token = format!(
      "export TOKN_VIEWER_TOKEN=\"$(cat {}/.tokn/hub/viewer.token)\"",
      home.display()
    );
    let lines: Vec<_> = script.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines.len() == 8
      && lines[0] == "#!/bin/sh"
      && lines[1] == "set -eu"
      && lines[2] == token
      && lines[3] == "case \"${1-}\" in"
      && lines[6] == "*) exit 2 ;;"
      && lines[7] == "esac"
      && lines[match kind {
        Kind::Api => 4,
        Kind::Connector => 5,
      }] == expected
  }
  fn not_loaded(output: &std::process::Output, label: &str) -> bool {
    let error = String::from_utf8_lossy(&output.stderr);
    !output.status.success() && error.contains(&format!("Could not find service \"{label}\" in domain"))
  }
  async fn inspect_service(home: &Path, kind: Kind, profile: Option<&HostProfile>) -> Service {
    let root = home.join(".local/share/tokn-host");
    let runner = root.join("run-service.sh");
    let plist = home.join(format!("Library/LaunchAgents/{}.plist", kind.label()));
    let target = format!("gui/{}/{}", uid(), kind.label());
    let mut service = Service {
      kind,
      plist,
      target,
      status: BackgroundService::default(),
    };
    let result: Result<(), String> = async {
      service.status.registered = match tokio::fs::symlink_metadata(&service.plist).await {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err("Could not inspect background service registration".into()),
      };
      let loaded = output("/bin/launchctl", &["print", &service.target]).await?;
      if loaded.status.success() {
        service.status.loaded = true;
        let text = String::from_utf8_lossy(&loaded.stdout);
        service.status.running = field(&text, "state = ") == Some("running");
        if !matches_loaded(&text, &service.plist, &runner, kind) {
          return Err("Loaded service does not match the installed launcher".into());
        }
      } else if !not_loaded(&loaded, kind.label()) {
        return Err("Could not check whether background service is loaded".into());
      }
      if !service.status.registered && !service.status.loaded {
        return Ok(());
      }
      owned_file(&runner).await?;
      owned_file(&service.plist).await?;
      let script = tokio::fs::read_to_string(&runner)
        .await
        .map_err(|_| "Could not inspect service launcher")?;
      if !matches_launcher(&script, &root, kind, profile) {
        return Err("Installed service launcher is not recognized for this machine".into());
      }
      let definition = output(
        "/usr/bin/plutil",
        &[
          "-convert",
          "json",
          "-o",
          "-",
          service.plist.to_str().ok_or("Invalid service path")?,
        ],
      )
      .await?;
      let value: Value =
        serde_json::from_slice(&definition.stdout).map_err(|_| "Could not inspect service definition")?;
      if !definition.status.success() || !matches_definition(&value, &runner, kind) {
        return Err("Service definition is not recognized".into());
      }
      service.status.verified = true;
      Ok(())
    }
    .await;
    service.status.error = result.err();
    service
  }
  async fn services(state_dir: &Path, profile: Option<&HostProfile>) -> Result<Vec<Service>, String> {
    let home = dirs::home_dir().ok_or("Cannot resolve service directory")?;
    if state_dir != home.join(".tokn/hub") {
      return Ok(Vec::new());
    }
    let (connector, api) = tokio::join!(
      inspect_service(&home, Kind::Connector, profile),
      inspect_service(&home, Kind::Api, profile)
    );
    Ok(vec![connector, api])
  }
  fn summary(services: &[Service]) -> BackgroundHosting {
    let mut status = BackgroundHosting::default();
    for service in services {
      match service.kind {
        Kind::Connector => status.connector = service.status.clone(),
        Kind::Api => status.api = service.status.clone(),
      }
    }
    status
  }
  pub async fn inspect(state_dir: &Path, profile: Option<&HostProfile>) -> Result<BackgroundHosting, String> {
    Ok(summary(&services(state_dir, profile).await?))
  }
  async fn stop_services<F, Fut>(services: &[Service], mut stop: F) -> Result<(), String>
  where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
  {
    if !summary(services).recognized() {
      return Err("Background service ownership changed; no services were stopped".into());
    }
    for kind in [Kind::Connector, Kind::Api] {
      for service in services.iter().filter(|s| s.kind == kind && s.status.loaded) {
        stop(service.target.clone()).await?;
      }
    }
    Ok(())
  }
  pub async fn manage(state_dir: &Path, profile: Option<&HostProfile>, remove: bool) -> Result<(), String> {
    let services = services(state_dir, profile).await?;
    let status = summary(&services);
    if !(if remove {
      status.remove_supported()
    } else {
      status.stop_supported()
    }) {
      return Err(
        "Background service ownership could not be verified. Refresh the status or manage these services manually."
          .into(),
      );
    }
    // Verify both components before changing either. Connector goes first;
    // otherwise it could keep forwarding requests into a stopped API.
    stop_services(&services, |target| async move {
      let result = output("/bin/launchctl", &["bootout", &target]).await?;
      if !result.status.success() {
        return Err(format!("Could not stop {target}. Refresh status before retrying."));
      }
      let result = output("/bin/launchctl", &["print", &target]).await?;
      let label = target.rsplit('/').next().unwrap();
      if !not_loaded(&result, label) {
        return Err(format!("{label} has not stopped; later services were left intact."));
      }
      Ok(())
    })
    .await?;
    if remove {
      // Reinspect after unloading; reject a registration changed during stop.
      let current = self::services(state_dir, profile).await?;
      if !summary(&current).remove_supported() || summary(&current).has_loaded() {
        return Err("Service registration changed while stopping; refresh before removing it".into());
      }
      for service in current {
        if service.status.registered {
          tokio::fs::remove_file(&service.plist)
            .await
            .map_err(|_| format!("Could not remove {} registration", service.kind.label()))?;
        }
      }
    }
    Ok(())
  }
  #[cfg(test)]
  mod tests {
    use super::*;
    fn profile() -> HostProfile {
      HostProfile {
        version: 1,
        host_id: "fixture".into(),
        hub_url: "https://hub.example/".into(),
        name: "Studio Mac".into(),
        viewer_url: "http://127.0.0.1:5558/".into(),
        allow_control: true,
        insecure_loopback: false,
        passkey_origin: None,
        ice_servers: Vec::new(),
      }
    }
    fn launcher() -> String {
      "#!/bin/sh\nset -eu\nexport TOKN_VIEWER_TOKEN=\"$(cat /test/.tokn/hub/viewer.token)\"\ncase \"${1-}\" in\napi) exec /test/.local/share/tokn-host/current/tokn-viewer-api --bind 127.0.0.1:5558 --api-only ;;\nhub) exec /test/.local/share/tokn-host/current/tokn-session-hub connect --hub https://hub.example --name \"Studio Mac\" --viewer-url http://127.0.0.1:5558 --allow-control ;;\n*) exit 2 ;;\nesac\n".into()
    }
    #[test]
    fn both_launchers_require_exact_commands_and_reject_extra_shell_instructions() {
      let root = Path::new("/test/.local/share/tokn-host");
      for kind in [Kind::Connector, Kind::Api] {
        assert!(matches_launcher(&launcher(), root, kind, Some(&profile())));
        assert!(!matches_launcher(
          &(launcher() + "exec other\n"),
          root,
          kind,
          Some(&profile())
        ));
      }
      assert!(matches_launcher(&launcher(), root, Kind::Api, None));
      assert!(!matches_launcher(&launcher(), root, Kind::Connector, None));
      assert!(!matches_launcher(
        &launcher().replace("--bind 127.0.0.1:5558", "--bind 0.0.0.0:5558"),
        root,
        Kind::Api,
        None
      ));
      assert!(!matches_launcher(
        &launcher().replace("https://hub.example", "https://other.example"),
        root,
        Kind::Connector,
        Some(&profile())
      ));
      assert!(!matches_launcher(
        &launcher().replace("--allow-control", "--allow-control --state-dir /other"),
        root,
        Kind::Connector,
        Some(&profile())
      ));
    }
    #[test]
    fn registered_and_loaded_services_must_identify_the_same_component() {
      let runner = Path::new("/test/run-service.sh");
      let plist = Path::new("/test/service.plist");
      for kind in [Kind::Connector, Kind::Api] {
        let definition = serde_json::json!({"Label":kind.label(),"ProgramArguments":[runner,kind.argument()]});
        assert!(matches_definition(&definition, runner, kind));
        let mut changed = definition.clone();
        changed["EnvironmentVariables"] = serde_json::json!({"HOME":"/other"});
        assert!(!matches_definition(&changed, runner, kind));
        let text = format!(
          "state = waiting\npath = /test/service.plist\nprogram = /test/run-service.sh\narguments = {{\n /test/run-service.sh\n {}\n}}\n",
          kind.argument()
        );
        assert!(matches_loaded(&text, plist, runner, kind));
        assert!(!matches_loaded(
          &text.replace("/test/service.plist", "/other.plist"),
          plist,
          runner,
          kind
        ));
        let other = if kind == Kind::Api { Kind::Connector } else { Kind::Api };
        assert!(!matches_definition(&definition, runner, other));
        assert!(!matches_loaded(&text, plist, runner, other));
      }
    }
    fn service(kind: Kind) -> Service {
      Service {
        kind,
        plist: PathBuf::new(),
        target: kind.label().into(),
        status: BackgroundService {
          registered: true,
          loaded: true,
          running: true,
          verified: true,
          error: None,
        },
      }
    }
    #[tokio::test]
    async fn stop_connector_before_api_and_leave_api_intact_on_connector_failure() {
      let services = vec![service(Kind::Api), service(Kind::Connector)];
      let mut stopped = Vec::new();
      stop_services(&services, |target| {
        stopped.push(target);
        std::future::ready(Ok(()))
      })
      .await
      .unwrap();
      assert_eq!(stopped, [Kind::Connector.label(), Kind::Api.label()]);
      stopped.clear();
      assert!(
        stop_services(&services, |target| {
          stopped.push(target);
          std::future::ready(Err("failure".into()))
        })
        .await
        .is_err()
      );
      assert_eq!(stopped, [Kind::Connector.label()]);
    }
    #[tokio::test]
    async fn refuse_all_stops_if_either_component_is_unverified() {
      let mut services = vec![service(Kind::Connector), service(Kind::Api)];
      services[1].status.verified = false;
      let mut calls = 0;
      assert!(
        stop_services(&services, |_| {
          calls += 1;
          std::future::ready(Ok(()))
        })
        .await
        .is_err()
      );
      assert_eq!(calls, 0);
    }
    #[test]
    fn orphaned_api_can_be_stopped_and_unloaded_registrations_can_be_removed() {
      let mut services = vec![service(Kind::Api)];
      assert!(summary(&services).stop_supported());
      services[0].status.loaded = false;
      assert!(!summary(&services).stop_supported());
      assert!(summary(&services).remove_supported());
      services[0].status.error = Some("unknown registration".into());
      assert!(!summary(&services).remove_supported());
    }
  }
}

pub async fn inspect(state_dir: &Path, profile: Option<&HostProfile>) -> Result<BackgroundHosting, String> {
  #[cfg(target_os = "macos")]
  {
    macos::inspect(state_dir, profile).await
  }
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (state_dir, profile);
    Ok(BackgroundHosting::default())
  }
}
pub async fn manage(state_dir: &Path, profile: Option<&HostProfile>, remove: bool) -> Result<(), String> {
  #[cfg(target_os = "macos")]
  {
    macos::manage(state_dir, profile, remove).await
  }
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (state_dir, profile, remove);
    Err("Manage background hosting outside this app on this platform.".into())
  }
}
