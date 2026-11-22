use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Dialect {
  LegacyV2,
  Current,
}

pub(super) async fn discover(transport: &Transport) -> Result<(Dialect, Value, Option<String>), InteractionError> {
  if let Some(health) = transport.optional_json(&["api", "health"]).await? {
    if health["healthy"] != true {
      return Err(InteractionError::unavailable(
        "OpenCode native V2 endpoint is not healthy",
      ));
    }
    let document = transport.json(&["doc"]).await?;
    let version = transport
      .json::<Value>(&["global", "health"])
      .await
      .ok()
      .and_then(|value| value["version"].as_str().map(str::to_owned));
    return Ok((Dialect::LegacyV2, document, version));
  }
  let info: Value = transport.json(&["api", "info"]).await?;
  let version = info["version"]
    .as_str()
    .filter(|version| !version.is_empty())
    .ok_or_else(|| InteractionError::unsupported("OpenCode server identity is unavailable"))?
    .to_owned();
  let document = transport.json(&["openapi.json"]).await?;
  Ok((Dialect::Current, document, Some(version)))
}
