//! Cloudflare Access: accept a request that carries a valid Access token.
//!
//! Behind an Access application, Cloudflare adds a signed JWT (`Cf-Access-Jwt-Assertion`) to every
//! request of a signed-in caller. The token is checked here against the team's public keys, the
//! application's audience tag, the issuer and the expiry.

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const KEY_LIFETIME: Duration = Duration::from_secs(3_600);
/// Unknown key ids trigger a refetch (keys rotate), but not more often than this.
const REFETCH_PAUSE: Duration = Duration::from_secs(30);

pub struct Verifier {
    issuer: String,
    audience: String,
    certs_url: String,
    keys: Mutex<(Option<Instant>, HashMap<String, DecodingKey>)>,
}

fn setting(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

impl Verifier {
    /// Built from `CONTEXT_BROKER_ACCESS_TEAM_DOMAIN` (`<team>.cloudflareaccess.com`) and
    /// `CONTEXT_BROKER_ACCESS_AUD` (the application's audience tag); `None` when neither is set.
    pub fn from_env() -> Result<Option<Self>, String> {
        let (team, audience) = match (setting("CONTEXT_BROKER_ACCESS_TEAM_DOMAIN"), setting("CONTEXT_BROKER_ACCESS_AUD")) {
            (None, None) => return Ok(None),
            (Some(team), Some(audience)) => (team, audience),
            _ => return Err("set both CONTEXT_BROKER_ACCESS_TEAM_DOMAIN and CONTEXT_BROKER_ACCESS_AUD".into()),
        };
        let issuer = format!("https://{}", team.trim_start_matches("https://").trim_end_matches('/'));
        let certs_url = setting("CONTEXT_BROKER_ACCESS_CERTS_URL").unwrap_or_else(|| format!("{issuer}/cdn-cgi/access/certs"));
        Ok(Some(Self { issuer, audience, certs_url, keys: Mutex::new((None, HashMap::new())) }))
    }

    fn fetch_keys(&self) -> Result<HashMap<String, DecodingKey>, String> {
        let mut response = ureq::get(&self.certs_url).call().map_err(|e| format!("fetch {}: {e}", self.certs_url))?;
        let body = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
        let document: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        Ok(document["keys"]
            .as_array()
            .map(|keys| {
                keys.iter()
                    .filter_map(|key| {
                        let decoding = DecodingKey::from_rsa_components(key["n"].as_str()?, key["e"].as_str()?).ok()?;
                        Some((key["kid"].as_str()?.to_string(), decoding))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn key(&self, id: &str) -> Option<DecodingKey> {
        let mut keys = self.keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let age = keys.0.map(|fetched| fetched.elapsed());
        let stale = age.is_none_or(|age| age > KEY_LIFETIME);
        let unknown = !keys.1.contains_key(id) && age.is_none_or(|age| age > REFETCH_PAUSE);
        if stale || unknown {
            keys.0 = Some(Instant::now());
            match self.fetch_keys() {
                Ok(fresh) => keys.1 = fresh,
                Err(error) => eprintln!("[broker] Access keys not refreshed: {error}"),
            }
        }
        keys.1.get(id).cloned()
    }

    /// True when `token` is an unexpired Access token for this application, signed by the team.
    pub fn accepts(&self, token: &str) -> bool {
        let Some(key) = decode_header(token).ok().and_then(|header| header.kid).and_then(|id| self.key(&id)) else {
            return false;
        };
        let mut rules = Validation::new(Algorithm::RS256);
        rules.set_audience(&[&self.audience]);
        rules.set_issuer(&[&self.issuer]);
        decode::<Value>(token, &key, &rules).is_ok()
    }
}
