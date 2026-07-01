//! Shared integration-test harness for braird-core's sync engine (SUR-724 / SUR-659b), reused
//! across SUR-659b/c/d and the future bindings crate.
//!
//! The engine's PostgREST calls need a REAL GoTrue-issued access token (a hand-forged JWT would
//! fail signature verification server-side), so this harness authenticates a real test user
//! against a running LOCAL Supabase stack and returns the real token. Everything here is env-
//! driven off `npx supabase status`:
//!   - `SUPABASE_URL`               — the local REST/Auth base (e.g. http://127.0.0.1:54321)
//!   - `SUPABASE_ANON_KEY`          — the public anon apikey
//!   - `SUPABASE_SERVICE_ROLE_KEY`  — the service-role key (admin: create the test user)
//!
//! When `SUPABASE_URL` is absent the integration test skips gracefully (see [`env`]); the CI
//! job exports these after `supabase start`.
//!
//! Native-only: this is test scaffolding for the native sync engine (reqwest blocking client),
//! and the engine itself is gated off wasm32. Compiling to an empty crate on wasm32 keeps a
//! workspace-wide `cargo build --target wasm32-unknown-unknown` green (the parity gate builds
//! only the root package, but this makes `--workspace` safe too).
#![cfg(not(target_arch = "wasm32"))]

use serde_json::{json, Value};

/// The resolved local-Supabase connection env. `None` when `SUPABASE_URL` is unset, so the
/// integration test can skip gracefully off-CI (`cargo test` without a running stack).
pub struct SupabaseEnv {
    pub url: String,
    pub anon_key: String,
    pub service_role_key: String,
}

/// Read the local-Supabase env, or `None` if `SUPABASE_URL` is absent (skip signal).
pub fn env() -> Option<SupabaseEnv> {
    let url = std::env::var("SUPABASE_URL").ok()?;
    let anon_key =
        std::env::var("SUPABASE_ANON_KEY").expect("SUPABASE_ANON_KEY (set with SUPABASE_URL)");
    let service_role_key = std::env::var("SUPABASE_SERVICE_ROLE_KEY")
        .expect("SUPABASE_SERVICE_ROLE_KEY (set with SUPABASE_URL)");
    Some(SupabaseEnv {
        url: url.trim_end_matches('/').to_string(),
        anon_key,
        service_role_key,
    })
}

/// A minted test user: the GoTrue-issued access token (the real JWT the engine hands to
/// PostgREST) and the user id (its `sub`, = the RLS-scoped owner of the seeded rows).
pub struct TestUser {
    pub access_token: String,
    pub user_id: String,
    pub email: String,
}

/// Create a fresh confirmed test user via the admin API and sign it in — returns a REAL
/// GoTrue-issued JWT. A unique email per call keeps parallel test runs isolated. Uses reqwest's
/// blocking client (integration tests are sync; no runtime to thread through the harness).
pub fn mint_test_user_jwt(env: &SupabaseEnv) -> TestUser {
    let http = reqwest::blocking::Client::new();
    let email = format!(
        "sur724-{}@example.test",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let password = "test-password-sur724";

    // Admin-create a pre-confirmed user (bypasses email confirmation + captcha).
    let create: Value = http
        .post(format!("{}/auth/v1/admin/users", env.url))
        .header("apikey", &env.service_role_key)
        .header("Authorization", format!("Bearer {}", env.service_role_key))
        .json(&json!({ "email": email, "password": password, "email_confirm": true }))
        .send()
        .expect("admin create user")
        .error_for_status()
        .expect("admin create user status")
        .json()
        .expect("admin create user body");
    let user_id = create["id"].as_str().expect("created user id").to_string();

    // Sign in with password → a real access token. surfc's config.toml enables Turnstile
    // captcha (`[auth.captcha]`), so the sign-in MUST carry a captcha token — the admin-create
    // above bypasses captcha, which is why only this call needs it. The sync-integration CI
    // sets Cloudflare's "always passes" test secret (`1x0000…AA`), against which GoTrue's
    // siteverify accepts the matching dummy token below. `gotrue_meta_security.captcha_token`
    // is the field supabase-js's `options.captchaToken` maps to.
    let token: Value = http
        .post(format!("{}/auth/v1/token?grant_type=password", env.url))
        .header("apikey", &env.anon_key)
        .json(&json!({
            "email": email,
            "password": password,
            "gotrue_meta_security": { "captcha_token": "XXXX.DUMMY.TOKEN.XXXX" }
        }))
        .send()
        .expect("password grant")
        .error_for_status()
        .expect("password grant status")
        .json()
        .expect("password grant body");
    let access_token = token["access_token"]
        .as_str()
        .expect("access_token")
        .to_string();

    TestUser {
        access_token,
        user_id,
        email,
    }
}

/// Read back a table's rows for the authenticated user, as a JSON array — the assertion seam
/// (the integration test greps `notes.text` for `enc:v2` and checks `content_tag`). `query` is a
/// PostgREST query string (e.g. `id=eq.n1`).
pub fn select(env: &SupabaseEnv, access_token: &str, table: &str, query: &str) -> Value {
    reqwest::blocking::Client::new()
        .get(format!("{}/rest/v1/{}?{}", env.url, table, query))
        .header("apikey", &env.anon_key)
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .expect("select send")
        .error_for_status()
        .expect("select status")
        .json()
        .expect("select body")
}
