//! Bounded idempotent message admission shared by managed provider workflows.
use super::{Admission, DeliveryError, MessageModelSelection, ResolvedImage};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Default)]
pub struct ManagedMessageReceipts {
  records: HashMap<String, (String, Option<Result<Admission, DeliveryError>>)>,
}

impl ManagedMessageReceipts {
  /// `None` admits a new request. A repeat returns its retained result, or an
  /// uncertain result while its original native acknowledgement is pending.
  pub fn admit(
    &mut self,
    request_id: &str,
    fingerprint: String,
  ) -> Result<Option<Result<Admission, DeliveryError>>, DeliveryError> {
    if let Some((previous, result)) = self.records.get(request_id) {
      if previous != &fingerprint {
        return Err(DeliveryError::not_sent(
          "Message request ID was already used with different content or model intent",
        ));
      }
      return Ok(Some(result.clone().unwrap_or_else(|| {
        Err(DeliveryError::uncertain(
          "The original message is still awaiting native acknowledgement",
        ))
      })));
    }
    if self.records.len() >= 1024 {
      return Err(DeliveryError::not_sent(
        "Managed runtime message receipt limit reached; resume this session to continue",
      ));
    }
    self.records.insert(request_id.to_owned(), (fingerprint, None));
    Ok(None)
  }

  pub fn complete(&mut self, request_id: &str, result: Result<Admission, DeliveryError>) {
    if let Some(receipt) = self.records.get_mut(request_id) {
      receipt.1 = Some(result);
    }
  }
}

pub fn managed_message_fingerprint(
  text: &str,
  images: &[ResolvedImage],
  selection: Option<&MessageModelSelection>,
) -> Result<String, String> {
  let images = images
    .iter()
    .map(|image| (&image.image_id, &image.sha256))
    .collect::<Vec<_>>();
  let bytes = if selection.is_some() {
    serde_json::to_vec(&(text, images, selection)).map_err(|error| error.to_string())?
  } else if images.is_empty() {
    text.as_bytes().to_vec()
  } else {
    serde_json::to_vec(&(text, images)).map_err(|error| error.to_string())?
  };
  Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn repeats_observe_pending_and_completed_admission_without_resubmitting() {
    let mut receipts = ManagedMessageReceipts::default();
    assert!(receipts.admit("id", "fingerprint".into()).unwrap().is_none());
    assert!(
      receipts
        .admit("id", "different".into())
        .unwrap_err()
        .message
        .contains("different content")
    );
    assert!(
      receipts
        .admit("id", "fingerprint".into())
        .unwrap()
        .unwrap()
        .unwrap_err()
        .may_have_been_sent
    );
    receipts.complete("id", Ok(Admission::Started));
    assert!(matches!(
      receipts.admit("id", "fingerprint".into()).unwrap(),
      Some(Ok(Admission::Started))
    ));
  }
}
