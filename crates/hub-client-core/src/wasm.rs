//! Thin browser bindings over the same native pairing and Noise state machines.
use crate::{
  pairing::{ClientAwaitingAck, ClientPairing},
  secure::{InnerMessage, MAX_PLAINTEXT, NoiseIdentity, NoiseInitiator, SecureChannel},
};
use wasm_bindgen::prelude::*;

fn error(message: String) -> JsValue {
  JsValue::from_str(&message)
}

fn unix_seconds(value: f64) -> Result<u64, JsValue> {
  if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > 9_007_199_254_740_991.0 {
    return Err(error("Unix time must be a nonnegative safe integer in seconds".into()));
  }
  Ok(value as u64)
}

/// Endpoint-owned identity. Exported secrets belong in private local storage.
#[wasm_bindgen(js_name = DeviceIdentity)]
pub struct WasmDeviceIdentity(NoiseIdentity);

#[wasm_bindgen(js_class = DeviceIdentity)]
impl WasmDeviceIdentity {
  pub fn generate() -> Result<Self, JsValue> {
    Ok(Self(NoiseIdentity::generate().map_err(error)?))
  }

  pub fn from_secret(secret: &str) -> Result<Self, JsValue> {
    Ok(Self(NoiseIdentity::import_secret(secret).map_err(error)?))
  }

  pub fn export_secret(&self) -> String {
    self.0.export_secret()
  }

  pub fn public_key(&self) -> String {
    self.0.public_key()
  }
}

#[wasm_bindgen(js_name = ClientPairing)]
pub struct WasmClientPairing {
  inner: ClientPairing,
  record: Vec<u8>,
}

#[wasm_bindgen(js_class = ClientPairing)]
impl WasmClientPairing {
  pub fn start(host_id: &str, identity: &WasmDeviceIdentity, code: &str, now_seconds: f64) -> Result<Self, JsValue> {
    let (inner, record) =
      ClientPairing::start(host_id, &identity.0, code, unix_seconds(now_seconds)?).map_err(error)?;
    Ok(Self { inner, record })
  }

  pub fn record(&self) -> Vec<u8> {
    self.record.clone()
  }

  /// Consume this attempt when authenticating the host's response.
  pub fn confirm(self, host_record: &[u8]) -> Result<WasmClientPairingConfirmation, JsValue> {
    let (inner, record) = self.inner.confirm(host_record).map_err(error)?;
    Ok(WasmClientPairingConfirmation { inner, record })
  }
}

#[wasm_bindgen(js_name = ClientPairingConfirmation)]
pub struct WasmClientPairingConfirmation {
  inner: ClientAwaitingAck,
  record: Vec<u8>,
}

#[wasm_bindgen(js_class = ClientPairingConfirmation)]
impl WasmClientPairingConfirmation {
  pub fn record(&self) -> Vec<u8> {
    self.record.clone()
  }

  /// Only a verified final acknowledgment may become a persisted host pin.
  pub fn finish(self, host_ack: &[u8]) -> Result<String, JsValue> {
    let pairing = self.inner.finish(host_ack).map_err(error)?;
    serde_json::to_string(&pairing).map_err(|failure| error(failure.to_string()))
  }
}

#[wasm_bindgen(js_name = NoiseInitiator)]
pub struct WasmNoiseInitiator {
  inner: NoiseInitiator,
  record: Vec<u8>,
}

#[wasm_bindgen(js_class = NoiseInitiator)]
impl WasmNoiseInitiator {
  pub fn start(identity: &WasmDeviceIdentity, host_public_key: &str) -> Result<Self, JsValue> {
    let mut inner = NoiseInitiator::new(&identity.0, host_public_key).map_err(error)?;
    let record = inner.start().map_err(error)?;
    Ok(Self { inner, record })
  }

  pub fn record(&self) -> Vec<u8> {
    self.record.clone()
  }

  pub fn finish(self, host_record: &[u8]) -> Result<WasmNoiseChannel, JsValue> {
    Ok(WasmNoiseChannel(self.inner.finish(host_record).map_err(error)?))
  }
}

/// Noncloneable ordered transport. Any bad incoming record permanently closes it.
#[wasm_bindgen(js_name = NoiseChannel)]
pub struct WasmNoiseChannel(SecureChannel);

#[wasm_bindgen(js_class = NoiseChannel)]
impl WasmNoiseChannel {
  pub fn remote_public_key(&self) -> String {
    self.0.remote_public_key().into()
  }

  pub fn channel_binding(&self) -> String {
    self.0.channel_binding().into()
  }

  pub fn encrypt_json(&mut self, json: &str) -> Result<Vec<u8>, JsValue> {
    if json.len() > MAX_PLAINTEXT {
      return Err(error("Secure message exceeds the Noise record limit".into()));
    }
    let message: InnerMessage =
      serde_json::from_str(json).map_err(|_| error("Invalid secure protocol message".into()))?;
    self.0.encrypt(&message).map_err(error)
  }

  pub fn decrypt_json(&mut self, record: &[u8]) -> Result<String, JsValue> {
    let message = self.0.decrypt(record).map_err(error)?;
    serde_json::to_string(&message).map_err(|failure| error(failure.to_string()))
  }

  pub fn encrypt_bytes(&mut self, bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
    self.0.encrypt_bytes(bytes).map_err(error)
  }

  pub fn decrypt_bytes(&mut self, record: &[u8]) -> Result<Vec<u8>, JsValue> {
    self.0.decrypt_bytes(record).map_err(error)
  }
}
