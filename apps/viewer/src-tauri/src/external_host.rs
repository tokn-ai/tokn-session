//! Control only the installed, verified per-user connector service. Never guess a PID.
use std::path::Path;
use tokn_session_hub::onboarding::HostProfile;

#[cfg(target_os = "macos")]
mod macos {
  use super::*;
  use serde_json::Value;
  use std::{os::unix::fs::MetadataExt, time::Duration};
  use tokio::process::Command;

  const LABEL: &str = "com.tokn.hub-connector";
  struct Service {
    target: String,
  }
  // SAFETY: geteuid reads the current effective user without accessing Rust memory.
  fn uid() -> u32 {
    unsafe { libc::geteuid() }
  }
  async fn owned_file(path: &Path) -> Result<(), String> {
    let metadata = tokio::fs::symlink_metadata(path)
      .await
      .map_err(|_| "Connector service files are unavailable")?;
    if !metadata.is_file() || metadata.uid() != uid() || metadata.mode() & 0o022 != 0 || metadata.len() > 64 * 1024 {
      return Err("Connector service files must be owned by this user and not writable by others".into());
    }
    Ok(())
  }
  async fn output(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    tokio::time::timeout(
      Duration::from_secs(3),
      Command::new(program).args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| "Connector service check timed out")?
    .map_err(|_| "Could not inspect connector service".to_string())
  }
  fn matches_definition(value: &Value, runner: &Path) -> bool {
    value["Label"].as_str() == Some(LABEL)
      && value["ProgramArguments"]
        .as_array()
        .is_some_and(|args| args.len() == 2 && args[0].as_str() == runner.to_str() && args[1].as_str() == Some("hub"))
      && value
        .get("Program")
        .is_none_or(|program| program.as_str() == runner.to_str())
  }
  fn matches_loaded(text: &str, plist: &Path, runner: &Path) -> bool {
    let field = |name: &str| text.lines().find_map(|line| line.trim().strip_prefix(name));
    let arguments = text
      .split("arguments = {\n")
      .nth(1)
      .and_then(|body| body.split('}').next());
    field("state = ") == Some("running")
      && field("pid = ")
        .and_then(|pid| pid.parse::<u32>().ok())
        .is_some_and(|pid| pid > 0)
      && field("path = ") == plist.to_str()
      && field("program = ") == runner.to_str()
      && arguments.is_some_and(|args| {
        let args: Vec<_> = args.lines().map(str::trim).filter(|arg| !arg.is_empty()).collect();
        args.len() == 2 && Some(args[0]) == runner.to_str() && args[1] == "hub"
      })
  }
  fn matches_launcher(script: &str, binary: &Path, profile: &HostProfile) -> bool {
    // Match the deployed launcher exactly, including its state-affecting options.
    // Other launchers require manual stopping rather than shell interpretation.
    let name = profile
      .name
      .replace('\\', "\\\\")
      .replace('"', "\\\"")
      .replace('$', "\\$")
      .replace('`', "\\`");
    let expected = format!(
      "hub) exec {} connect --hub {} --name \"{}\" --viewer-url {}{} ;;",
      binary.display(),
      profile.hub_url.trim_end_matches('/'),
      name,
      profile.viewer_url.trim_end_matches('/'),
      if profile.allow_control { " --allow-control" } else { "" }
    );
    script.lines().any(|line| line.trim() == expected)
  }
  async fn verified(state_dir: &Path, profile: &HostProfile) -> Result<Service, String> {
    let home = dirs::home_dir().ok_or("Cannot resolve connector service directory")?;
    if state_dir != home.join(".tokn/hub") {
      return Err("This connector state directory is not managed by the installed service".into());
    }
    let root = home.join(".local/share/tokn-host");
    let runner = root.join("run-service.sh");
    let plist = home.join(format!("Library/LaunchAgents/{LABEL}.plist"));
    owned_file(&runner).await?;
    owned_file(&plist).await?;
    let script = tokio::fs::read_to_string(&runner)
      .await
      .map_err(|_| "Could not inspect connector launcher")?;
    if !matches_launcher(&script, &root.join("current/tokn-session-hub"), profile) {
      return Err("Installed connector launcher does not match this saved host configuration".into());
    }
    let definition = output(
      "/usr/bin/plutil",
      &[
        "-convert",
        "json",
        "-o",
        "-",
        plist.to_str().ok_or("Invalid connector path")?,
      ],
    )
    .await?;
    let value: Value =
      serde_json::from_slice(&definition.stdout).map_err(|_| "Could not inspect connector service definition")?;
    if !definition.status.success() || !matches_definition(&value, &runner) {
      return Err("Connector service definition is not recognized".into());
    }
    let target = format!("gui/{}/{LABEL}", uid());
    let loaded = output("/bin/launchctl", &["print", &target]).await?;
    if !loaded.status.success() || !matches_loaded(&String::from_utf8_lossy(&loaded.stdout), &plist, &runner) {
      return Err("Loaded connector service does not match the installed launcher".into());
    }
    Ok(Service { target })
  }
  pub async fn available(state_dir: &Path, profile: &HostProfile) -> bool {
    verified(state_dir, profile).await.is_ok()
  }
  pub async fn stop(state_dir: &Path, profile: &HostProfile) -> Result<(), String> {
    let service = verified(state_dir, profile).await?;
    let result = output("/bin/launchctl", &["bootout", &service.target]).await?;
    if !result.status.success() {
      return Err("Could not unload the installed connector service. Stop it manually and retry.".into());
    }
    Ok(())
  }
  #[cfg(test)]
  mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn deployed_launcher_matches_only_this_hosts_configuration() {
      let profile = HostProfile {
        version: 1,
        host_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        hub_url: "https://hub.example/".into(),
        name: "Studio Mac".into(),
        viewer_url: "http://127.0.0.1:5558/".into(),
        allow_control: true,
        insecure_loopback: false,
        passkey_origin: None,
        ice_servers: Vec::new(),
      };
      let binary = Path::new("/test/current/tokn-session-hub");
      let launcher = "  hub) exec /test/current/tokn-session-hub connect --hub https://hub.example --name \"Studio Mac\" --viewer-url http://127.0.0.1:5558 --allow-control ;;";
      assert!(matches_launcher(launcher, binary, &profile));
      assert!(!matches_launcher(
        &launcher.replace(" --allow-control", " --allow-control --state-dir /other"),
        binary,
        &profile
      ));
      assert!(!matches_launcher(
        &launcher.replace("https://hub.example", "https://other.example"),
        binary,
        &profile
      ));
      assert!(!matches_launcher(
        &launcher.replace("connect", "client"),
        binary,
        &profile
      ));
    }
    #[test]
    fn service_definition_and_loaded_arguments_must_match_exactly() {
      let runner = PathBuf::from("/test/run-service.sh");
      let plist = PathBuf::from("/test/connector.plist");
      let value = serde_json::json!({"Label": LABEL, "ProgramArguments": [runner, "hub"]});
      assert!(matches_definition(&value, &runner));
      assert!(!matches_definition(
        &serde_json::json!({"Label": LABEL, "ProgramArguments": [runner, "api"]}),
        &runner
      ));
      assert!(!matches_definition(
        &serde_json::json!({"Label": LABEL, "ProgramArguments": [runner, "hub", "--state-dir", "/other"]}),
        &runner
      ));
      let loaded = "state = running\npid = 123\npath = /test/connector.plist\nprogram = /test/run-service.sh\narguments = {\n /test/run-service.sh\n hub\n}\n";
      assert!(matches_loaded(loaded, &plist, &runner));
      assert!(!matches_loaded(
        &loaded.replace("state = running", "state = waiting"),
        &plist,
        &runner
      ));
      assert!(!matches_loaded(&loaded.replace(" hub\n", " api\n"), &plist, &runner));
      assert!(!matches_loaded(
        &loaded.replace("/test/connector.plist", "/other.plist"),
        &plist,
        &runner
      ));
    }
  }
}

pub async fn available(state_dir: &Path, profile: &HostProfile) -> bool {
  #[cfg(target_os = "macos")]
  {
    macos::available(state_dir, profile).await
  }
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (state_dir, profile);
    false
  }
}
pub async fn stop(state_dir: &Path, profile: &HostProfile) -> Result<(), String> {
  #[cfg(target_os = "macos")]
  {
    macos::stop(state_dir, profile).await
  }
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (state_dir, profile);
    Err("Stop this external connector manually; its service cannot be controlled by this app.".into())
  }
}
