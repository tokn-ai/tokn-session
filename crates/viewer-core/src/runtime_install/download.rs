use super::*;
use sha2::{Digest, Sha256};
use std::{
  fs::{self, File},
  io::{Read, Write},
  time::Duration,
};
use tokio::io::AsyncWriteExt;

const DOWNLOAD_LIMIT: u64 = 512 * 1024 * 1024;
const EXPANDED_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const ENTRY_LIMIT: usize = 4096;

/// Bound every decompressed byte, including GNU/PAX extension records and tar
/// padding that the archive parser consumes without yielding a file entry.
struct ArchiveReader<R> {
  inner: R,
  remaining: u64,
  cancel: CancellationToken,
}
impl<R: Read> Read for ArchiveReader<R> {
  fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
    if self.cancel.is_cancelled() {
      return Err(std::io::Error::other("Agent installation cancelled"));
    }
    let allowed = buffer.len().min(self.remaining.saturating_add(1) as usize);
    let count = self.inner.read(&mut buffer[..allowed])?;
    if count as u64 > self.remaining {
      return Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "The decompressed agent archive exceeds its size limit",
      ));
    }
    self.remaining -= count as u64;
    Ok(count)
  }
}

fn client() -> Result<reqwest::Client, String> {
  reqwest::Client::builder()
    .user_agent("tokn-session-agent-installer")
    .connect_timeout(Duration::from_secs(20))
    .timeout(Duration::from_secs(15 * 60))
    .redirect(reqwest::redirect::Policy::custom(|attempt| {
      if attempt.previous().len() > 5 || attempt.url().scheme() != "https" {
        attempt.stop()
      } else {
        attempt.follow()
      }
    }))
    .build()
    .map_err(|_| "Could not initialize agent download networking".into())
}
pub(super) async fn json(url: &str, cancel: &CancellationToken) -> Result<serde_json::Value, String> {
  let mut response = tokio::select! { biased; _ = cancel.cancelled() => return Err("Agent installation cancelled".into()), response = client()?.get(url).send() => response.map_err(|_| "Could not contact the official agent distribution service")? }
    .error_for_status().map_err(|_| "The official agent distribution service returned an error; retry later")?;
  let mut bytes = Vec::new();
  while let Some(chunk) = tokio::select! { biased; _ = cancel.cancelled() => return Err("Agent installation cancelled".into()), chunk = response.chunk() => chunk.map_err(|_| "Could not read official agent metadata")? }
  {
    if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
      return Err("Official agent metadata exceeds its size limit".into());
    }
    bytes.extend_from_slice(&chunk);
  }
  serde_json::from_slice(&bytes).map_err(|_| "The official distribution returned invalid agent metadata".into())
}
pub(super) fn asset_name(provider: Provider) -> Result<String, String> {
  asset_for(
    provider,
    std::env::consts::OS,
    std::env::consts::ARCH,
    cfg!(target_env = "musl"),
  )
}
fn asset_for(provider: Provider, os: &str, arch: &str, musl: bool) -> Result<String, String> {
  let arch = match arch {
    "aarch64" => "aarch64",
    "x86_64" => "x86_64",
    _ => return Err("Agent downloads are not supported for this host architecture".into()),
  };
  match provider {
    Provider::Codex => {
      let target = match os {
        "macos" => format!("{arch}-apple-darwin"),
        "linux" => format!("{arch}-unknown-linux-musl"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        _ => return Err("Codex downloads are not supported on this host OS".into()),
      };
      Ok(if os == "windows" {
        format!("codex-{target}.exe.zip")
      } else {
        format!("codex-{target}.tar.gz")
      })
    }
    Provider::OpenCode => {
      let arch = if arch == "aarch64" { "arm64" } else { "x64-baseline" };
      let (os, extension) = match os {
        "macos" => ("darwin", "zip"),
        "linux" => ("linux", "tar.gz"),
        "windows" => ("windows", "zip"),
        _ => return Err("OpenCode downloads are not supported on this host OS".into()),
      };
      Ok(format!(
        "opencode-{os}-{arch}{}.{extension}",
        if musl && os == "linux" { "-musl" } else { "" }
      ))
    }
    _ => Err("This agent uses a host Node/npm package installation".into()),
  }
}
pub(super) async fn install(
  manager: &RuntimeInstallManager,
  id: &str,
  provider: Provider,
  staging: &Path,
  cancel: &CancellationToken,
) -> Result<InstallRecord, String> {
  let repo = match provider {
    Provider::Codex => "openai/codex",
    Provider::OpenCode => "anomalyco/opencode",
    _ => return Err("This agent uses npm".into()),
  };
  let name = asset_name(provider)?;
  let release = json(&format!("https://api.github.com/repos/{repo}/releases/latest"), cancel).await?;
  let tag = release
    .get("tag_name")
    .and_then(|v| v.as_str())
    .filter(|v| !v.is_empty() && v.len() < 128)
    .ok_or("The official agent release has no valid version")?;
  let version = tag
    .strip_prefix("rust-v")
    .or_else(|| tag.strip_prefix('v'))
    .unwrap_or(tag)
    .to_owned();
  let assets = release
    .get("assets")
    .and_then(|v| v.as_array())
    .ok_or("The official agent release contains no download assets")?;
  let matches = assets
    .iter()
    .filter(|asset| asset.get("name").and_then(|v| v.as_str()) == Some(&name))
    .collect::<Vec<_>>();
  if matches.len() != 1 {
    return Err(
      "The latest official release has no unique download for this host; retry after the upstream release is complete"
        .into(),
    );
  }
  let asset = matches[0];
  let expected = asset
    .get("digest")
    .and_then(|v| v.as_str())
    .and_then(|v| v.strip_prefix("sha256:"))
    .filter(|v| v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit()))
    .ok_or("The official agent asset has no SHA-256 digest; refusing an unverified download")?
    .to_ascii_lowercase();
  let size = asset
    .get("size")
    .and_then(|v| v.as_u64())
    .filter(|size| *size > 0 && *size <= DOWNLOAD_LIMIT)
    .ok_or("The official agent download exceeds its size limit")?;
  let url = asset
    .get("browser_download_url")
    .and_then(|v| v.as_str())
    .filter(|url| url.starts_with(&format!("https://github.com/{repo}/releases/download/")))
    .ok_or("The official release asset has an unexpected download URL")?;
  manager.progress(id, "downloading", Some(0), Some(size));
  let archive = tempfile::NamedTempFile::new_in(staging).map_err(|_| "Could not create a private download file")?;
  let mut file = tokio::fs::File::from_std(archive.reopen().map_err(|_| "Could not open download file")?);
  let mut response = tokio::select! { biased; _ = cancel.cancelled() => return Err("Agent installation cancelled".into()), response = client()?.get(url).send() => response.map_err(|_| "Could not download the official agent release")? }
    .error_for_status().map_err(|_| "The official agent download returned an error; retry later")?;
  let mut digest = Sha256::new();
  let mut downloaded = 0u64;
  let mut reported = 0u64;
  while let Some(chunk) = tokio::select! { biased; _ = cancel.cancelled() => return Err("Agent installation cancelled".into()), chunk = response.chunk() => chunk.map_err(|_| "The agent download was interrupted")? }
  {
    downloaded += chunk.len() as u64;
    if downloaded > size || downloaded > DOWNLOAD_LIMIT {
      return Err("The agent download exceeded its advertised size".into());
    }
    digest.update(&chunk);
    file
      .write_all(&chunk)
      .await
      .map_err(|_| "Could not save the agent download; check available disk space")?;
    if downloaded - reported >= 256 * 1024 {
      manager.progress(id, "downloading", Some(downloaded), Some(size));
      reported = downloaded;
    }
  }
  file
    .flush()
    .await
    .map_err(|_| "Could not finish saving the agent download")?;
  drop(file);
  verify_download(downloaded, size, &format!("{:x}", digest.finalize()), &expected)?;
  manager.progress(id, "installing", Some(downloaded), Some(size));
  let path = archive.path().to_path_buf();
  let destination = staging.join("native");
  let destination_clone = destination.clone();
  let name_clone = name.clone();
  let token = cancel.clone();
  tokio::task::spawn_blocking(move || extract(&path, &name_clone, &destination_clone, &token))
    .await
    .map_err(|_| "Could not unpack the official agent download")??;
  check_cancel(cancel)?;
  let executable = find_executable(&destination, provider, &name)?;
  Ok(InstallRecord {
    provider,
    version,
    generation: String::new(),
    entrypoint: PathBuf::from("native").join(executable),
    node: false,
    digest: format!("sha256:{expected}"),
  })
}
fn find_executable(directory: &Path, provider: Provider, archive: &str) -> Result<PathBuf, String> {
  let stem = archive.trim_end_matches(".tar.gz").trim_end_matches(".zip");
  let name = if provider == Provider::Codex {
    "codex"
  } else {
    "opencode"
  };
  let mut candidates = vec![
    PathBuf::from(name),
    PathBuf::from(format!("{name}.exe")),
    PathBuf::from(stem),
  ];
  candidates.dedup();
  let mut found = candidates.into_iter().filter(|path| directory.join(path).is_file());
  let selected = found
    .next()
    .ok_or("The official archive did not contain the expected agent executable")?;
  if found.next().is_some() {
    return Err("The official archive contained ambiguous agent executables".into());
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(directory.join(&selected), fs::Permissions::from_mode(0o755))
      .map_err(|_| "Could not mark agent software executable")?;
  }
  Ok(selected)
}
fn archive_path(value: &Path) -> Result<PathBuf, String> {
  use std::path::Component;
  if value.to_string_lossy().contains(['\\', ':']) {
    return Err("The agent archive contains an unsafe path".into());
  }
  let mut normalized = PathBuf::new();
  for part in value.components() {
    match part {
      Component::Normal(part) => normalized.push(part),
      Component::CurDir => {}
      _ => return Err("The agent archive contains an unsafe path".into()),
    }
  }
  Ok(normalized)
}
fn verify_download(downloaded: u64, size: u64, actual: &str, expected: &str) -> Result<(), String> {
  if downloaded != size || actual != expected {
    return Err(
      "The agent download failed SHA-256 or size verification; the previous installation was retained".into(),
    );
  }
  Ok(())
}
fn extract(archive: &Path, name: &str, destination: &Path, cancel: &CancellationToken) -> Result<(), String> {
  fs::create_dir(destination).map_err(|_| "Could not prepare the native agent directory")?;
  let mut expanded = 0u64;
  if name.ends_with(".zip") {
    let mut zip = zip::ZipArchive::new(File::open(archive).map_err(|_| "Could not read agent archive")?)
      .map_err(|_| "The agent ZIP archive is invalid")?;
    if zip.len() > ENTRY_LIMIT {
      return Err("The agent archive contains too many files".into());
    }
    for i in 0..zip.len() {
      check_cancel(cancel)?;
      let mut entry = zip.by_index(i).map_err(|_| "Could not read agent ZIP entry")?;
      let path = PathBuf::from(
        entry
          .name()
          .map_err(|_| "The agent ZIP entry name is invalid")?
          .as_ref(),
      );
      let path = archive_path(&path)?;
      let file_type = entry.unix_mode().unwrap_or(0) & 0o170000;
      if !matches!(file_type, 0 | 0o100000 | 0o040000) {
        return Err("The agent archive contains a link or unsupported file type".into());
      }
      let declared = entry.size();
      expanded = expanded
        .checked_add(declared)
        .filter(|size| *size <= EXPANDED_LIMIT)
        .ok_or("The expanded agent archive exceeds its size limit")?;
      if entry.is_dir() {
        fs::create_dir_all(destination.join(&path)).map_err(|_| "Could not create agent archive directory")?;
      } else {
        copy_file(&mut entry, &destination.join(path), declared, 0o755, cancel)?;
      }
    }
  } else {
    let gzip = flate2::read::GzDecoder::new(File::open(archive).map_err(|_| "Could not read agent archive")?);
    let mut tar = tar::Archive::new(ArchiveReader {
      inner: gzip,
      remaining: EXPANDED_LIMIT,
      cancel: cancel.clone(),
    });
    for (i, entry) in tar
      .entries()
      .map_err(|_| "The agent tar archive is invalid")?
      .enumerate()
    {
      check_cancel(cancel)?;
      if i >= ENTRY_LIMIT {
        return Err("The agent archive contains too many files".into());
      }
      let mut entry = entry.map_err(|_| "Could not read agent archive entry")?;
      let path = entry
        .path()
        .map_err(|_| "The agent archive path is invalid")?
        .into_owned();
      let path = archive_path(&path)?;
      let kind = entry.header().entry_type();
      if !kind.is_file() && !kind.is_dir() {
        return Err("The agent archive contains a link or unsupported file type".into());
      }
      let declared = entry.size();
      expanded = expanded
        .checked_add(declared)
        .filter(|size| *size <= EXPANDED_LIMIT)
        .ok_or("The expanded agent archive exceeds its size limit")?;
      if kind.is_dir() {
        fs::create_dir_all(destination.join(path)).map_err(|_| "Could not create agent archive directory")?;
      } else {
        let mode = entry.header().mode().unwrap_or(0o644);
        copy_file(&mut entry, &destination.join(path), declared, mode, cancel)?;
      }
    }
  }
  Ok(())
}
fn copy_file(
  reader: &mut impl Read,
  path: &Path,
  limit: u64,
  mode: u32,
  cancel: &CancellationToken,
) -> Result<(), String> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).map_err(|_| "Could not create agent archive directory")?;
  }
  let mut options = fs::OpenOptions::new();
  options.write(true).create_new(true);
  let mut file = options
    .open(path)
    .map_err(|_| "Could not create agent archive file, or archive contained duplicate paths")?;
  let mut copied = 0u64;
  let mut buffer = [0u8; 64 * 1024];
  loop {
    check_cancel(cancel)?;
    let count = reader
      .read(&mut buffer)
      .map_err(|_| "Could not unpack agent archive file")?;
    if count == 0 {
      break;
    }
    copied += count as u64;
    if copied > limit {
      return Err("The agent archive entry exceeds its size limit".into());
    }
    file
      .write_all(&buffer[..count])
      .map_err(|_| "Could not save unpacked agent software")?;
  }
  if copied != limit {
    return Err("The agent archive entry does not match its declared size".into());
  }
  file.flush().map_err(|_| "Could not save unpacked agent software")?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
      path,
      fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 }),
    )
    .map_err(|_| "Could not set agent file permissions")?;
  }
  #[cfg(not(unix))]
  let _ = mode;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn current_official_asset_recipes_are_exact_and_conservative() {
    assert_eq!(
      asset_for(Provider::Codex, "macos", "aarch64", false).unwrap(),
      "codex-aarch64-apple-darwin.tar.gz"
    );
    assert_eq!(
      asset_for(Provider::Codex, "windows", "x86_64", false).unwrap(),
      "codex-x86_64-pc-windows-msvc.exe.zip"
    );
    assert_eq!(
      asset_for(Provider::OpenCode, "linux", "x86_64", true).unwrap(),
      "opencode-linux-x64-baseline-musl.tar.gz"
    );
    assert_eq!(
      asset_for(Provider::OpenCode, "macos", "x86_64", false).unwrap(),
      "opencode-darwin-x64-baseline.zip"
    );
    assert!(asset_for(Provider::OpenCode, "linux", "riscv64", false).is_err());
  }
  #[test]
  fn archive_paths_normalize_current_directory_but_reject_escape() {
    assert_eq!(
      archive_path(Path::new("./opencode")).unwrap(),
      PathBuf::from("opencode")
    );
    for path in ["../escape", "/absolute", "a/../../escape", "C:\\escape", "..\\escape"] {
      assert!(archive_path(Path::new(path)).is_err(), "{path}");
    }
  }
  #[test]
  fn digest_and_exact_download_size_are_both_required() {
    assert!(verify_download(3, 3, "abcd", "abcd").is_ok());
    assert!(verify_download(3, 4, "abcd", "abcd").is_err());
    assert!(verify_download(3, 3, "abcd", "abce").is_err());
  }
  #[test]
  fn tar_reader_caps_metadata_and_padding_and_cancels_before_reading() {
    let token = CancellationToken::new();
    let mut reader = ArchiveReader {
      inner: &b"gnu-extension-and-padding"[..],
      remaining: 8,
      cancel: token.clone(),
    };
    let mut bytes = Vec::new();
    assert!(reader.read_to_end(&mut bytes).is_err());
    token.cancel();
    let mut reader = ArchiveReader {
      inner: &b"fixture"[..],
      remaining: 64,
      cancel: token,
    };
    assert!(reader.read(&mut [0; 1]).is_err());
  }
  fn tar_fixture(path: &Path, link: bool) {
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
      File::create(path).unwrap(),
      flate2::Compression::default(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    if link {
      header.set_entry_type(tar::EntryType::Symlink);
      header.set_size(0);
      header.set_link_name("outside").unwrap();
      header.set_cksum();
      tar.append_data(&mut header, "opencode", std::io::empty()).unwrap();
    } else {
      header.set_size(7);
      header.set_cksum();
      tar.append_data(&mut header, "./opencode", &b"fixture"[..]).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
  }
  #[test]
  fn extraction_accepts_official_style_path_and_rejects_links() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("agent.tar.gz");
    tar_fixture(&archive, false);
    extract(
      &archive,
      "agent.tar.gz",
      &directory.path().join("valid"),
      &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(fs::read(directory.path().join("valid/opencode")).unwrap(), b"fixture");
    tar_fixture(&archive, true);
    assert!(
      extract(
        &archive,
        "agent.tar.gz",
        &directory.path().join("invalid"),
        &CancellationToken::new()
      )
      .is_err()
    );
  }
}
