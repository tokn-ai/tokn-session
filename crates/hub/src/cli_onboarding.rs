use serde::{Deserialize, Serialize};
use std::{
  fs::{self, OpenOptions},
  io::{IsTerminal, Read, Write},
  path::{Path, PathBuf},
};
use tokn_session_hub::{
  connector::{self, ConnectorConfig, PairedHostConfig},
  onboarding::{self, HostProfile},
  pairing::{TOTP_PERIOD, TotpSecret},
  secure::NoiseIdentity,
};
use url::Url;
use zeroize::Zeroizing;

pub struct HostOptions {
  pub hub: Option<Url>,
  pub name: Option<String>,
  pub viewer_url: Option<Url>,
  pub passkey_origin: Option<String>,
  pub state_dir: PathBuf,
  pub totp_secret_file: Option<PathBuf>,
  pub viewer_token: Option<String>,
  pub allow_control: Option<bool>,
  pub insecure_loopback: bool,
  pub ice_servers: Option<Vec<String>>,
}

pub fn prepare_host(options: HostOptions) -> Result<ConnectorConfig, String> {
  let profile_path = options.state_dir.join("host.json");
  let previous = HostProfile::load(&profile_path)?;
  let hub = options
    .hub
    .or_else(|| previous.as_ref().and_then(|profile| Url::parse(&profile.hub_url).ok()))
    .ok_or("First setup requires --hub https://your-hub.example")?;
  let local_url = options
    .viewer_url
    .or_else(|| {
      previous
        .as_ref()
        .and_then(|profile| Url::parse(&profile.viewer_url).ok())
    })
    .unwrap_or_else(|| Url::parse("http://127.0.0.1:5558").unwrap());
  let passkey_origin = options
    .passkey_origin
    .or_else(|| previous.as_ref().and_then(|profile| profile.passkey_origin.clone()));
  let passkey_origin = match passkey_origin {
    Some(origin) => Some(
      tokn_session_hub::host_passkeys::validate_origin(&origin)?
        .origin()
        .ascii_serialization(),
    ),
    None => {
      let origin = onboarding::canonical_hub(&hub)?;
      tokn_session_hub::host_passkeys::validate_origin(&origin)
        .ok()
        .map(|origin| origin.origin().ascii_serialization())
    }
  };
  let profile = HostProfile {
    version: 1,
    host_id: previous
      .as_ref()
      .map(|profile| profile.host_id.clone())
      .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    hub_url: onboarding::canonical_hub(&hub)?,
    name: options
      .name
      .or_else(|| previous.as_ref().map(|profile| profile.name.clone()))
      .unwrap_or_else(|| "Session host".into()),
    viewer_url: local_url.to_string(),
    allow_control: options
      .allow_control
      .unwrap_or_else(|| previous.as_ref().is_some_and(|profile| profile.allow_control)),
    insecure_loopback: options.insecure_loopback || previous.as_ref().is_some_and(|profile| profile.insecure_loopback),
    passkey_origin,
    ice_servers: options.ice_servers.unwrap_or_else(|| {
      previous
        .as_ref()
        .map(|profile| profile.ice_servers.clone())
        .unwrap_or_default()
    }),
  };
  let key_file = options.state_dir.join("host-enrollment.key");
  let noise_key_file = options.state_dir.join("host-noise.key");
  let state_file = options.state_dir.join("host-access.json");
  if previous.is_none() && [&key_file, &noise_key_file].iter().any(|path| path.exists()) {
    return Err("Host configuration is missing for existing keys; restore host.json before reconnecting".into());
  }
  let config = ConnectorConfig {
    hub_url: hub,
    local_url,
    key_file: key_file.clone(),
    name: profile.name.clone(),
    local_token: options.viewer_token,
    allow_control: profile.allow_control,
    insecure_loopback: profile.insecure_loopback,
    ice_servers: profile.ice_servers.clone(),
    secure: None,
    paired: Some(PairedHostConfig {
      host_id: profile.host_id.clone(),
      noise_key_file: noise_key_file.clone(),
      state_file: state_file.clone(),
    }),
  };
  config.validate()?;
  if previous.is_some()
    && [&key_file, &noise_key_file, &state_file]
      .iter()
      .any(|path| !path.is_file())
  {
    return Err("Saved host keys or pairing state are missing; restore them before reconnecting".into());
  }
  if options.totp_secret_file.is_some() && state_file.try_exists().map_err(|e| e.to_string())? {
    return Err("Authenticator is already configured; importing must not replace existing device trust".into());
  }
  let is_new_secret = !state_file.try_exists().map_err(|e| e.to_string())?;
  // Validate user input and existing private state before creating identities.
  // An invalid import must be retryable without leaving a half-created host.
  let new_secret = if is_new_secret {
    Some(match options.totp_secret_file {
      Some(path) => read_seed(&path)?,
      None => TotpSecret::generate(),
    })
  } else {
    onboarding::read_totp_secret(&state_file)?;
    onboarding::validate_host_passkey_origin(&state_file, profile.passkey_origin.as_deref())?;
    None
  };
  connector::initialize_identity(&key_file)?;
  let noise_identity = NoiseIdentity::load_or_create(&noise_key_file)?;
  if let Some(secret) = new_secret {
    onboarding::initialize_host_access(&state_file, &secret)?;
  }
  profile.save(&profile_path)?;
  eprintln!("Host ID: {}", profile.host_id);
  eprintln!("Machine reference: {}@{}", profile.host_id, noise_identity.public_key());
  eprintln!("Open the Hub viewer and connect with this host ID and your authenticator code.");
  if let Some(origin) = &profile.passkey_origin {
    eprintln!("Host passkey browser origin: {origin}");
  }
  if is_new_secret {
    if std::io::stderr().is_terminal() {
      display_authenticator(
        &onboarding::read_totp_secret(&state_file)?,
        &profile.name,
        Some(&format!("{}@{}", profile.host_id, noise_identity.public_key())),
      )?;
    } else {
      eprintln!(
        "Authenticator saved locally. Run `tokn-session-hub authenticator` in a terminal with the same --state-dir to display its setup QR."
      );
    }
  }
  Ok(config)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientProfile {
  version: u8,
  hub_url: String,
  insecure_loopback: bool,
}

pub fn prepare_client(state_dir: &Path, hub: Option<Url>, insecure_loopback: bool) -> Result<(Url, bool), String> {
  let path = state_dir.join("client.json");
  let previous: Option<ClientProfile> = onboarding::read_optional(&path)?;
  if previous.as_ref().is_some_and(|value| value.version != 1) {
    return Err("Unsupported client configuration version".into());
  }
  let hub = hub
    .or_else(|| previous.as_ref().and_then(|profile| Url::parse(&profile.hub_url).ok()))
    .ok_or("First setup requires --hub https://your-hub.example")?;
  let insecure_loopback = insecure_loopback || previous.as_ref().is_some_and(|profile| profile.insecure_loopback);
  let hub_url = onboarding::canonical_hub(&hub)?;
  let loopback = match hub.host() {
    Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
    Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
    Some(url::Host::Domain("localhost")) => true,
    _ => false,
  };
  if !matches!(hub.scheme(), "https" | "wss") && !(insecure_loopback && loopback) {
    return Err("Hub requires HTTPS; unencrypted access requires --insecure-loopback and a loopback address".into());
  }
  onboarding::save_configuration(
    &path,
    &ClientProfile {
      version: 1,
      hub_url,
      insecure_loopback,
    },
  )?;
  Ok((hub, insecure_loopback))
}

pub fn authenticator(
  state_dir: &Path,
  import_file: Option<PathBuf>,
  export_file: Option<PathBuf>,
) -> Result<(), String> {
  let path = state_dir.join("host-access.json");
  if !path.try_exists().map_err(|e| e.to_string())? && HostProfile::load(&state_dir.join("host.json"))?.is_some() {
    return Err(
      "Saved host pairing state is missing; restore host-access.json instead of creating a new authenticator".into(),
    );
  }
  if import_file.is_some() && path.try_exists().map_err(|e| e.to_string())? {
    return Err("Authenticator is already configured; existing device trust was preserved".into());
  }
  if !path.try_exists().map_err(|e| e.to_string())? {
    let secret = match import_file {
      Some(path) => read_seed(&path)?,
      None => TotpSecret::generate(),
    };
    onboarding::initialize_host_access(&path, &secret)?;
  }
  let secret = onboarding::read_totp_secret(&path)?;
  if let Some(output) = export_file {
    let encoded = Zeroizing::new(secret.to_base32());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
      .open(&output)
      .map_err(|e| format!("Could not create secret export: {e}"))?;
    file
      .write_all(encoded.as_bytes())
      .and_then(|_| file.sync_all())
      .map_err(|e| e.to_string())?;
    eprintln!(
      "Authenticator secret exported to {}. Transfer it through a trusted channel.",
      output.display()
    );
    return Ok(());
  }
  let profile = HostProfile::load(&state_dir.join("host.json"))?;
  let label = profile
    .as_ref()
    .map(|profile| profile.name.as_str())
    .unwrap_or("My hosts");
  let reference = profile
    .as_ref()
    .map(|profile| {
      let identity = NoiseIdentity::load(&state_dir.join("host-noise.key"))?;
      Ok::<_, String>(format!("{}@{}", profile.host_id, identity.public_key()))
    })
    .transpose()?;
  display_authenticator(&secret, label, reference.as_deref())
}

fn display_authenticator(secret: &TotpSecret, label: &str, reference: Option<&str>) -> Result<(), String> {
  if !std::io::stderr().is_terminal() {
    return Err(
      "Display authenticator setup in a local terminal, or use --export-file to create a private seed file".into(),
    );
  }
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_err(|_| "Invalid host clock")?
    .as_secs();
  eprint!("{}", render_authenticator(secret, label, reference, now)?.as_str());
  Ok(())
}

fn render_authenticator(
  secret: &TotpSecret,
  label: &str,
  reference: Option<&str>,
  now: u64,
) -> Result<Zeroizing<String>, String> {
  use std::fmt::Write;
  let uri = Zeroizing::new(secret.provisioning_uri(label)?);
  let qr = qrcode::QrCode::new(uri.as_bytes()).map_err(|_| "Could not render authenticator QR")?;
  let mut output = Zeroizing::new(String::new());
  if let Some(reference) = reference {
    writeln!(output, "Machine reference: {reference}").map_err(|e| e.to_string())?;
  }
  writeln!(
    output,
    "Scan this QR with your authenticator. It contains a secret; keep it off the Hub."
  )
  .map_err(|e| e.to_string())?;
  writeln!(output, "{}", qr.render::<qrcode::render::unicode::Dense1x2>().build()).map_err(|e| e.to_string())?;
  writeln!(
    output,
    "Full setup URI (same secret and settings as the QR): {}",
    uri.as_str()
  )
  .map_err(|e| e.to_string())?;
  writeln!(
    output,
    "Use the QR or full URI with an authenticator that supports SHA-256 TOTP."
  )
  .map_err(|e| e.to_string())?;
  writeln!(
    output,
    "Manual setup key: {}",
    Zeroizing::new(secret.to_base32()).as_str()
  )
  .map_err(|e| e.to_string())?;
  writeln!(
    output,
    "Manual settings: Time based (TOTP) · SHA256 · 6 digits · 30 seconds"
  )
  .map_err(|e| e.to_string())?;
  writeln!(
    output,
    "The manual key contains only the secret. Set the algorithm explicitly; a SHA-1 default produces different codes."
  )
  .map_err(|e| e.to_string())?;
  writeln!(
    output,
    "Current TOTP: {} (valid for {}s)",
    Zeroizing::new(secret.code_at(now)).as_str(),
    TOTP_PERIOD - now % TOTP_PERIOD
  )
  .map_err(|e| e.to_string())?;
  writeln!(
    output,
    "Compare this code with your authenticator; rerun this command for a fresh code."
  )
  .map_err(|e| e.to_string())?;
  writeln!(
    output,
    "If codes differ, check the algorithm, digits, period, and device clocks. If your app cannot use SHA-256, use a compatible authenticator."
  )
  .map_err(|e| e.to_string())?;
  Ok(output)
}

fn read_seed(path: &Path) -> Result<TotpSecret, String> {
  let metadata = fs::symlink_metadata(path).map_err(|e| format!("Could not inspect authenticator import: {e}"))?;
  if !metadata.is_file() {
    return Err("Authenticator import must be a private regular file".into());
  }
  let mut options = OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
  }
  let file = options
    .open(path)
    .map_err(|e| format!("Could not open authenticator import: {e}"))?;
  let metadata = file.metadata().map_err(|e| e.to_string())?;
  if !metadata.is_file() || metadata.len() > 256 {
    return Err("Authenticator import must be a Base32 seed of at most 256 bytes".into());
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no memory side effects.
    if metadata.mode() & 0o777 != 0o600 || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
      return Err("Authenticator import must be owned by the current user with mode 0600 and no hard links".into());
    }
  }
  let mut bytes = Zeroizing::new(String::new());
  file
    .take(257)
    .read_to_string(&mut bytes)
    .map_err(|_| "Authenticator import must contain Base32 text")?;
  TotpSecret::from_base32(&bytes)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn authenticator_display_includes_reference_and_rfc_sha256_verification_code() {
    let secret = TotpSecret::from_base32("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZA").unwrap();
    let reference = "11111111-1111-4111-8111-111111111111@verified-host-key";
    let output = render_authenticator(&secret, "Workstation", Some(reference), 59).unwrap();
    assert!(output.contains(&format!("Machine reference: {reference}")));
    assert!(output.contains("Current TOTP: 119246 (valid for 1s)"));
    assert!(output.contains("SHA256 · 6 digits · 30 seconds"));
    let uri = output
      .lines()
      .find_map(|line| line.strip_prefix("Full setup URI (same secret and settings as the QR): "))
      .unwrap();
    assert_eq!(uri, secret.provisioning_uri("Workstation").unwrap());
    let uri = Url::parse(uri).unwrap();
    let settings = uri.query_pairs().collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(settings["secret"], secret.to_base32());
    assert_eq!(settings["algorithm"], "SHA256");
    assert_eq!(settings["digits"], "6");
    assert_eq!(settings["period"], "30");
    let standalone = render_authenticator(&secret, "My hosts", None, 60).unwrap();
    assert!(!standalone.contains("Machine reference:"));
    assert!(standalone.contains("valid for 30s"));
  }

  #[test]
  fn saved_host_reconnects_without_options_and_missing_keys_do_not_reset_trust() {
    let directory = tempfile::tempdir().unwrap();
    let setup = |hub| HostOptions {
      hub,
      name: None,
      viewer_url: None,
      passkey_origin: None,
      ice_servers: None,
      state_dir: directory.path().into(),
      totp_secret_file: None,
      viewer_token: None,
      allow_control: None,
      insecure_loopback: false,
    };
    let first = prepare_host(setup(Some(Url::parse("https://hub.example").unwrap()))).unwrap();
    let second = prepare_host(setup(None)).unwrap();
    assert_eq!(
      first.paired.as_ref().unwrap().host_id,
      second.paired.as_ref().unwrap().host_id
    );
    assert_eq!(first.hub_url, second.hub_url);
    let mut direct = setup(None);
    direct.ice_servers = Some(vec!["stun:stun.example:3478".into()]);
    assert_eq!(
      prepare_host(direct).unwrap().ice_servers,
      vec!["stun:stun.example:3478"]
    );
    assert_eq!(
      prepare_host(setup(None)).unwrap().ice_servers,
      vec!["stun:stun.example:3478"]
    );
    let mut clear = setup(None);
    clear.ice_servers = Some(Vec::new());
    assert!(prepare_host(clear).unwrap().ice_servers.is_empty());
    assert!(prepare_host(setup(None)).unwrap().ice_servers.is_empty());

    let mut enable = setup(None);
    enable.allow_control = Some(true);
    assert!(prepare_host(enable).unwrap().allow_control);
    assert!(prepare_host(setup(None)).unwrap().allow_control);
    let mut disable = setup(None);
    disable.allow_control = Some(false);
    assert!(!prepare_host(disable).unwrap().allow_control);
    assert!(!prepare_host(setup(None)).unwrap().allow_control);
    fs::remove_file(&first.paired.unwrap().noise_key_file).unwrap();
    assert!(prepare_host(setup(None)).is_err());
    fs::remove_file(directory.path().join("host-access.json")).unwrap();
    assert!(authenticator(directory.path(), None, Some(directory.path().join("export.txt"))).is_err());
    assert!(!directory.path().join("host-access.json").exists());
    fs::remove_file(directory.path().join("host.json")).unwrap();
    assert!(prepare_host(setup(Some(Url::parse("https://hub.example").unwrap()))).is_err());
  }

  #[test]
  fn invalid_remote_http_does_not_create_configuration_or_keys() {
    let directory = tempfile::tempdir().unwrap();
    assert!(prepare_client(directory.path(), Some(Url::parse("http://example.com").unwrap()), true).is_err());
    assert!(!directory.path().join("client.json").exists());
  }

  #[test]
  fn invalid_seed_import_can_be_corrected_without_partial_identity_files() {
    let directory = tempfile::tempdir().unwrap();
    let seed = directory.path().join("seed.txt");
    let setup = || HostOptions {
      hub: Some(Url::parse("https://hub.example").unwrap()),
      name: None,
      viewer_url: None,
      passkey_origin: None,
      ice_servers: None,
      state_dir: directory.path().join("host"),
      totp_secret_file: Some(seed.clone()),
      viewer_token: None,
      allow_control: None,
      insecure_loopback: false,
    };
    assert!(prepare_host(setup()).is_err());
    assert!(!directory.path().join("host/host-enrollment.key").exists());
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600);
    }
    options
      .open(&seed)
      .unwrap()
      .write_all(TotpSecret::generate().to_base32().as_bytes())
      .unwrap();
    assert!(prepare_host(setup()).is_ok());
  }
}
