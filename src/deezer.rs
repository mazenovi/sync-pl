use anyhow::{anyhow, bail, Context, Result};
use reqwest::cookie::Jar;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const GW_LIGHT_URL: &str = "https://www.deezer.com/ajax/gw-light.php";
const AUTH_ARL_URL: &str = "https://auth.deezer.com/login/arl?i=p&jo=p&rto=c";
const UPLOAD_PICTURE_URL: &str = "https://upload.deezer.com/v2/playlist/picture";
const GRAPHQL_URL: &str = "https://pipe.deezer.com/api";
const UPDATE_PLAYLIST_MUTATION: &str = "mutation UpdatePlaylist($input: PlaylistUpdateMutationInput!) { updatePlaylist(input: $input) { playlist { id } } }";
const LIBRARY_PLAYLISTS_QUERY: &str = "query LibraryPlaylists($first: Int!, $after: String) { me { id playlists(first: $first, after: $after, sort: {by: LAST_MODIFICATION_DATE, order: DESC}) { edges { node { id title description owner { id } } } pageInfo { hasNextPage endCursor } } } }";
/// The JWT returned by auth.deezer.com lives ~6 minutes; refresh a bit early.
const JWT_TTL: Duration = Duration::from_secs(300);
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct DeezerTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration: i64,
    /// ISRC of the recording when known (gw search + public track API).
    pub isrc: Option<String>,
    /// Album title when known — a disambiguation signal for matching.
    pub album: Option<String>,
}

impl DeezerTrack {
    pub fn url(&self) -> String {
        format!("https://www.deezer.com/track/{}", self.id)
    }
}

/// Extracts a Deezer track ID from a pasted URL (any deezer.com form) or a
/// bare numeric ID.
fn extract_track_id(input: &str) -> Option<String> {
    let s = input.trim();
    if let Some(pos) = s.find("/track/") {
        let rest = &s[pos + "/track/".len()..];
        let end = rest.find(['?', '&', '/', ' ']).unwrap_or(rest.len());
        let id = &rest[..end];
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
            return Some(id.to_string());
        }
        return None;
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return Some(s.to_string());
    }
    None
}

/// Extracts a Deezer playlist ID from a pasted URL (deezer.com, link.deezer.com,
/// www.deezer.com/playlist/..., with or without query params) or a bare
/// numeric ID. `None` when the input isn't a playlist reference.
pub fn extract_playlist_id(input: &str) -> Option<String> {
    let s = input.trim();
    if let Some(pos) = s.find("/playlist/") {
        let rest = &s[pos + "/playlist/".len()..];
        let end = rest.find(['?', '&', '/', ' ']).unwrap_or(rest.len());
        let id = &rest[..end];
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
            return Some(id.to_string());
        }
        return None;
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return Some(s.to_string());
    }
    None
}

/// The gw-light gateway returns numeric fields as either JSON numbers or
/// decimal strings, inconsistently (see the "flexString" pattern).
fn flex_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Decodes HTML entities in text returned by Deezer's private APIs, which
/// HTML-escape descriptions (e.g. `&#x2F;` for `/`), turning URLs stored in
/// playlist descriptions back into readable form.
pub(crate) fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let Some(end) = tail.find(';').filter(|&e| (1..=12).contains(&e)) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded: Option<char> = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{00a0}'),
            _ => {
                if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    entity.strip_prefix('#').and_then(|d| d.parse::<u32>().ok())
                }
                .and_then(char::from_u32)
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parses a `search.music` TRACK item (rich gw shape with ISRC and album).
fn parse_gw_track(t: &Value) -> Option<DeezerTrack> {
    let id = t.get("SNG_ID").and_then(flex_i64)?.to_string();
    let mut title = t
        .get("SNG_TITLE")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(version) = t.get("VERSION").and_then(Value::as_str).filter(|v| !v.is_empty()) {
        title = format!("{} ({})", title, version);
    }
    let artist = t
        .get("ART_NAME")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let duration = t.get("DURATION").and_then(flex_i64).unwrap_or(0);
    let isrc = t.get("ISRC").and_then(Value::as_str).map(str::to_string);
    let album = t
        .get("ALB_TITLE")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(DeezerTrack {
        id,
        title,
        artist,
        duration,
        isrc,
        album,
    })
}

fn scan_track_id(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(pos) = rest.find("/track/") {
        let after = &rest[pos + "/track/".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return Some(digits);
        }
        rest = after;
    }
    None
}

#[derive(Debug, Clone)]
pub struct DeezerPlaylist {
    pub id: String,
    pub title: String,
    pub owned: bool,
    /// Description when the listing provides one (empty otherwise — the
    /// GraphQL/gw library listings don't return it). Kept for diagnostics;
    /// the sync rewrites the description unconditionally.
    #[allow(dead_code)]
    pub description: String,
}

