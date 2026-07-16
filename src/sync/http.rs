//! The PostgREST client (SUR-724 / SUR-659b). Authenticated upsert plus targeted existing-row
//! patch primitives mirroring supabase-js on the wire:
//!
//!   POST {SUPABASE_URL}/rest/v1/{table}?on_conflict={pk}
//!   apikey: <anon>
//!   Authorization: Bearer <jwt>
//!   Content-Type: application/json
//!   Prefer: resolution=merge-duplicates
//!   body: [ {row}, ... ]
//!
//!   PATCH {SUPABASE_URL}/rest/v1/{table}?{pk}=eq.{record_id}
//!   apikey / Authorization / Content-Type as above
//!   Prefer: return=minimal
//!   body: {partial row without the primary key}
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

    /// Patch one existing row selected by `primary_key = record_id`. Unlike an upsert, PostgREST
    /// does not construct an INSERT candidate, so a narrow notes patch may omit NOT-NULL `text`.
    pub async fn patch_existing(
        &self,
        table: &str,
        primary_key: &str,
        record_id: &str,
        row: &Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let token = self
            .access_token
            .as_deref()
            .ok_or("no access token set — call set_access_token before flush")?;

        let mut url = reqwest::Url::parse(&format!("{}/rest/v1/{}", self.base_url, table))?;
        url.query_pairs_mut()
            .append_pair(primary_key, &format!("eq.{record_id}"));

        let mut headers = HeaderMap::new();
        headers.insert("apikey", HeaderValue::from_str(&self.anon_key)?);
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("Prefer", HeaderValue::from_static("return=minimal"));

        let resp = self
            .http
            .patch(url)
            .headers(headers)
            .json(row)
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
    /// **Commit-ordered (SUR-743):** `change_seq` is assigned in COMMIT order per user — surfc
    /// migration 0052 replaced 0051's per-table `nextval` (allocated at statement time) with a
    /// per-user lock-serialized counter — so the exclusive keyset is skip-safe by construction: a
    /// concurrent flush can no longer commit a lower value after the cursor passed a higher one. The
    /// fix was server-side + trigger-only; the client already consumed a commit-ordered watermark
    /// correctly, so no change here.
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

    /// Batch-fetch rows from `table` by its supplied descriptor primary key, via PostgREST's
    /// `in.()` filter — the
    /// missing-book backfill read ([`super::reconcile`], SUR-820). `ids` must be non-empty (an
    /// empty `in.()` filter is invalid PostgREST syntax); callers guard this before calling.
    pub async fn get_by_ids(
        &self,
        table: &str,
        primary_key: &str,
        ids: &[String],
    ) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let token = self
            .access_token
            .as_deref()
            .ok_or("no access token set — call set_access_token before pull")?;

        let url = by_ids_url(&self.base_url, table, primary_key, ids)?;

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

    /// Read one global runtime config row from `app_config` (SUR-492 kill-switch, migration 0038):
    /// `GET /app_config?key=eq.<key>&select=value` — the PostgREST mirror of the PWA's
    /// `fetchAppConfig` (`surfc/src/supabase.js`). GLOBAL (not user-scoped) and client-readable;
    /// returns the parsed `value` (jsonb) of the first matching row, or `None` if the key is absent.
    pub async fn get_app_config(
        &self,
        key: &str,
    ) -> Result<Option<Value>, Box<dyn std::error::Error + Send + Sync>> {
        let token = self
            .access_token
            .as_deref()
            .ok_or("no access token set — call set_access_token before reading app_config")?;

        let url = format!(
            "{}/rest/v1/app_config?key=eq.{}&select=value",
            self.base_url, key
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
        // `?key=eq...` returns an array; take the first row's `value` (maybeSingle equivalent).
        let rows = resp.json::<Vec<Value>>().await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.get("value").cloned()))
    }

    /// ⚠ NON-SUPABASE EGRESS (SUR-828) — the core's first outbound call to a host other than
    /// Supabase. Query the public Open Library Search API for a book cover by `title` (+ optional
    /// `author`): `GET https://openlibrary.org/search.json?title=…&author=…&limit=1&fields=…`.
    /// UNAUTHENTICATED (public endpoint — no apikey/Bearer) with a short per-request timeout so a
    /// slow Open Library never stalls a reconcile pass. Returns the first doc reduced to a
    /// [`CoverSearchHit`], `None` when there is no result (a definitive miss), or `Err` for a
    /// transient transport/HTTP/parse failure (the caller leaves the book unstamped to retry).
    pub async fn openlibrary_search(
        &self,
        title: &str,
        author: Option<&str>,
    ) -> Result<Option<CoverSearchHit>, Box<dyn std::error::Error + Send + Sync>> {
        let mut query: Vec<(&str, &str)> = vec![
            ("title", title),
            ("limit", "1"),
            ("fields", "key,title,author_name,cover_i,isbn"),
        ];
        if let Some(a) = author.map(str::trim).filter(|a| !a.is_empty()) {
            query.push(("author", a));
        }
        let resp = self
            .http
            .get("https://openlibrary.org/search.json")
            .query(&query)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Open Library search HTTP {status}: {body}").into());
        }
        let data = resp.json::<Value>().await?;
        let Some(doc) = data
            .get("docs")
            .and_then(Value::as_array)
            .and_then(|docs| docs.first())
        else {
            return Ok(None);
        };
        let cover_i = doc.get("cover_i").and_then(Value::as_i64);
        let isbn = doc
            .get("isbn")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(Some(CoverSearchHit { cover_i, isbn }))
    }
}

