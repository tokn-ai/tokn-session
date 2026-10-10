//! Explicit display resources and independent source inspection.
use crate::{
  ViewerService,
  model::{EventDetail, LoadEventDetailRequest},
  updates::{SessionUpdate, SessionUpdatesRequest, UpdateLevel},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionDetailsRequest {
  Group { request: SessionUpdatesRequest },
  Tool { request: LoadEventDetailRequest },
}
#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum SessionDetails {
  Group(SessionUpdate),
  Tool(EventDetail),
}
impl ViewerService {
  pub fn load_session_details(&self, request: SessionDetailsRequest) -> Result<SessionDetails, String> {
    match request {
      SessionDetailsRequest::Group { mut request } => {
        if request.scope.as_ref().is_none_or(|scope| scope.group_keys.is_empty()) {
          return Err("Group details require explicit group interests".into());
        }
        request.level = UpdateLevel::Steps;
        request.detail_keys.clear();
        Ok(SessionDetails::Group(self.load_session_updates(request)?))
      }
      SessionDetailsRequest::Tool { request } => {
        let mut detail = self.load_event_detail(request)?;
        detail.native = None;
        Ok(SessionDetails::Tool(detail))
      }
    }
  }
  pub fn inspect_session_event(&self, request: LoadEventDetailRequest) -> Result<EventDetail, String> {
    self.load_event_detail(request)
  }
}
