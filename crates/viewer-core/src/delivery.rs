//! Tool display payloads and independent source inspection.
use crate::{
  ViewerService,
  model::{EventDetail, LoadEventDetailRequest},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionDetailsRequest {
  Tool { request: LoadEventDetailRequest },
}

impl ViewerService {
  pub fn load_session_details(&self, resource: SessionDetailsRequest) -> Result<EventDetail, String> {
    let SessionDetailsRequest::Tool { request } = resource;
    let mut detail = self.load_event_detail(request)?;
    detail.native = None;
    Ok(detail)
  }

  pub fn inspect_session_event(&self, request: LoadEventDetailRequest) -> Result<EventDetail, String> {
    self.load_event_detail(request)
  }
}
