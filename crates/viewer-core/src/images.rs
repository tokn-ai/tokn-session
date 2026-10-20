//! Session-bound image resources and private input uploads.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
  collections::{HashMap, HashSet},
  fs::File,
  io::{Cursor, Read, Seek, SeekFrom, Write},
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

#[cfg(test)]
mod tests;

pub const MAX_IMAGES: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TOTAL_IMAGE_BYTES: usize = 24 * 1024 * 1024;
pub const IMAGE_CHUNK_BYTES: usize = 256 * 1024;
pub const IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageTarget {
  Session { session_key: String },
  Project { project_id: String },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadImageChunkRequest {
  pub target: ImageTarget,
  pub upload_id: String,
  pub offset: usize,
  pub total_bytes: usize,
  pub mime_type: String,
  pub name: Option<String>,
  pub data_base64: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct UploadImageChunkResponse {
  pub upload_id: String,
  pub received_bytes: usize,
  pub image_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseImageUploadRequest {
  pub target: ImageTarget,
  pub upload_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadSessionImageRequest {
  pub session_key: String,
  pub event_key: String,
  pub image_id: String,
  pub variant: ImageVariant,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageVariant {
  Thumbnail,
  Full,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionImage {
  pub mime_type: String,
  pub data_base64: String,
  pub width: u32,
  pub height: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct MessageImageSummary {
  pub image_id: String,
  /// Exact source record owning this resource when its card is assembled
  /// from several tool events. Absent for ordinary message/raw source rows.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub source_event_key: Option<String>,
  pub mime_type: Option<String>,
  pub name: Option<String>,
  pub availability: ImageAvailability,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageAvailability {
  Available,
  Unavailable,
}

/// Validated, immutable host bytes. Paths never come from input clients.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedImage {
  pub image_id: String,
  pub mime_type: String,
  pub sha256: String,
  pub path: PathBuf,
  pub bytes: Arc<Vec<u8>>,
}

const UPLOAD_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_UPLOADS: usize = 64;
const MAX_STAGED_BYTES: usize = 128 * 1024 * 1024;
const MAX_PIXELS: u64 = 32 * 1024 * 1024;

struct Upload {
  target: ImageTarget,
  total: usize,
  mime_type: String,
  name: Option<String>,
  received: usize,
  touched: Instant,
  file: Option<tempfile::NamedTempFile>,
  path: PathBuf,
  image_id: Option<String>,
  sha256: Option<String>,
  retained: bool,
}

struct ImageStoreState {
  root: PathBuf,
  _temporary_root: Option<tempfile::TempDir>,
  uploads: HashMap<String, Upload>,
}

#[derive(Clone)]
pub(crate) struct ImageStore(Arc<Mutex<ImageStoreState>>);

impl ImageStore {
  pub(crate) fn configured(root: PathBuf) -> Self {
    Self(Arc::new(Mutex::new(ImageStoreState {
      root,
      _temporary_root: None,
      uploads: HashMap::new(),
    })))
  }
  pub(crate) fn temporary() -> Result<Self, String> {
    let temporary = tempfile::tempdir().map_err(|_| "Could not create image storage")?;
    Ok(Self(Arc::new(Mutex::new(ImageStoreState {
      root: temporary.path().to_owned(),
      _temporary_root: Some(temporary),
      uploads: HashMap::new(),
    }))))
  }

  pub(crate) fn persistent(root: PathBuf) -> Result<Self, String> {
    ensure_private_directory(&root)?;
    // Files promoted for provider/history use have a companion marker. Only
    // expired, unpromoted upload files may be recovered after a process crash.
    for entry in std::fs::read_dir(&root)
      .map_err(|_| "Could not inspect image storage")?
      .flatten()
    {
      let path = entry.path();
      let expired = entry.metadata().ok().is_some_and(|metadata| {
        metadata.is_file()
          && metadata
            .modified()
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age >= UPLOAD_TTL)
      });
      if entry.file_name().to_string_lossy().starts_with("upload-")
        && path.extension().is_some_and(|extension| extension != "retained")
        && !retained_marker(&path).exists()
        && expired
      {
        let _ = std::fs::remove_file(path);
      }
    }
    Ok(Self(Arc::new(Mutex::new(ImageStoreState {
      root,
      _temporary_root: None,
      uploads: HashMap::new(),
    }))))
  }

  pub(crate) fn upload(&self, request: UploadImageChunkRequest) -> Result<UploadImageChunkResponse, String> {
    if uuid::Uuid::parse_str(&request.upload_id).is_err()
      || request.total_bytes == 0
      || request.total_bytes > MAX_IMAGE_BYTES
      || !IMAGE_MIME_TYPES.contains(&request.mime_type.as_str())
      || request
        .name
        .as_ref()
        .is_some_and(|name| name.chars().count() > 255 || name.chars().any(char::is_control))
      || request.data_base64.len() > IMAGE_CHUNK_BYTES.div_ceil(3) * 4
    {
      return Err("Invalid image upload or image exceeds its size limit".into());
    }
    let bytes = STANDARD
      .decode(&request.data_base64)
      .map_err(|_| "Invalid image upload encoding")?;
    if bytes.is_empty()
      || bytes.len() > IMAGE_CHUNK_BYTES
      || request
        .offset
        .checked_add(bytes.len())
        .is_none_or(|end| end > request.total_bytes)
    {
      return Err("Invalid image upload chunk".into());
    }
    let mut state = self.0.lock().map_err(|_| "Image storage is unavailable")?;
    state.uploads.retain(|_, upload| upload.touched.elapsed() < UPLOAD_TTL);
    if !state.uploads.contains_key(&request.upload_id) {
      if request.offset != 0 {
        return Err("Image upload expired; attach the image again".into());
      }
      let staged: usize = state
        .uploads
        .values()
        .filter(|upload| !upload.retained)
        .map(|upload| upload.total)
        .sum();
      if state.uploads.len() >= MAX_UPLOADS || staged.saturating_add(request.total_bytes) > MAX_STAGED_BYTES {
        return Err("Too many image uploads; remove unused attachments and retry".into());
      }
      ensure_private_directory(&state.root)?;
      let file = tempfile::Builder::new()
        .prefix("upload-")
        .suffix(extension_for_mime(&request.mime_type))
        .tempfile_in(&state.root)
        .map_err(|_| "Could not stage the image")?;
      let path = file.path().to_owned();
      state.uploads.insert(
        request.upload_id.clone(),
        Upload {
          target: request.target.clone(),
          total: request.total_bytes,
          mime_type: request.mime_type.clone(),
          name: request.name.clone(),
          received: 0,
          touched: Instant::now(),
          file: Some(file),
          path,
          image_id: None,
          sha256: None,
          retained: false,
        },
      );
    }
    let upload = state.uploads.get_mut(&request.upload_id).unwrap();
    if upload.target != request.target
      || upload.total != request.total_bytes
      || upload.mime_type != request.mime_type
      || upload.name != request.name
    {
      return Err("Image upload does not match this draft".into());
    }
    if request.offset < upload.received {
      // Retrying an identical chunk is safe even when the previous reply was
      // lost. Different bytes at an old offset must never mutate an image ID.
      if request.offset + bytes.len() > upload.received {
        return Err("Image upload offset does not match".into());
      }
      let mut file = File::open(&upload.path).map_err(|_| "Image upload is unavailable")?;
      file
        .seek(SeekFrom::Start(request.offset as u64))
        .map_err(|_| "Could not check image upload")?;
      let mut previous = vec![0; bytes.len()];
      file
        .read_exact(&mut previous)
        .map_err(|_| "Could not check image upload")?;
      if previous != bytes {
        return Err("Image upload chunk conflicts with earlier bytes".into());
      }
    } else if request.offset == upload.received && !upload.retained && upload.image_id.is_none() {
      let file = upload.file.as_mut().ok_or("Image upload is unavailable")?;
      file.write_all(&bytes).map_err(|_| "Could not store image upload")?;
      upload.received += bytes.len();
      if upload.received == upload.total {
        file.flush().map_err(|_| "Could not finish image upload")?;
        let complete = read_file(&upload.path)?;
        let (mime_type, _, _) = validate_raster(&complete)?;
        if mime_type != upload.mime_type {
          return Err("Image content does not match its declared format".into());
        }
        upload.sha256 = Some(format!("{:x}", Sha256::digest(&complete)));
        upload.image_id = Some(uuid::Uuid::new_v4().to_string());
      }
    } else {
      return Err("Image upload offset does not match".into());
    }
    upload.touched = Instant::now();
    Ok(UploadImageChunkResponse {
      upload_id: request.upload_id,
      received_bytes: upload.received,
      image_id: upload.image_id.clone(),
    })
  }

  pub(crate) fn release(&self, request: ReleaseImageUploadRequest) -> Result<(), String> {
    let mut state = self.0.lock().map_err(|_| "Image storage is unavailable")?;
    if let Some(upload) = state.uploads.get(&request.upload_id) {
      if upload.target != request.target {
        return Err("Image upload does not belong to this draft".into());
      }
      if upload.retained {
        return Ok(());
      }
    }
    state.uploads.remove(&request.upload_id);
    Ok(())
  }

  pub(crate) fn resolve(&self, target: &ImageTarget, image_ids: &[String]) -> Result<Vec<ResolvedImage>, String> {
    if image_ids.len() > MAX_IMAGES || image_ids.iter().collect::<HashSet<_>>().len() != image_ids.len() {
      return Err(format!("Attach at most {MAX_IMAGES} distinct images"));
    }
    let mut state = self.0.lock().map_err(|_| "Image storage is unavailable")?;
    let mut result = Vec::new();
    let mut total = 0;
    for id in image_ids {
      let upload = state
        .uploads
        .values_mut()
        .find(|upload| upload.image_id.as_ref() == Some(id))
        .ok_or("Image upload expired; attach the image again")?;
      if &upload.target != target || upload.touched.elapsed() >= UPLOAD_TTL {
        return Err("Image upload does not belong to this draft or has expired".into());
      }
      let bytes = read_file(&upload.path)?;
      let sha256 = format!("{:x}", Sha256::digest(&bytes));
      if upload.sha256.as_ref() != Some(&sha256) {
        return Err("Uploaded image changed; attach it again".into());
      }
      total += bytes.len();
      if total > MAX_TOTAL_IMAGE_BYTES {
        return Err("Attached images exceed the total size limit".into());
      }
      upload.touched = Instant::now();
      result.push(ResolvedImage {
        image_id: id.clone(),
        mime_type: upload.mime_type.clone(),
        sha256,
        path: upload.path.clone(),
        bytes: Arc::new(bytes),
      });
    }
    Ok(result)
  }

  pub(crate) fn retain(&self, image_ids: &[String]) -> Result<(), String> {
    let mut state = self.0.lock().map_err(|_| "Image storage is unavailable")?;
    for id in image_ids {
      let upload = state
        .uploads
        .values_mut()
        .find(|upload| upload.image_id.as_ref() == Some(id))
        .ok_or("Image upload is unavailable")?;
      if upload.retained {
        continue;
      }
      let marker = retained_marker(&upload.path);
      let mut options = std::fs::OpenOptions::new();
      options.write(true).create_new(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
      }
      let marker_file = options
        .open(marker)
        .map_err(|_| "Could not retain image for delivery")?;
      marker_file
        .sync_all()
        .map_err(|_| "Could not retain image for delivery")?;
      upload
        .file
        .take()
        .ok_or("Image upload is unavailable")?
        .keep()
        .map_err(|_| "Could not retain image for delivery")?;
      upload.retained = true;
    }
    Ok(())
  }
}

fn retained_marker(path: &Path) -> PathBuf {
  path.with_extension("retained")
}

fn extension_for_mime(mime: &str) -> &'static str {
  match mime {
    "image/png" => ".png",
    "image/jpeg" => ".jpg",
    "image/webp" => ".webp",
    "image/gif" => ".gif",
    _ => ".image",
  }
}

fn ensure_private_directory(root: &Path) -> Result<(), String> {
  std::fs::create_dir_all(root).map_err(|_| "Could not create private image storage")?;
  let metadata = std::fs::symlink_metadata(root).map_err(|_| "Could not inspect image storage")?;
  if !metadata.is_dir() || metadata.file_type().is_symlink() {
    return Err("Image storage must be a regular directory".into());
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
      .map_err(|_| "Could not secure image storage")?;
  }
  Ok(())
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
  let mut options = std::fs::OpenOptions::new();
  options.read(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NONBLOCK);
  }
  let file = options.open(path).map_err(|_| "Image file is unavailable")?;
  let metadata = file.metadata().map_err(|_| "Image file is unavailable")?;
  if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_IMAGE_BYTES as u64 {
    return Err("Image file is unavailable or exceeds the size limit".into());
  }
  let mut bytes = Vec::new();
  file
    .take((MAX_IMAGE_BYTES + 1) as u64)
    .read_to_end(&mut bytes)
    .map_err(|_| "Could not read image")?;
  if bytes.len() > MAX_IMAGE_BYTES {
    return Err("Image exceeds the size limit".into());
  }
  Ok(bytes)
}

fn raster_format(bytes: &[u8]) -> Result<image::ImageFormat, String> {
  if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
    return Err("Image exceeds the size limit".into());
  }
  match image::guess_format(bytes).map_err(|_| "Image format is unsupported")? {
    format @ (image::ImageFormat::Png
    | image::ImageFormat::Jpeg
    | image::ImageFormat::WebP
    | image::ImageFormat::Gif) => Ok(format),
    _ => Err("Only PNG, JPEG, WebP, and GIF images are supported".into()),
  }
}

fn decode_raster(bytes: &[u8]) -> Result<(image::ImageFormat, image::DynamicImage), String> {
  let format = raster_format(bytes)?;
  let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
    .into_dimensions()
    .map_err(|_| "Invalid image dimensions")?;
  if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
    return Err("Image dimensions exceed the supported limit".into());
  }
  let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
  let mut limits = image::Limits::default();
  limits.max_image_width = Some(16_384);
  limits.max_image_height = Some(16_384);
  limits.max_alloc = Some(160 * 1024 * 1024);
  reader.limits(limits);
  let image = reader
    .decode()
    .map_err(|_| "Image is invalid or exceeds decoder limits")?;
  Ok((format, image))
}

fn validate_raster(bytes: &[u8]) -> Result<(String, u32, u32), String> {
  let (format, image) = decode_raster(bytes)?;
  Ok((format.to_mime_type().into(), image.width(), image.height()))
}

pub(crate) fn image_response(bytes: Vec<u8>, variant: ImageVariant) -> Result<SessionImage, String> {
  let (format, image) = decode_raster(&bytes)?;
  let (mime_type, data, width, height) = match variant {
    ImageVariant::Full => (format.to_mime_type().to_owned(), bytes, image.width(), image.height()),
    ImageVariant::Thumbnail => {
      let thumbnail = image.thumbnail(image.width().min(480), image.height().min(360));
      let mut data = Cursor::new(Vec::new());
      thumbnail
        .write_to(&mut data, image::ImageFormat::Png)
        .map_err(|_| "Could not prepare image thumbnail")?;
      (
        "image/png".into(),
        data.into_inner(),
        thumbnail.width(),
        thumbnail.height(),
      )
    }
  };
  Ok(SessionImage {
    mime_type,
    data_base64: STANDARD.encode(data),
    width,
    height,
  })
}

pub(crate) fn read_source(image: &tokn_session_core::MessageImage) -> Result<Vec<u8>, String> {
  use tokn_session_core::MessageImageSource;
  let decode = |data: &str| {
    if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
      return Err("Image exceeds the size limit".to_string());
    }
    STANDARD.decode(data).map_err(|_| "Image data is invalid".to_string())
  };
  match &image.source {
    MessageImageSource::DataUrl { url } => {
      let (header, data) = url.split_once(',').ok_or("Image data URL is invalid")?;
      let mime = header
        .strip_prefix("data:")
        .and_then(|header| header.strip_suffix(";base64"))
        .filter(|mime| IMAGE_MIME_TYPES.contains(mime))
        .ok_or("Image format is unsupported")?;
      let bytes = decode(data)?;
      if raster_format(&bytes)?.to_mime_type() != mime {
        return Err("Image data does not match its format".into());
      }
      Ok(bytes)
    }
    MessageImageSource::Base64 { data } => decode(data),
    MessageImageSource::LocalFile { path } if path.is_absolute() => read_file(path),
    MessageImageSource::LocalFile { .. } => Err("Relative image paths are unavailable".into()),
    MessageImageSource::Unavailable { .. } => Err("This image reference is unavailable on the session host".into()),
  }
}

/// Binary bodies are explicit resources; even an `all` native projection must
/// not ship them repeatedly inside its ordinary JSON details.
pub(crate) fn sanitize_display_value(value: &mut serde_json::Value) {
  tokn_session_core::omit_image_resources(value);
}
