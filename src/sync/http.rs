//! The PostgREST client (SUR-724 / SUR-659b). One authenticated upsert primitive that
//! mirrors what surfc's `supabase.from(table).upsert(...)` does on the wire:
//!
//!   POST {SUPABASE_URL}/rest/v1/{table}?on_conflict={pk}
//!   apikey: <anon>
//!   Authorization: Bearer <jwt>
//!   Content-Type: application/json
//!   Prefer: resolution=merge-duplicates
//!   body: [ {row}, ... ]
//!
//! `user_id` is stamped onto each row by the caller (from the JWT `sub`), never stored in
//! the outbox — exactly as the PWA injects the auth user id at write.

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

/// A PostgREST error surfaced from a failed upsert. Coarse on purpose: the flush only needs
/// "did this table write succeed", and per-record failures stay queued for the next flush.
#[derive(Debug)]
pub struct PostgrestError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for PostgrestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PostgREST {} — {}", self.status, self.body)
    }
}

impl std::error::Error for PostgrestError {}

/// Reusable PostgREST client — holds the base URL, the anon apikey, and the current access
/// token (swapped by [`super::SyncEngine::set_access_token`]). `reqwest::Client` pools
/// connections, so one instance is reused for the whole flush.
pub struct PostgrestClient {
    base_url: String,
    anon_key: String,
    access_token: Option<String>,
    http: reqwest::Client,
}

impl PostgrestClient {
    pub fn new(base_url: String, anon_key: String) -> Self {
        Self {
            // Trim a trailing slash so `{base}/rest/v1/{table}` never doubles up.
            base_url: base_url.trim_end_matches('/').to_string(),
            anon_key,
            access_token: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn set_access_token(&mut self, jwt: String) {
        self.access_token = Some(jwt);
    }

    pub fn access_token(&self) -> Option<&str> {
        self.access_token.as_deref()
    }

    /// Upsert `rows` into `table`, conflict target `on_conflict` (the PK). `rows` is a JSON
    /// array of already-`user_id`-stamped objects. `merge-duplicates` = the PostgREST spelling
    /// of "upsert" (`ON CONFLICT DO UPDATE`), matching supabase-js `.upsert()`.
    pub async fn upsert(
        &self,
        table: &str,
        on_conflict: &str,
        rows: &Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let token = self
            .access_token
            .as_deref()
            .ok_or("no access token set — call set_access_token before flush")?;

        let url = format!(
            "{}/rest/v1/{}?on_conflict={}",
            self.base_url, table, on_conflict
        );

        let mut headers = HeaderMap::new();
        headers.insert("apikey", HeaderValue::from_str(&self.anon_key)?);
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "Prefer",
            HeaderValue::from_static("resolution=merge-duplicates"),
        );

        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .json(rows)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Box::new(PostgrestError {
                status: status.as_u16(),
                body,
            }));
        }
        Ok(())
    }
}

/// Extract the `sub` (user id) claim from a JWT WITHOUT verifying the signature. The token
/// was minted by GoTrue and PostgREST verifies it server-side; the core only needs the
/// `user_id` to stamp rows. Decodes the middle (payload) segment as base64url-no-pad JSON.
///
/// ponytail: no signature check here — verification is the server's job (PostgREST rejects a
/// forged token). We read one claim we then send back to that same server; forging `sub`
/// gains nothing because RLS is keyed off the token's verified `sub`, not this parsed copy.
pub fn user_id_from_jwt(jwt: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use base64::Engine;
    let payload_b64 = jwt
        .split('.')
        .nth(1)
        .ok_or("malformed JWT: no payload segment")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)?;
    let claims: Value = serde_json::from_slice(&bytes)?;
    claims
        .get("sub")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "JWT has no `sub` claim".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn extracts_sub_from_jwt() {
        // header.payload.sig — only the payload matters; signature is ignored.
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"user-123","role":"authenticated"}"#);
        let jwt = format!("h.{payload}.sig");
        assert_eq!(user_id_from_jwt(&jwt).unwrap(), "user-123");
    }

    #[test]
    fn rejects_jwt_without_sub() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"role":"anon"}"#);
        let jwt = format!("h.{payload}.sig");
        assert!(user_id_from_jwt(&jwt).is_err());
    }

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        let c = PostgrestClient::new("http://localhost:54321/".into(), "anon".into());
        assert_eq!(c.base_url, "http://localhost:54321");
    }
}
