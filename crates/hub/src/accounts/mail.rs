use std::{ffi::OsString, path::PathBuf, process::Stdio, time::Duration};

use tokio::{io::AsyncWriteExt, process::Command};

pub(super) struct Sendmail {
  from: String,
  program: PathBuf,
}

impl Sendmail {
  pub(super) fn from_env() -> Result<Option<Self>, String> {
    Self::from_config(
      std::env::var_os("TOKN_HUB_ACCOUNT_FROM"),
      std::env::var_os("TOKN_HUB_ACCOUNT_SENDMAIL"),
    )
  }

  fn from_config(from: Option<OsString>, program: Option<OsString>) -> Result<Option<Self>, String> {
    let Some(from) = from else { return Ok(None) };
    let from = from
      .to_str()
      .and_then(mailbox)
      .ok_or("TOKN_HUB_ACCOUNT_FROM must be a valid sender address")?;
    let program = program.map_or_else(|| PathBuf::from("/usr/sbin/sendmail"), PathBuf::from);
    if !program.is_absolute() {
      return Err("TOKN_HUB_ACCOUNT_SENDMAIL must name an absolute executable path".into());
    }
    Ok(Some(Self { from, program }))
  }

  pub(super) async fn deliver(&self, email: &str, code: &str) -> Result<(), ()> {
    let mut process = Command::new(&self.program)
      .args(["-oi", "-f", &self.from, "-t"])
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .kill_on_drop(true)
      .spawn()
      .map_err(|_| ())?;
    let mut stdin = process.stdin.take().ok_or(())?;
    let message = format!(
      "From: {}\r\nTo: {email}\r\nSubject: Your Tokn Hub sign-in code\r\n\r\nYour Tokn Hub sign-in code is {code}.\r\nIt expires in 10 minutes.\r\n",
      self.from
    );
    tokio::time::timeout(Duration::from_secs(10), async {
      stdin.write_all(message.as_bytes()).await?;
      drop(stdin);
      process.wait().await
    })
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?
    .success()
    .then_some(())
    .ok_or(())
  }
}

/// Accept ordinary addr-spec mailboxes and preserve the local part because some
/// servers distinguish its case. Exclude syntax that could create more headers
/// or recipients in the sendmail message.
pub(super) fn mailbox(value: &str) -> Option<String> {
  let value = value.trim();
  let (local, domain) = value.split_once('@')?;
  if value.len() > 254
    || local.is_empty()
    || local.len() > 64
    || local.starts_with('.')
    || local.ends_with('.')
    || local.contains("..")
    || !local
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-' | b'%'))
    || !domain.split('.').all(|label| {
      !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
  {
    return None;
  }
  Some(format!("{local}@{}", domain.to_ascii_lowercase()))
}

#[cfg(test)]
impl Sendmail {
  pub(super) fn test_delivery() -> Self {
    Self {
      from: "hub@example.test".into(),
      program: PathBuf::from("/usr/bin/true"),
    }
  }
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn sends_a_single_message_through_the_configured_mailer() {
  use std::{fs, os::unix::fs::PermissionsExt};

  let directory = tempfile::tempdir().unwrap();
  let program = directory.path().join("sendmail");
  fs::write(&program, "#!/bin/sh\ncat >\"$0.message\"\n").unwrap();
  fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
  let transport = Sendmail::from_config(Some("hub@example.test".into()), Some(program.into_os_string()))
    .unwrap()
    .unwrap();
  transport.deliver("person@example.test", "123456").await.unwrap();
  let message = fs::read_to_string(directory.path().join("sendmail.message")).unwrap();
  assert!(message.starts_with("From: hub@example.test\r\nTo: person@example.test\r\n"));
  assert!(message.contains("\r\n\r\nYour Tokn Hub sign-in code is 123456.\r\n"));
}
