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
    pub fn new(base_url: String, anon_key: String) -> Result<Self, String> {
        // Trim a trailing slash so `{base}/rest/v1/{table}` never doubles up.
        let base_url = base_url.trim_end_matches('/').to_string();
        // Defense-in-depth: a Bearer JWT + apikey must never leave over plaintext http.
        require_secure_base_url(&base_url)?;
        Ok(Self {
            base_url,
            anon_key,
            access_token: None,
            http: reqwest::Client::new(),
        })
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
    pub async fn post_upsert(
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
        // `resolution=merge-duplicates` = PostgREST's "upsert" (ON CONFLICT DO UPDATE).
        // `return=minimal` = 204 No Content: the flush only needs success/failure, and a
        // representation response would force a RLS SELECT-back of every upserted row.
        // Together these are what supabase-js `.upsert()` (without `.select()`) emits.
        headers.insert(
            "Prefer",
            HeaderValue::from_static("resolution=merge-duplicates, return=minimal"),
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

    /// Fetch ONE page of `table` rows with `change_seq > after_seq`, ordered by `change_seq`
    /// ascending, capped at `limit` — the incremental-pull read (SUR-739 / SUR-652). `change_seq` is
    /// the server-assigned visibility watermark (surfc migration 0051 / trigger `t02_change_seq`),
    /// distinct from the client-authored `updated_at` used for last-write-wins; it is stamped when the
    /// server makes a row visible, so the exclusive `gt` keyset delivers a delayed/offline flush the
    /// moment it appears (the SUR-739 primary win) and needs no writer-clock-skew lookback. The caller
    /// ([`super::pull`]) loops, advancing per page until a short page.
    ///
    /// **Caveat (SUR-739 follow-up):** exact skip-safety needs `change_seq` COMMIT-ordered, but 0051's
    /// bare per-table `nextval` is allocated at statement time — a concurrent flush that commits a
    /// lower value after the cursor passed a higher one is skipped until a full re-pull. The durable
    /// fix is server-side (a per-user lock-serialized counter; trigger-only, no change here). See
    /// [`super::pull`] and SUR-743 (the SUR-739 follow-up migration).
    ///
    /// Returns the raw PostgREST row objects (snake_case, `change_seq` included); RLS scopes them to
    /// the token's user, and the owner sees their own tombstones so `deleted:1` rows come back too.
    pub async fn get_page(
        &self,
        table: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let token = self
            .access_token
            .as_deref()
            .ok_or("no access token set — call set_access_token before pull")?;

        let url = format!(
            "{}/rest/v1/{}?change_seq=gt.{}&order=change_seq.asc&limit={}",
            self.base_url, table, after_seq, limit
        );

        let mut headers = HeaderMap::new();
        headers.insert("apikey", HeaderValue::from_str(&self.anon_key)?);
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );

        let resp = self.http.get(&url).headers(headers).send().await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Box::new(PostgrestError {
                status: status.as_u16(),
                body,
            }));
        }
        Ok(resp.json::<Vec<Value>>().await?)
    }
}

/// The PostgREST seam [`push::flush`](super::push::flush) and [`pull`](super::pull) drive.
/// `PostgrestClient` is the production impl (real reqwest POST/GET); a `#[cfg(test)]` stub lets
/// the flush/pull orchestration be unit-tested without a live Supabase — the SUR-724 Gate-2
/// testability concern, extended to pull in SUR-725.
///
/// `async fn` in a trait is fine here: flush and pull both run single-threaded on the engine's
/// current-thread runtime, so the returned futures never need to be `Send`.
#[allow(async_fn_in_trait)]
pub trait PostgrestSink {
    async fn upsert(&self, table: &str, on_conflict: &str, rows: &Value) -> Result<(), String>;

    /// Fetch one page of `table` rows with `change_seq > after_seq`, ordered by `change_seq` asc,
    /// capped at `limit` (keyset incremental pull, SUR-739 / SUR-652).
    async fn fetch_page(
        &self,
        table: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<Value>, String>;
}

impl PostgrestSink for PostgrestClient {
    async fn upsert(&self, table: &str, on_conflict: &str, rows: &Value) -> Result<(), String> {
        self.post_upsert(table, on_conflict, rows)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_page(
        &self,
        table: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<Value>, String> {
        self.get_page(table, after_seq, limit)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Reject a non-https `base_url` unless it targets loopback — a real user's Bearer JWT +
/// apikey must never transit plaintext http. The local Supabase the integration harness
/// drives is `http://localhost`, which stays allowed.
fn require_secure_base_url(base_url: &str) -> Result<(), String> {
    if base_url.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = base_url.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
            return Ok(());
        }
    }
    Err(format!(
        "refusing non-https base_url {base_url:?}: a Bearer JWT + apikey must not transit plaintext http (loopback excepted)"
    ))
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
        let c = PostgrestClient::new("http://localhost:54321/".into(), "anon".into()).unwrap();
        assert_eq!(c.base_url, "http://localhost:54321");
    }

    #[test]
    fn rejects_remote_plaintext_http() {
        // A real remote over http would leak the Bearer JWT + apikey — refuse it.
        assert!(PostgrestClient::new("http://evil.example.com".into(), "anon".into()).is_err());
    }

    #[test]
    fn allows_https_and_loopback_http() {
        assert!(PostgrestClient::new("https://proj.supabase.co".into(), "anon".into()).is_ok());
        assert!(PostgrestClient::new("http://127.0.0.1:54321".into(), "anon".into()).is_ok());
        assert!(PostgrestClient::new("http://localhost:54321".into(), "anon".into()).is_ok());
    }
}
