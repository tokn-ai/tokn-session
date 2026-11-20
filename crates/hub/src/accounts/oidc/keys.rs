use std::{
  collections::HashMap,
  time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{
  Algorithm, DecodingKey,
  jwk::{AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use reqwest::Client;
use url::Url;

use super::{AccountError, CACHE_LIFETIME, invalid_identity, read_json, unavailable, visible_ascii};

const REFRESH_COOLDOWN: Duration = Duration::from_secs(10);
const MAX_KEYS: usize = 16;

#[derive(Default)]
pub(super) struct Cache {
  endpoint: Option<Url>,
  keys: HashMap<String, (Algorithm, DecodingKey)>,
  fetched_at: Option<Instant>,
  last_attempt: Option<Instant>,
}

impl Cache {
  pub(super) async fn get(
    &mut self,
    client: &Client,
    endpoint: &Url,
    kid: &str,
    algorithm: Algorithm,
  ) -> Result<DecodingKey, AccountError> {
    if self.endpoint.as_ref() != Some(endpoint) {
      *self = Self {
        endpoint: Some(endpoint.clone()),
        ..Self::default()
      };
    }
    if self.fetched_at.is_some_and(|time| time.elapsed() < CACHE_LIFETIME)
      && let Some(key) = self.lookup(kid, algorithm)
    {
      return Ok(key);
    }
    // Unknown kids can trigger one early refresh for provider key rotation.
    if self.last_attempt.is_some_and(|time| time.elapsed() < REFRESH_COOLDOWN) {
      return Err(invalid_identity());
    }
    self.last_attempt = Some(Instant::now());
    let jwks: JwkSet = read_json(client.get(endpoint.clone())).await?;
    self.keys = signing_keys(jwks)?;
    self.fetched_at = Some(Instant::now());
    self.lookup(kid, algorithm).ok_or_else(invalid_identity)
  }

  fn lookup(&self, kid: &str, algorithm: Algorithm) -> Option<DecodingKey> {
    self
      .keys
      .get(kid)
      .filter(|(key_algorithm, _)| *key_algorithm == algorithm)
      .map(|(_, key)| key.clone())
  }

  #[cfg(test)]
  pub(super) fn seeded(endpoint: Url, jwks: JwkSet) -> Result<Self, AccountError> {
    Ok(Self {
      endpoint: Some(endpoint),
      keys: signing_keys(jwks)?,
      fetched_at: Some(Instant::now()),
      last_attempt: Some(Instant::now()),
    })
  }
}

fn signing_keys(jwks: JwkSet) -> Result<HashMap<String, (Algorithm, DecodingKey)>, AccountError> {
  if jwks.keys.is_empty() || jwks.keys.len() > MAX_KEYS {
    return Err(unavailable());
  }
  let mut keys = HashMap::new();
  for jwk in jwks.keys {
    let Some(algorithm) = signing_algorithm(&jwk) else {
      continue;
    };
    let key = DecodingKey::from_jwk(&jwk).map_err(|_| unavailable())?;
    let kid = jwk
      .common
      .key_id
      .filter(|value| visible_ascii(value, 128))
      .ok_or_else(unavailable)?;
    if keys.insert(kid, (algorithm, key)).is_some() {
      return Err(unavailable());
    }
  }
  if keys.is_empty() {
    return Err(unavailable());
  }
  Ok(keys)
}

fn signing_algorithm(jwk: &Jwk) -> Option<Algorithm> {
  if jwk
    .common
    .public_key_use
    .as_ref()
    .is_some_and(|usage| *usage != PublicKeyUse::Signature)
    || jwk
      .common
      .key_operations
      .as_ref()
      .is_some_and(|ops| !ops.contains(&KeyOperations::Verify))
  {
    return None;
  }
  let (algorithm, key_algorithm) = match &jwk.algorithm {
    AlgorithmParameters::EllipticCurve(ec) => match ec.curve {
      EllipticCurve::P256 if ec.x.len() == 43 && ec.y.len() == 43 => (Algorithm::ES256, KeyAlgorithm::ES256),
      EllipticCurve::P384 if ec.x.len() == 64 && ec.y.len() == 64 => (Algorithm::ES384, KeyAlgorithm::ES384),
      _ => return None,
    },
    AlgorithmParameters::RSA(rsa) if rsa.e == "AQAB" => {
      let modulus = URL_SAFE_NO_PAD.decode(&rsa.n).ok()?;
      if !(256..=512).contains(&modulus.len()) || modulus[0] < 128 {
        return None;
      }
      (Algorithm::RS256, KeyAlgorithm::RS256)
    }
    _ => return None,
  };
  (jwk.common.key_algorithm == Some(key_algorithm)).then_some(algorithm)
}
