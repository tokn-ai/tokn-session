use super::*;
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub(super) struct NodeTools {
  pub node: PathBuf,
  npm_cli: PathBuf,
}
pub(super) fn node_tools() -> Result<NodeTools, String> {
  if cfg!(windows) {
    return Err("Tokn-managed Pi and DSH downloads are not yet supported on Windows; configure an existing runtime profile instead".into());
  }
  let path = std::env::var_os("PATH");
  let node = crate::rpc_runtime::discovery::resolve_executable(None, path.as_deref(), "node")
    .map_err(|_| "Install Node.js and npm on this host before downloading Pi or DSH")?;
  let npm = crate::rpc_runtime::discovery::resolve_executable(None, path.as_deref(), "npm")
    .map_err(|_| "Install npm on this host before downloading Pi or DSH")?;
  let npm_cli = if npm.extension().is_some_and(|extension| extension == "js") {
    npm
  } else {
    npm
      .parent()
      .ok_or("Could not locate the host npm CLI")?
      .join("node_modules/npm/bin/npm-cli.js")
  };
  if !npm_cli.is_file() {
    return Err("This host's npm launcher does not expose npm-cli.js. Use a standard Node/npm installation or configure an existing agent runtime profile.".into());
  }
  Ok(NodeTools { node, npm_cli })
}
fn recipe(provider: Provider) -> Result<(&'static str, &'static str), String> {
  match provider {
    Provider::Pi => Ok(("@earendil-works/pi-coding-agent", "pi")),
    Provider::Dsh => Ok(("@deepseek-ai/dsh", "dsh")),
    _ => Err("This provider uses a native release download".into()),
  }
}
pub(super) async fn install(
  manager: &RuntimeInstallManager,
  id: &str,
  provider: Provider,
  staging: &Path,
  cancel: &CancellationToken,
) -> Result<InstallRecord, String> {
  let tools = node_tools()?;
  let (package, command) = recipe(provider)?;
  let metadata = download::json(&format!("https://registry.npmjs.org/{package}/latest"), cancel).await?;
  let version = metadata
    .get("version")
    .and_then(|v| v.as_str())
    .filter(|v| !v.is_empty() && v.len() < 128 && v.chars().all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c)))
    .ok_or("The official package registry returned an invalid version")?;
  if metadata.get("name").and_then(|v| v.as_str()) != Some(package) {
    return Err("The official package registry returned a different package".into());
  }
  let integrity = metadata
    .pointer("/dist/integrity")
    .and_then(|v| v.as_str())
    .filter(|v| v.starts_with("sha512-") && v.len() < 256)
    .ok_or("The official package registry did not provide SHA-512 package integrity")?;
  let entrypoint = metadata
    .get("bin")
    .and_then(|v| v.get(command))
    .and_then(|v| v.as_str())
    .or_else(|| metadata.get("bin").and_then(|v| v.as_str()))
    .ok_or("The agent package does not advertise its CLI entrypoint")?;
  let entrypoint = PathBuf::from(entrypoint.trim_start_matches("./"));
  if !filesystem::relative_path(&entrypoint) {
    return Err("The agent package advertised an unsafe CLI entrypoint".into());
  }
  let user_config = staging.join(".npmrc-user");
  let global_config = staging.join(".npmrc-global");
  std::fs::write(&user_config, b"")
    .and_then(|_| std::fs::write(&global_config, b""))
    .map_err(|_| "Could not prepare isolated npm configuration")?;
  std::fs::write(
    staging.join("package.json"),
    br#"{"name":"tokn-managed-agent","version":"1.0.0","private":true}"#,
  )
  .map_err(|_| "Could not prepare the local package installation")?;
  manager.progress(id, "installing", None, None);
  let mut process = Command::new(&tools.node);
  process
    .arg(&tools.npm_cli)
    .args([
      "install",
      "--global=false",
      "--location=project",
      "--omit=dev",
      "--engine-strict",
      "--no-audit",
      "--no-fund",
      "--save-exact",
      "--package-lock=true",
      "--registry=https://registry.npmjs.org/",
    ])
    .arg(format!("--userconfig={}", user_config.display()))
    .arg(format!("--globalconfig={}", global_config.display()))
    .arg("--cache")
    .arg(staging.join(".npm-cache"))
    .arg("--prefix")
    .arg(staging)
    .arg(format!("{package}@{version}"))
    .current_dir(staging);
  // Public official packages never need the user's registry credentials, global
  // npm prefix, or Node injection settings. Keep ordinary provider auth alone.
  for (key, _) in std::env::vars_os() {
    let name = key.to_string_lossy().to_ascii_lowercase();
    if name.starts_with("npm_config_") || name == "node_options" || name == "node_path" {
      process.env_remove(key);
    }
  }
  let mut paths = vec![
    tools
      .node
      .parent()
      .ok_or("The host Node executable has no directory")?
      .to_path_buf(),
  ];
  paths.extend(
    std::env::var_os("PATH")
      .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
      .unwrap_or_default(),
  );
  process.env(
    "PATH",
    std::env::join_paths(paths).map_err(|_| "Could not prepare the host Node environment")?,
  );
  run(&mut process, cancel, Duration::from_secs(15 * 60)).await?;
  if !storage_within_limit(staging) {
    return Err(
      "The local npm installation exceeded its storage limit; the previous agent installation was retained".into(),
    );
  }
  check_cancel(cancel)?;
  let package_directory = staging.join("node_modules").join(package);
  let installed: serde_json::Value = bounded_json_file(&package_directory.join("package.json"))?;
  if installed.get("name").and_then(|v| v.as_str()) != Some(package)
    || installed.get("version").and_then(|v| v.as_str()) != Some(version)
  {
    return Err("npm installed a different agent package or version".into());
  }
  let lock = bounded_json_file(&staging.join("package-lock.json"))?;
  let locked = lock
    .get("packages")
    .and_then(|v| v.get(format!("node_modules/{package}")))
    .and_then(|v| v.get("integrity"))
    .and_then(|v| v.as_str());
  if locked != Some(integrity) {
    return Err("The installed package integrity does not match the official registry".into());
  }
  // Never retain npm's download cache or credentials in the executable tree.
  let _ = std::fs::remove_dir_all(staging.join(".npm-cache"));
  let _ = std::fs::remove_file(user_config);
  let _ = std::fs::remove_file(global_config);
  Ok(InstallRecord {
    provider,
    version: version.to_owned(),
    generation: String::new(),
    entrypoint: PathBuf::from("node_modules").join(package).join(entrypoint),
    node: true,
    digest: integrity.to_owned(),
  })
}
fn bounded_json_file(path: &Path) -> Result<serde_json::Value, String> {
  use std::io::Read;
  let mut bytes = Vec::new();
  std::fs::File::open(path)
    .map_err(|_| "Installed package metadata is missing")?
    .take(8 * 1024 * 1024 + 1)
    .read_to_end(&mut bytes)
    .map_err(|_| "Could not read installed package metadata")?;
  if bytes.len() > 8 * 1024 * 1024 {
    return Err("Installed package metadata exceeds its size limit".into());
  }
  serde_json::from_slice(&bytes).map_err(|_| "Installed package metadata is invalid".into())
}
async fn collect(mut pipe: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>, String> {
  let mut buffer = [0; 8192];
  let mut output = Vec::new();
  let mut total = 0usize;
  loop {
    let count = pipe
      .read(&mut buffer)
      .await
      .map_err(|_| "Could not read the installation process output")?;
    if count == 0 {
      return Ok(output);
    }
    total += count;
    if total > 2 * 1024 * 1024 {
      return Err("The installation process exceeded its output limit".into());
    }
    if output.len() < 64 * 1024 {
      output.extend_from_slice(&buffer[..count.min(64 * 1024 - output.len())]);
    }
  }
}
/// All process diagnostics stay local. Public errors are fixed strings because
/// npm diagnostics may contain credential-bearing URLs and host-private paths.
pub(super) async fn run(command: &mut Command, cancel: &CancellationToken, timeout: Duration) -> Result<(), String> {
  let directory = command.as_std().get_current_dir().map(Path::to_path_buf);
  command
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  #[cfg(unix)]
  {
    command.process_group(0);
  }
  let mut child = command
    .spawn()
    .map_err(|_| "Could not start the host Node/npm process")?;
  let pid = child.id();
  let mut stdout = tokio::spawn(collect(child.stdout.take().ok_or("npm stdout unavailable")?));
  let mut stderr = tokio::spawn(collect(child.stderr.take().ok_or("npm stderr unavailable")?));
  let result = tokio::select! {
    biased;
    _ = cancel.cancelled() => Err("Agent installation cancelled".to_owned()),
    _ = tokio::time::sleep(timeout) => Err("The host npm installation timed out; check this host's network and retry".to_owned()),
    error = monitor_storage(directory) => Err(error),
    result = async {
      let (status, _out, err) = tokio::try_join!(
        async { child.wait().await.map_err(|_| "Could not wait for the host npm process".to_owned()) },
        async { (&mut stdout).await.map_err(|_| "Could not collect npm output".to_owned())? },
        async { (&mut stderr).await.map_err(|_| "Could not collect npm diagnostics".to_owned())? },
      )?;
      if !status.success() {
        if String::from_utf8_lossy(&err).contains("EBADENGINE") { return Err("This host's Node/npm version is incompatible with the latest agent; update Node/npm on the host and retry".into()); }
        return Err("The host npm package installation failed; check Node/npm, network access and available disk space, then retry".into());
      }
      Ok(())
    } => result,
  };
  if result.is_err() {
    #[cfg(unix)]
    if let Some(pid) = pid {
      unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
      }
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.kill().await;
    let _ = child.wait().await;
    stdout.abort();
    stderr.abort();
  }
  result
}

async fn monitor_storage(directory: Option<PathBuf>) -> String {
  let Some(directory) = directory else {
    return std::future::pending().await;
  };
  loop {
    tokio::time::sleep(Duration::from_secs(2)).await;
    let directory = directory.clone();
    if !matches!(
      tokio::task::spawn_blocking(move || storage_within_limit(&directory)).await,
      Ok(true)
    ) {
      return "The local npm installation exceeded its storage limit; the previous agent installation was retained"
        .into();
    }
  }
}
fn storage_within_limit(root: &Path) -> bool {
  let mut pending = vec![(root.to_path_buf(), 0u8)];
  let mut files = 0usize;
  let mut bytes = 0u64;
  while let Some((path, depth)) = pending.pop() {
    if depth > 64 {
      return false;
    }
    let entries = match std::fs::read_dir(path) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
      Err(_) => return false,
    };
    for entry in entries {
      let Ok(entry) = entry else { return false };
      let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
        continue;
      };
      files += 1;
      bytes = bytes.saturating_add(metadata.len());
      if files > 200_000 || bytes > 4 * 1024 * 1024 * 1024 {
        return false;
      }
      if metadata.is_dir() && !metadata.file_type().is_symlink() {
        pending.push((entry.path(), depth + 1));
      }
    }
  }
  true
}

