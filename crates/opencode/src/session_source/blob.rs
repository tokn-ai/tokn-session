//! Lazy, bounded reads from OpenCode's native content-addressed media store.
use super::*;
use sha2::{Digest, Sha256};

const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;

pub struct OpenCodeBlob {
  pub mime: String,
  pub data: Vec<u8>,
  pub width: Option<u32>,
  pub height: Option<u32>,
}

/// Native stored media names bytes, never a filesystem path or server URL.
pub fn media_reference_hash(reference: &str) -> Option<&str> {
  let hash = reference.strip_prefix("media:")?;
  (hash.len() == 64
    && hash
      .bytes()
      .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
  .then_some(hash)
}

impl OpenCodeSessionSource {
  /// Read only a blob explicitly owned by this persisted session and message.
  /// Listing, normalization and incremental snapshots never call this method.
  pub fn read_blob_exact(
    &self,
    session_id: &str,
    message_id: &str,
    reference: &str,
    maximum_bytes: usize,
  ) -> Result<Option<OpenCodeBlob>, String> {
    let Some(hash) = media_reference_hash(reference) else {
      return Err("Invalid OpenCode media reference".into());
    };
    if maximum_bytes == 0 || maximum_bytes > MAX_BLOB_BYTES {
      return Err("Invalid OpenCode blob read limit".into());
    }
    if !matches!(self.flavor, SessionDatabaseFlavor::OpenCode) {
      return Ok(None);
    }
    let mut database = connect_database(&self.database_path()?)?;
    let transaction = database.transaction().map_err(|error| error.to_string())?;
    let capabilities = OpenCodeCapabilities::detect(&transaction)?;
    if !capabilities.has_v2 || load_session_row(&transaction, capabilities, session_id, self.flavor)?.is_none() {
      return Ok(None);
    }
    let blob = crate::schema::table_columns(&transaction, "blob")?;
    let ownership = crate::schema::table_columns(&transaction, "blob_ref")?;
    if blob.is_empty() && ownership.is_empty() {
      return Ok(None);
    }
    if !["hash", "mime", "bytes", "data"]
      .iter()
      .all(|column| blob.contains(*column))
      || !["session_id", "message_id", "hash", "seq"]
        .iter()
        .all(|column| ownership.contains(*column))
    {
      return Err("Unsupported OpenCode blob store schema".into());
    }
    let width = if blob.contains("width") { "b.width" } else { "null" };
    let height = if blob.contains("height") { "b.height" } else { "null" };
    let metadata = transaction.query_row(
      &format!("select b.mime,b.bytes,length(b.data),{width},{height} from blob b join blob_ref r on r.hash=b.hash join session_message m on m.id=r.message_id and m.session_id=r.session_id and m.seq=r.seq where r.session_id=?1 and r.message_id=?2 and r.hash=?3"),
      params![session_id,message_id,hash],
      |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<i64>>(3)?,row.get::<_,Option<i64>>(4)?)),
    ).optional().map_err(|error| format!("Could not verify OpenCode blob ownership: {error}"))?;
    let Some((mime, declared, actual, width, height)) = metadata else {
      return Ok(None);
    };
    let size = usize::try_from(actual).map_err(|_| "Invalid OpenCode blob size")?;
    if declared != actual
      || size > maximum_bytes
      || mime.is_empty()
      || mime.len() > 4096
      || mime.chars().any(char::is_control)
    {
      return Err("OpenCode blob exceeds its limit or has invalid metadata".into());
    }
    // The SQL bound also limits allocation if an unusual database provides a
    // dynamic value. The same read transaction fences metadata and bytes.
    let data: Vec<u8> = transaction
      .query_row(
        "select substr(data,1,?2) from blob where hash=?1",
        params![hash, (maximum_bytes + 1) as i64],
        |row| row.get(0),
      )
      .map_err(|error| error.to_string())?;
    if data.len() != size || format!("{:x}", Sha256::digest(&data)) != hash {
      return Err("OpenCode blob bytes do not match their native identity".into());
    }
    let dimension = |value: Option<i64>| {
      value
        .map(u32::try_from)
        .transpose()
        .map_err(|_| "Invalid OpenCode blob dimensions".to_owned())
    };
    Ok(Some(OpenCodeBlob {
      mime,
      data,
      width: dimension(width)?,
      height: dimension(height)?,
    }))
  }
}
