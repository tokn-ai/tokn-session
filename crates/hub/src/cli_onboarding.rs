use serde::{Deserialize, Serialize};
use std::{
  fs::OpenOptions,
  io::{IsTerminal, Write},
  path::{Path, PathBuf},
};
use tokn_session_hub::{
  connector::ConnectorConfig,
  onboarding::{self, HostProfile},
  pairing::{TOTP_PERIOD, TotpSecret},
  secure::NoiseIdentity,
};
use url::Url;
use zeroize::Zeroizing;

pub use tokn_session_hub::host_setup::HostOptions;
use tokn_session_hub::host_setup::read_seed;

pub fn prepare_host(options: HostOptions) -> Result<ConnectorConfig, String> {
  let prepared = tokn_session_hub::host_setup::prepare_host(options)?;
  let profile = prepared.config.paired.as_ref().expect("paired host setup");
  eprintln!("Host ID: {}", profile.host_id);
  eprintln!("Machine reference: {}", prepared.reference);
  if let Some(origin) = &prepared.passkey_origin {
    eprintln!("Host passkey browser origin: {origin}");
  }
  eprintln!("Open the Hub viewer and connect with this host ID and your authenticator code.");
  if prepared.is_new_secret {
    if std::io::stderr().is_terminal() {
      display_authenticator(
        &onboarding::read_totp_secret(&profile.state_file)?,
        &prepared.config.name,
        Some(&prepared.reference),
      )?;
    } else {
      eprintln!(
        "Authenticator saved locally. Run `tokn-session-hub authenticator` in a terminal with the same --state-dir to display its setup QR."
      );
    }
  }
  Ok(prepared.config)
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

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