pub struct DeezerClient {
    http: reqwest::Client,
    api_token: Mutex<String>,
    user_id: Mutex<Option<String>>,
    arl: String,
    /// Bearer JWT for auth.deezer.com-protected endpoints (picture upload +
    /// pipe GraphQL), cached with its acquisition time.
    jwt: Mutex<Option<(String, std::time::Instant)>>,
}

#[derive(Deserialize)]
struct GwResponse {
    #[serde(default)]
    results: Value,
    #[serde(default)]
    error: Option<Value>,
}

enum GwError {
    CsrfExpired(String),
    RateLimited(String),
    AuthFailed(String),
    Other(String),
}

fn classify_error(err: &Option<Value>) -> Option<GwError> {
    let e = err.as_ref()?;
    if e.is_null() {
        return None;
    }
    if let Some(arr) = e.as_array() {
        if arr.is_empty() {
            return None;
        }
        return Some(GwError::Other("unknown gateway error".to_string()));
    }
    let Some(map) = e.as_object() else {
        return Some(GwError::Other("unknown gateway error".to_string()));
    };
    if map.is_empty() {
        return None;
    }

    // Numeric-code shape (legacy): {"code": N, "message": "..."}
    if let Some(code) = map.get("code").and_then(Value::as_i64) {
        let msg = map
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return match code {
            4 => Some(GwError::RateLimited(format!("rate limit ({}): {}", code, msg))),
            700 => Some(GwError::CsrfExpired(msg.to_string())),
            _ => Some(GwError::Other(format!("{}: {}", code, msg))),
        };
    }

    // String-keyed shape: {"VALID_TOKEN_REQUIRED": "Invalid CSRF token", ...}
    let (code, v) = map.iter().next().unwrap();
    let msg = v.as_str().unwrap_or("").to_string();
    match code.as_str() {
        "VALID_TOKEN_REQUIRED" | "CSRF_TOKEN_INVALID" => Some(GwError::CsrfExpired(msg)),
        "GATEWAY_ERROR" => match msg.as_str() {
            "invalid api token" => Some(GwError::CsrfExpired(msg)),
            "NEED_USER_AUTH_REQUIRED" | "USER_AUTH_REQUIRED" => Some(GwError::AuthFailed(msg)),
            _ => Some(GwError::Other(format!("GATEWAY_ERROR: {}", msg))),
        },
        "QUOTA_ERROR" => Some(GwError::RateLimited(format!("QUOTA_ERROR: {}", msg))),
        "DATA_ERROR" => Some(GwError::Other(format!("DATA_ERROR: {}", msg))),
        _ => Some(GwError::Other(format!("{}: {}", code, msg))),
    }
}

impl DeezerClient {
    pub async fn new(arl: String) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let url = "https://www.deezer.com"
            .parse::<reqwest::Url>()
            .context("Invalid Deezer URL")?;
        jar.add_cookie_str(&format!("arl={}", arl), &url);

        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .cookie_provider(jar)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            .build()
            .context("Failed to build HTTP client")?;

        let client = DeezerClient {
            http,
            api_token: Mutex::new(String::from("null")),
            user_id: Mutex::new(None),
            arl,
            jwt: Mutex::new(None),
        };