#[cfg(all(test, unix))]
mod tests {
  use super::*;
  #[tokio::test]
  async fn child_diagnostics_are_bounded_and_do_not_expose_private_data() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf 'private-host-path secret-token' >&2; exit 1"]);
    let error = run(&mut command, &CancellationToken::new(), Duration::from_secs(3))
      .await
      .unwrap_err();
    assert!(!error.contains("secret-token"));
    assert!(!error.contains("private-host-path"));
    let mut command = Command::new("/bin/sh");
    command.args([
      "-c",
      "while :; do printf '0123456789012345678901234567890123456789012345678901234567890123456789'; done",
    ]);
    let error = tokio::time::timeout(
      Duration::from_secs(5),
      run(&mut command, &CancellationToken::new(), Duration::from_secs(10)),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.contains("output limit"));
  }
  #[tokio::test]
  async fn cancellation_reaps_installer_before_releasing_its_future() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("child-pid");
    let mut command = Command::new("/bin/sh");
    command
      .arg("-c")
      .arg("echo $$ > \"$1\"; while :; do sleep 1; done")
      .arg("fixture")
      .arg(&marker);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let task = tokio::spawn(async move { run(&mut command, &token, Duration::from_secs(5)).await });
    let pid = tokio::time::timeout(Duration::from_secs(2), async {
      loop {
        if let Some(pid) = std::fs::read_to_string(&marker)
          .ok()
          .and_then(|value| value.trim().parse::<i32>().ok())
        {
          break pid;
        }
        tokio::task::yield_now().await;
      }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(task.await.unwrap().unwrap_err().contains("cancelled"));
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
  }
}