/// The PostgREST `in.()` filter URL for a batch primary-key fetch — split out from
/// [`PostgrestClient::get_by_ids`] so the wire-format shape is unit-testable without a live
/// server (SUR-820: no integration test for this new read path, per the fast-gate-only scope).
fn by_ids_url(
    base_url: &str,
    table: &str,
    primary_key: &str,
    ids: &[String],
) -> Result<String, &'static str> {
    let mut url = reqwest::Url::parse(&format!("{base_url}/rest/v1/{table}"))
        .map_err(|_| "invalid PostgREST base URL")?;
    let values = ids
        .iter()
        .map(|id| {
            let escaped = id.replace('\\', "\\\\").replace('\"', "\\\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(",");
    url.query_pairs_mut()
        .append_pair(primary_key, &format!("in.({values})"));
    Ok(url.into())
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

    /// Patch one existing row by primary key. Default failure keeps non-push test sinks honest if
    /// a new code path unexpectedly attempts a server mutation they do not model.
    async fn patch(
        &self,
        _table: &str,
        _primary_key: &str,
        _record_id: &str,
        _row: &Value,
    ) -> Result<(), String> {
        Err("targeted patch is not supported by this sink".into())
    }

    /// Fetch one page of `table` rows with `change_seq > after_seq`, ordered by `change_seq` asc,
    /// capped at `limit` (keyset incremental pull, SUR-739 / SUR-652).
    async fn fetch_page(
        &self,
        table: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<Value>, String>;

    /// Batch-fetch rows by the supplied descriptor primary key (`<pk>=in.(...)`) — the
    /// post-pull reconciliation's
    /// missing-book backfill ([`super::reconcile`], SUR-820). Defaulted to empty so the existing
    /// sinks that never fetch by id (`MapSink`, `PagingSink`, `VecSink`, `RecordingSink`) don't
    /// need a stub they'd never exercise.
    async fn fetch_by_ids(
        &self,
        _table: &str,
        _primary_key: &str,
        _ids: &[String],
    ) -> Result<Vec<Value>, String> {
        Ok(Vec::new())
    }

    /// Read a global runtime config value from `app_config` (SUR-492 kill-switch, migration 0038):
    /// the parsed `value` (jsonb) for `key`, or `None`. Defaulted to `None` so a non-production
    /// sink (no config table) reads as "unset" — callers FAIL OPEN on `None`/`Err`, mirroring the
    /// PWA's `fetchAppConfig`: a transient read failure must not disable a feature the flag GATES.
    async fn fetch_app_config(&self, _key: &str) -> Result<Option<Value>, String> {
        Ok(None)
    }
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

    async fn patch(
        &self,
        table: &str,
        primary_key: &str,
        record_id: &str,
        row: &Value,
    ) -> Result<(), String> {
        self.patch_existing(table, primary_key, record_id, row)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_by_ids(
        &self,
        table: &str,
        primary_key: &str,
        ids: &[String],
    ) -> Result<Vec<Value>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.get_by_ids(table, primary_key, ids)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_app_config(&self, key: &str) -> Result<Option<Value>, String> {
        self.get_app_config(key).await.map_err(|e| e.to_string())
    }
}

/// One Open Library Search API hit, reduced to the two fields cover resolution needs (SUR-828):
/// the numeric `cover_i` (→ a `/b/id/<cover_i>` cover URL) and a healed `isbn` (→ a
/// `/b/isbn/<isbn>` URL — the SUR-566 self-heal). Either or both may be absent; a doc with neither
/// is a definitive miss.
#[derive(Debug, Clone, Default)]
pub struct CoverSearchHit {
    pub cover_i: Option<i64>,
    pub isbn: Option<String>,
}

/// ⚠ The core's FIRST non-Supabase egress seam (SUR-828) — kept in its OWN trait, deliberately NOT
/// folded into [`PostgrestSink`], so the new outbound boundary is explicit and greppable for the
/// security-review gate. `PostgrestClient` implements it with a plain unauthenticated `reqwest` GET
/// to the public Open Library Search API; test sinks inherit the `Ok(None)` default and make no
/// network call. The egress is additionally gated at the call site ([`super::reconcile`]) by the
/// SUR-492 `openlibrary_egress` kill-switch and paced (≤10 searches per pass).
#[allow(async_fn_in_trait)]
pub trait CoverEgress {
    /// Query Open Library's Search API for a book cover by `title` (+ optional `author`). Returns
    /// the first doc as a [`CoverSearchHit`] (`Ok(Some)`), `Ok(None)` for no result (a definitive
    /// miss the caller stamps so it never re-queries), or `Err` for a transient outage (the caller
    /// leaves the book UNSTAMPED to retry next pass). Defaulted to `Ok(None)` — a sink overrides it
    /// only to actually reach Open Library.
    async fn search_cover(
        &self,
        _title: &str,
        _author: Option<&str>,
    ) -> Result<Option<CoverSearchHit>, String> {
        Ok(None)
    }
}

impl CoverEgress for PostgrestClient {
    async fn search_cover(
        &self,
        title: &str,
        author: Option<&str>,
    ) -> Result<Option<CoverSearchHit>, String> {
        self.openlibrary_search(title, author)
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

    #[test]
    fn by_ids_url_uses_the_requested_primary_key() {
        let url = by_ids_url(
            "https://proj.supabase.co",
            "note_signals",
            "note_id",
            &["n1".into(), "n2".into()],
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query: Vec<_> = parsed.query_pairs().collect();

        assert_eq!(parsed.path(), "/rest/v1/note_signals");
        assert_eq!(query, vec![("note_id".into(), "in.(\"n1\",\"n2\")".into())]);
    }

    #[test]
    fn by_ids_url_quotes_and_encodes_reserved_id_characters() {
        let ids = vec![
            "comma,value".into(),
            "paren(value)".into(),
            "quote\"value".into(),
            "slash\\value".into(),
        ];
        let url = by_ids_url("https://proj.supabase.co", "books", "id", &ids).unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let filter = parsed
            .query_pairs()
            .find_map(|(key, value)| (key == "id").then(|| value.into_owned()))
            .unwrap();

        assert_eq!(
            filter,
            "in.(\"comma,value\",\"paren(value)\",\"quote\\\"value\",\"slash\\\\value\")"
        );
        assert_eq!(
            parsed.query_pairs().count(),
            1,
            "IDs cannot add query terms"
        );
    }
}