        client.refresh_token().await?;
        Ok(client)
    }

    /// Re-fetches the CSRF token by calling deezer.getUserData with the
    /// literal api_token value "null", as required by the gw-light protocol.
    async fn refresh_token(&self) -> Result<()> {
        {
            let mut tok = self.api_token.lock().unwrap();
            *tok = String::from("null");
        }
        let data = self.call_once("deezer.getUserData", None).await?;

        let token = data
            .get("checkForm")
            .and_then(Value::as_str)
            .map(str::to_string);
        let user_id = data.pointer("/USER/USER_ID").and_then(|v| {
            v.as_i64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_string))
        });

        match token {
            Some(token) => {
                *self.api_token.lock().unwrap() = token;
            }
            None => bail!("Deezer login failed: no API token in response"),
        }
        if let Some(user_id) = user_id {
            if user_id == "0" {
                bail!("Deezer login failed: USER_ID is 0 — the ARL token is invalid or expired");
            }
            *self.user_id.lock().unwrap() = Some(user_id);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn validate_arl(&self) -> Result<bool> {
        Ok(self.user_id.lock().unwrap().is_some())
    }

    pub fn user_id(&self) -> Option<String> {
        self.user_id.lock().unwrap().clone()
    }

    /// Performs a single POST with no retry/refresh.
    async fn call_once(&self, method: &str, request: Option<Value>) -> Result<Value> {
        let api_token = self.api_token.lock().unwrap().clone();
        let query = [
            ("method", method.to_string()),
            ("input", "3".to_string()),
            ("api_version", "1.0".to_string()),
            ("api_token", api_token),
        ];

        let body = request.unwrap_or_else(|| json!({}));
        let resp = self
            .http
            .post(GW_LIGHT_URL)
            .query(&query)
            .json(&body)
            .header("Accept", "application/json, text/javascript, */*; q=0.01")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Origin", "https://www.deezer.com")
            .header("Referer", "https://www.deezer.com/")
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            bail!("Deezer gw-light {} returned HTTP {}", method, status);
        }

        let parsed: GwResponse = serde_json::from_str(&text)
            .with_context(|| format!("Invalid JSON from Deezer for method {}", method))?;

        if let Some(err) = classify_error(&parsed.error) {
            let msg = match err {
                GwError::CsrfExpired(m) => {
                    format!("Deezer CSRF token expired for method {}: {}", method, m)
                }
                GwError::RateLimited(m) => {
                    format!("Deezer rate-limited for method {}: {}", method, m)
                }
                GwError::AuthFailed(m) => {
                    format!("Deezer auth failed for method {}: {}", method, m)
                }
                GwError::Other(m) => format!("Deezer error for method {}: {}", method, m),
            };
            bail!("{}", msg);
        }

        Ok(parsed.results)
    }

    /// Gateway call with a single CSRF refresh-and-retry on token expiry.
    async fn call(&self, method: &str, request: Option<Value>) -> Result<Value> {
        match self.call_once(method, request.clone()).await {
            Ok(v) => Ok(v),
            Err(e) if e.to_string().contains("CSRF token expired") => {
                self.refresh_token().await?;
                self.call_once(method, request).await
            }
            Err(e) => Err(e),
        }
    }

    /// Resolves a manually pasted input (full URL, link.deezer.com short
    /// link, or bare ID) to a Deezer track ID.
    pub async fn resolve_manual_track(&self, input: &str) -> Result<String> {
        let input = input.trim();
        if let Some(id) = extract_track_id(input) {
            return Ok(id);
        }
        if !input.starts_with("http") {
            bail!("Not a Deezer track URL or ID");
        }

        let resp = self
            .http
            .get(input)
            .send()
            .await
            .context("Could not follow that link")?;
        let final_url = resp.url().to_string();
        if let Some(id) = extract_track_id(&final_url) {
            return Ok(id);
        }

        if input.contains("link.deezer.com") && resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            if let Some(id) = scan_track_id(&body) {
                return Ok(id);
            }
        }

        bail!(
            "The link does not resolve to a track page (landed on {})",
            final_url
        )
    }

    /// Fetch a single track by ID from the public API (used to validate
    /// manually pasted URLs).
    pub async fn track_by_id(&self, id: &str) -> Result<DeezerTrack> {
        let url = format!("https://api.deezer.com/track/{}", id);
        let data: Value = self
            .http
            .get(&url)
            .send()
            .await
            .context("Deezer track request failed")?
            .error_for_status()
            .context("Deezer track returned an HTTP error")?
            .json()
            .await
            .context("Invalid JSON from Deezer track")?;

        if let Some(err) = data.get("error").filter(|e| !e.is_null())
            && err.as_object().is_some_and(|m| !m.is_empty())
        {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("track {} not found on Deezer: {}", id, msg);
        }

        let title = data
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Unexpected track response for id {}", id))?;
        let artist = data
            .get("artist")
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let duration = data.get("duration").and_then(Value::as_i64).unwrap_or(0);
        let isrc = data
            .get("isrc")
            .and_then(Value::as_str)
            .map(str::to_string);
        let album = data
            .pointer("/album/title")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(DeezerTrack {
            id: id.to_string(),
            title: title.to_string(),
            artist,
            duration,
            isrc,
            album,
        })
    }

    /// Raw gateway call, exposed for the `debug` subcommand.
    pub async fn gw_call(&self, method: &str, body: Option<Value>) -> Result<Value> {
        self.call(method, body).await
    }

    /// Track search. Uses the authenticated gw `search.music` gateway — the
    /// same endpoint the web player uses — which returns the ISRC, album and
    /// version directly, and returns more results than the public API. Falls
    /// back to the public REST search if the gateway call fails.
    pub async fn search_track(&self, query: &str) -> Result<Vec<DeezerTrack>> {
        match self.search_track_gw(query).await {
            Ok(tracks) => Ok(tracks),
            Err(e) => {
                log::warn!("gw search.music failed ({e}) — falling back to public API search");
                self.search_track_public(query).await
            }
        }
    }

    async fn search_track_gw(&self, query: &str) -> Result<Vec<DeezerTrack>> {
        const NB: i64 = 25;
        let params = json!({
            "query": query,
            "filter": "SNG",
            "output": "TRACK",
            "nb": NB,
            "start": 0,
        });
        let data = self.call("search.music", Some(params)).await?;
        let items = data
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(items.iter().filter_map(parse_gw_track).collect())
    }

    async fn search_track_public(&self, query: &str) -> Result<Vec<DeezerTrack>> {
        let url = reqwest::Url::parse_with_params(
            "https://api.deezer.com/search/track",
            &[("q", query), ("limit", "10")],
        )
        .context("Invalid Deezer search URL")?;

        let mut attempt = 0;
        let data: Value = loop {
            attempt += 1;
            let outcome: Result<Value> = async {
                let resp = self
                    .http
                    .get(url.clone())
                    .send()
                    .await
                    .context("Deezer search request failed")?;
                let resp = resp
                    .error_for_status()
                    .context("Deezer search returned an HTTP error")?;
                resp.json::<Value>()
                    .await
                    .context("Invalid JSON from Deezer search")
            }
            .await;

            match outcome {
                Ok(json) => break json,
                Err(e) => {
                    if attempt >= 3 || !crate::net::is_transient_http(&e.to_string()) {
                        return Err(e);
                    }
                    let wait = 1u64 << attempt;
                    log::warn!(
                        "Deezer search failed ({e}) — retrying in {wait}s ({}/3)",
                        attempt + 1
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                }
            }
        };


        let tracks = data
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        Ok(tracks
            .into_iter()
            .filter_map(|t| {
                let id = t.get("id")?.as_i64()?.to_string();
                let title = t.get("title")?.as_str()?.to_string();
                let artist = t
                    .get("artist")
                    .and_then(|a| a.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let duration = t.get("duration").and_then(Value::as_i64).unwrap_or(0);
                Some(DeezerTrack {
                    id,
                    title,
                    artist,
                    duration,
                    // The public search does not carry the ISRC; the album
                    // object (when present) is a useful extra signal.
                    isrc: None,
                    album: t
                        .get("album")
                        .and_then(|a| a.get("title"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            })
            .collect())
    }

    /// Lists the playlists of the logged-in user (owned + followed). Uses
    /// the pipe GraphQL API — the same source as the web player's library —
    /// because the legacy gw profile page's playlist tab turned out to be
    /// volatile (it can return a "recently modified" subset, hiding mirrors
    /// and causing duplicate creations). Falls back to the gw page when the
    /// GraphQL call fails.
    pub async fn user_playlists(&self) -> Result<Vec<DeezerPlaylist>> {
        match self.user_playlists_graphql().await {
            Ok(v) if !v.is_empty() => Ok(v),
            other => {
                if let Err(e) = &other {
                    log::warn!("GraphQL library listing failed ({e}) — falling back to gw-light");
                }
                self.user_playlists_gw().await
            }
        }
    }

    async fn user_playlists_graphql(&self) -> Result<Vec<DeezerPlaylist>> {
        let jwt = self.auth_jwt().await?;
        let mut out: Vec<DeezerPlaylist> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut after: Option<String> = None;
        let mut me_id = String::new();

        for _ in 0..50 {
            let mut variables = json!({ "first": 100 });
            if let Some(c) = &after {
                variables["after"] = json!(c);
            }
            let resp = self
                .http
                .post(GRAPHQL_URL)
                .bearer_auth(&jwt)
                .json(&json!({
                    "query": LIBRARY_PLAYLISTS_QUERY,
                    "variables": variables,
                }))
                .send()
                .await
                .context("Deezer GraphQL library request failed")?;
            let status = resp.status();
            let data: Value = resp
                .json()
                .await
                .context("Invalid JSON from Deezer GraphQL library")?;
            if !status.is_success() {
                bail!("Deezer GraphQL library returned HTTP {}", status);
            }
            if let Some(errors) = data.get("errors").and_then(Value::as_array)
                .filter(|e| !e.is_empty())
            {
                bail!(
                    "GraphQL library error: {}",
                    errors[0]
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                );
            }
            let me = data
                .pointer("/data/me")
                .ok_or_else(|| anyhow!("Unexpected GraphQL library response"))?;
            if let Some(id) = me.get("id").and_then(Value::as_str) {
                me_id = id.to_string();
            }
            let edges = me
                .pointer("/playlists/edges")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for edge in &edges {
                let node = &edge["node"];
                let Some(id) = node.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(title) = node.get("title").and_then(Value::as_str) else {
                    continue;
                };
                if !seen.insert(id.to_string()) {
                    continue;
                }
                let owner = node.pointer("/owner/id").and_then(Value::as_str).unwrap_or("");
                out.push(DeezerPlaylist {
                    id: id.to_string(),
                    title: title.to_string(),
                    owned: owner == me_id,
                    description: decode_html_entities(
                        node.get("description")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    ),
                });
            }
            let more = me
                .pointer("/playlists/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !more {
                break;
            }
            after = me
                .pointer("/playlists/pageInfo/endCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if after.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Legacy listing via `deezer.pageProfile`, kept as a fallback.
    async fn user_playlists_gw(&self) -> Result<Vec<DeezerPlaylist>> {
        let user_id = self
            .user_id
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("Not logged in on Deezer"))?;

        let mut playlists: Vec<DeezerPlaylist> = Vec::new();
        let mut start = 0;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for _ in 0..20 {
            let params = json!({
                "USER_ID": user_id,
                "tab": "playlists",
                "nb": 50,
                "start": start,
            });
            let data = self.call("deezer.pageProfile", Some(params)).await?;
            if start == 0 {
                let raw = serde_json::to_string(&data).unwrap_or_default();
                let truncated: String = raw.chars().take(2000).collect();
                log::debug!("pageProfile raw response: {}", truncated);
            }

            let page = data
                .pointer("/TAB/playlists/data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            let mut new_items = 0;
            for p in page {
                let id = p
                    .get("PLAYLIST_ID")
                    .or_else(|| p.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        p.get("PLAYLIST_ID")
                            .or_else(|| p.get("id"))
                            .and_then(Value::as_i64)
                            .map(|v| v.to_string())
                    });
                let title = p
                    .get("TITLE")
                    .or_else(|| p.get("title"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let (Some(id), Some(title)) = (id, title) else {
                    continue;
                };
                let parent = p
                    .get("PARENT_USER_ID")
                    .or_else(|| p.get("USER_ID"))
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        _ => String::new(),
                    });
                let owned = parent.as_deref() == Some(user_id.as_str());
                if seen.insert(id.clone()) {
                    let description = decode_html_entities(
                        p.get("DESCRIPTION")
                            .or_else(|| p.get("description"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    );
                    playlists.push(DeezerPlaylist {
                        id,
                        title,
                        owned,
                        description,
                    });
                    new_items += 1;
                }
            }

            if new_items == 0 {
                break;
            }
            start += new_items;
        }

        Ok(playlists)
    }

    #[allow(dead_code)]
    pub async fn create_playlist(&self, title: &str) -> Result<String> {
        self.create_playlist_with_description(title, "").await
    }

    pub async fn create_playlist_with_description(
        &self,
        title: &str,
        description: &str,
    ) -> Result<String> {
        let params = json!({
            "title": title,
            "status": 0,
            "description": description,
            "songs": [],
        });
        let data = self.call("playlist.create", Some(params)).await?;

        fn extract_id(v: &Value) -> Option<String> {
            if let Some(n) = v.as_i64() {
                return Some(n.to_string());
            }
            if let Some(s) = v.as_str() {
                if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                    return Some(s.to_string());
                }
                return None;
            }
            if let Some(map) = v.as_object() {
                for key in ["API_PLAYLIST_ID", "PLAYLIST_ID", "ID", "id"] {
                    if let Some(inner) = map.get(key)
                        && let Some(id) = extract_id(inner)
                    {
                        return Some(id);
                    }
                }
            }
            None
        }

        match extract_id(&data) {
            Some(id) => Ok(id),
            None => bail!(
                "Unexpected playlist.create response: {}",
                serde_json::to_string(&data).unwrap_or_default()
            ),
        }
    }

    /// Fetches the title of a Deezer playlist by ID (public API — works for
    /// any playlist, own or not). Errors when the playlist doesn't exist or
    /// isn't accessible.
    pub async fn playlist_title(&self, playlist_id: &str) -> Result<String> {
        if !playlist_id.chars().all(|c| c.is_ascii_digit()) {
            anyhow::bail!("Invalid Deezer playlist id '{playlist_id}'");
        }
        let url = format!("https://api.deezer.com/playlist/{}", playlist_id);
        let data: Value = self
            .http
            .get(&url)
            .send()
            .await
            .context("Deezer public API request failed")?
            .error_for_status()
            .context("Deezer public API returned an HTTP error")?
            .json()
            .await
            .context("Invalid JSON from Deezer public API")?;
        if data.get("error").and_then(|e| e.as_object()).is_some_and(|e| !e.is_empty()) {
            anyhow::bail!(
                "Deezer playlist {} not found or inaccessible: {}",
                playlist_id,
                data.get("error").map(|e| e.to_string()).unwrap_or_default()
            );
        }
        data.get("title")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .with_context(|| format!("Deezer playlist {playlist_id} returned no title"))
    }

    pub async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<DeezerTrack>> {
        let mut tracks: Vec<DeezerTrack> = Vec::new();
        let mut start = 0;

        loop {
            let params = json!({
                "playlist_id": playlist_id,
                "start": start,
                "nb": 500,
                "tab": "songs",
            });
            let data = self.call("playlist.getSongs", Some(params)).await?;
            if start == 0 {
                let raw = serde_json::to_string(&data).unwrap_or_default();
                let truncated: String = raw.chars().take(2000).collect();
                log::debug!("playlist.getSongs raw response: {}", truncated);
            }

            let page = data
                .pointer("/data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let total = data.get("total").and_then(flex_i64);

            for t in &page {
                let Some(id) = t.get("SNG_ID").and_then(flex_i64).map(|v| v.to_string()) else {
                    continue;
                };
                let title = t
                    .get("SNG_TITLE")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let artist = t
                    .get("ART_NAME")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let duration = t.get("DURATION").and_then(flex_i64).unwrap_or(0);
                tracks.push(DeezerTrack {
                    id,
                    title,
                    artist,
                    duration,
                    isrc: None,
                    album: None,
                });
            }

            start += page.len();
            if page.is_empty() {
                break;
            }
            if let Some(total) = total
                && start as i64 >= total
            {
                break;
            }
        }

        Ok(tracks)
    }

    pub async fn add_tracks(&self, playlist_id: &str, track_ids: &[String]) -> Result<()> {
        let songs: Vec<Value> = track_ids
            .iter()
            .map(|id| json!([id, 0]))
            .collect();

        let params = json!({
            "PLAYLIST_ID": playlist_id,
            "songs": songs,
            "offset": -1,
        });
        self.call("playlist.addSongs", Some(params)).await?;
        Ok(())
    }

    /// Returns a cached Bearer JWT obtained by exchanging the ARL token at
    /// auth.deezer.com, refreshing it when older than JWT_TTL. This token
    /// authenticates the private picture-upload and pipe-GraphQL endpoints.
    async fn auth_jwt(&self) -> Result<String> {
        {
            let cached = self.jwt.lock().unwrap();
            if let Some((jwt, at)) = cached.as_ref()
                && at.elapsed() < JWT_TTL
            {
                return Ok(jwt.clone());
            }
        }
        let resp = self
            .http
            .post(AUTH_ARL_URL)
            .header("Accept", "application/json")
            .json(&json!({ "arl": self.arl }))
            .send()
            .await
            .context("Deezer auth request failed")?;
        let status = resp.status();
        let data: Value = resp
            .json()
            .await
            .context("Invalid JSON from Deezer auth service")?;
        if !status.is_success() {
            bail!(
                "Deezer auth failed (HTTP {}): {} — the ARL token may have expired",
                status,
                data
            );
        }
        let jwt = data
            .get("jwt")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Deezer auth response missing jwt"))?
            .to_string();
        *self.jwt.lock().unwrap() = Some((jwt.clone(), std::time::Instant::now()));
        Ok(jwt)
    }

    /// Sets the playlist cover from raw JPEG/PNG bytes, mirroring what the
    /// Deezer web player does: upload via `upload.deezer.com/v2` (Bearer JWT)
    /// then attach with the `updatePlaylist` GraphQL mutation (pipe.deezer.com,
    /// same JWT). This is an undocumented flow and may break without notice.
    pub async fn set_playlist_cover(&self, playlist_id: &str, image: &[u8]) -> Result<()> {
        if image.is_empty() {
            bail!("Empty cover image");
        }
        let mime = match image {
            [0xFF, 0xD8, ..] => "image/jpeg",
            [0x89, b'P', b'N', b'G', ..] => "image/png",
            _ => bail!("Cover must be a JPEG or PNG image"),
        };

        let jwt = self.auth_jwt().await?;
        let part = reqwest::multipart::Part::bytes(image.to_vec())
            .file_name("cover.jpg")
            .mime_str(mime)?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let resp = self
            .http
            .post(UPLOAD_PICTURE_URL)
            .bearer_auth(&jwt)
            .multipart(form)
            .send()
            .await
            .context("Deezer picture upload failed")?;
        let status = resp.status();
        let data: Value = resp
            .json()
            .await
            .context("Invalid JSON from Deezer picture upload")?;
        if !status.is_success() {
            bail!("Deezer picture upload returned HTTP {}: {}", status, data);
        }
        let picture = data
            .get("results")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "Deezer picture upload rejected the image: {}",
                    data.get("error").map(|e| e.to_string()).unwrap_or_default()
                )
            })?;

        let resp = self
            .http
            .post(GRAPHQL_URL)
            .bearer_auth(&jwt)
            .json(&json!({
                "query": UPDATE_PLAYLIST_MUTATION,
                "variables": {
                    "input": {
                        "playlistId": playlist_id,
                        "picture": picture,
                    }
                }
            }))
            .send()
            .await
            .context("Deezer GraphQL request failed")?;
        let data: Value = resp
            .json()
            .await
            .context("Invalid JSON from Deezer GraphQL endpoint")?;

        if let Some(errors) = data.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty())
            && let Some(first) = errors.first()
        {
            let msg = first
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown GraphQL error");
            bail!("Deezer cover update failed: {}", msg);
        }
        Ok(())
    }

    /// Updates a Deezer playlist description via the private GraphQL API
    /// (same channel as the cover update).
    pub async fn update_playlist_description(
        &self,
        playlist_id: &str,
        description: &str,
    ) -> Result<()> {
        let jwt = self
            .auth_jwt()
            .await
            .context("Deezer JWT auth failed (description update)")?;

        let resp = self
            .http
            .post(GRAPHQL_URL)
            .bearer_auth(&jwt)
            .json(&json!({
                "query": UPDATE_PLAYLIST_MUTATION,
                "variables": {
                    "input": {
                        "playlistId": playlist_id,
                        "description": description,
                    }
                }
            }))
            .send()
            .await
            .context("Deezer GraphQL request failed")?;
        let data: Value = resp
            .json()
            .await
            .context("Invalid JSON from Deezer GraphQL endpoint")?;

        if let Some(errors) = data.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty())
            && let Some(first) = errors.first()
        {
            let msg = first
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown GraphQL error");
            bail!("Deezer description update failed: {}", msg);
        }
        if data.pointer("/data/updatePlaylist/playlist/id").is_none() {
            bail!(
                "Unexpected Deezer GraphQL response: {}",
                serde_json::to_string(&data).unwrap_or_default()
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_deezer_playlist_ids() {
        assert_eq!(
            extract_playlist_id("https://www.deezer.com/playlist/7510693564"),
            Some("7510693564".to_string())
        );
        assert_eq!(
            extract_playlist_id("https://www.deezer.com/fr/playlist/7510693564?utm_source=x"),
            Some("7510693564".to_string())
        );
        assert_eq!(
            extract_playlist_id("https://link.deezer.com/s/xyz"),
            None,
            "short links need resolution — not supported, must not parse as an ID"
        );
        assert_eq!(
            extract_playlist_id("7510693564"),
            Some("7510693564".to_string())
        );
        assert_eq!(extract_playlist_id("2020 les plus grand moment du country"), None);
        assert_eq!(extract_playlist_id(""), None);
    }

    #[test]
    fn decodes_html_entities_in_descriptions() {
        assert_eq!(
            decode_html_entities("source: https:&#x2F;&#x2F;fr.wikipedia.org&#x2F;wiki&#x2F;Beastie_Boys"),
            "source: https://fr.wikipedia.org/wiki/Beastie_Boys"
        );
        assert_eq!(
            decode_html_entities("a &amp; b &#39;quoted&#39; &quot;x&quot; &lt;tag&gt;"),
            "a & b 'quoted' \"x\" <tag>"
        );
        assert_eq!(
            decode_html_entities("numérique &#233; and &#xE9;"),
            "numérique é and é"
        );
        // Non-entities pass through untouched.
        assert_eq!(decode_html_entities("Tom & Jerry 5 < 6"), "Tom & Jerry 5 < 6");
        assert_eq!(decode_html_entities("100% &amp done"), "100% &amp done");
    }

    #[test]
    fn extracts_id_from_full_url() {
        assert_eq!(
            extract_track_id("https://www.deezer.com/track/3135553"),
            Some("3135553".to_string())
        );
    }

    #[test]
    fn extracts_id_from_url_with_query() {
        assert_eq!(
            extract_track_id("https://www.deezer.com/track/3135553?utm_source=x"),
            Some("3135553".to_string())
        );
    }

    #[test]
    fn extracts_bare_numeric_id() {
        assert_eq!(extract_track_id(" 3135553 "), Some("3135553".to_string()));
    }

    #[test]
    fn rejects_non_track_inputs() {
        assert_eq!(extract_track_id("https://www.deezer.com/playlist/123"), None);
        assert_eq!(extract_track_id("not a url"), None);
        assert_eq!(extract_track_id(""), None);
        assert_eq!(extract_track_id("https://www.deezer.com/track/"), None);
    }

    #[test]
    fn scans_track_id_from_html() {
        let html = r#"<html><meta http-equiv="refresh" content="0; url=https://www.deezer.com/track/3135553"><body></body></html>"#;
        assert_eq!(scan_track_id(html), Some("3135553".to_string()));
        assert_eq!(scan_track_id("<html>no track here</html>"), None);
    }

    #[test]
    fn flex_numbers_accept_strings_and_numbers() {
        assert_eq!(flex_i64(&serde_json::json!(12345)), Some(12345));
        assert_eq!(
            flex_i64(&serde_json::json!("12345")),
            Some(12345)
        );
        assert_eq!(flex_i64(&serde_json::json!("abc")), None);
        assert_eq!(flex_i64(&serde_json::json!(null)), None);
    }

    #[test]
    fn parses_gw_search_item() {
        let item = json!({
            "SNG_ID": "3135553",
            "SNG_TITLE": "One More Time",
            "VERSION": "Radio Edit",
            "ART_NAME": "Daft Punk",
            "DURATION": "320",
            "ISRC": "GBDUW0000053",
            "ALB_TITLE": "Discovery",
        });
        let t = parse_gw_track(&item).unwrap();
        assert_eq!(t.id, "3135553");
        assert_eq!(t.title, "One More Time (Radio Edit)");
        assert_eq!(t.artist, "Daft Punk");
        assert_eq!(t.duration, 320);
        assert_eq!(t.isrc.as_deref(), Some("GBDUW0000053"));
        assert_eq!(t.album.as_deref(), Some("Discovery"));
    }

    #[test]
    fn parse_gw_track_without_version_keeps_plain_title() {
        let item = json!({
            "SNG_ID": 3135553,
            "SNG_TITLE": "One More Time",
            "ART_NAME": "Daft Punk",
            "DURATION": 320,
            "ISRC": null,
        });
        let t = parse_gw_track(&item).unwrap();
        assert_eq!(t.title, "One More Time");
        assert_eq!(t.isrc, None);
        assert_eq!(t.album, None);
    }

    /// Live round-trip against the real account: create a throwaway playlist,
    /// apply a local image as its cover, verify the CDN picture id changed,
    /// delete it. Skipped unless SYNCPL_TEST_ARL (and SYNCPL_TEST_IMAGE, a
    /// JPEG on disk) are set.
    #[tokio::test]
    #[ignore = "hits the real Deezer API; run with --ignored and SYNCPL_TEST_ARL"]
    async fn live_sets_playlist_cover() {
        let Some(arl) = std::env::var("SYNCPL_TEST_ARL").ok().filter(|s| !s.is_empty()) else {
            return;
        };
        let image: Vec<u8> = match std::env::var("SYNCPL_TEST_IMAGE") {
            Ok(path) => std::fs::read(path).expect("failed to read test image"),
            Err(_) => return,
        };

        let client = DeezerClient::new(arl).await.unwrap();
        let id = client.create_playlist("sync-pl cover test").await.unwrap();

        let before = playlist_picture_medium(&id).await;
        let result = client.set_playlist_cover(&id, &image).await;
        let mut after = playlist_picture_medium(&id).await;
        for _ in 0..3 {
            if after != before {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            after = playlist_picture_medium(&id).await;
        }
        client
            .gw_call("playlist.delete", Some(json!({ "PLAYLIST_ID": id })))
            .await
            .ok();

        result.unwrap();
        assert_ne!(
            before, after,
            "playlist picture_medium should change after set_playlist_cover"
        );
    }

    async fn playlist_picture_medium(id: &str) -> Option<String> {
        let url = format!("https://api.deezer.com/playlist/{}", id);
        let v: Value = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        v.get("picture_medium")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Live check that user_playlists() returns the full library without
    /// duplicates. Skipped unless SYNCPL_TEST_ARL is set.
    #[tokio::test]
    #[ignore = "hits the real Deezer API; run with --ignored and SYNCPL_TEST_ARL"]
    async fn live_lists_all_user_playlists() {
        let Some(arl) = std::env::var("SYNCPL_TEST_ARL").ok().filter(|s| !s.is_empty()) else {
            return;
        };
        let client = DeezerClient::new(arl).await.unwrap();
        let playlists = client.user_playlists().await.unwrap();
        println!("user_playlists count: {}", playlists.len());
        let mut ids: Vec<&str> = playlists.iter().map(|p| p.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), playlists.len(), "no duplicate ids in listing");
        assert!(
            playlists.len() >= 15,
            "listing should see the whole library ({} seen)",
            playlists.len()
        );
    }
}
