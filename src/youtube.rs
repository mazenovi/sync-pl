use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{self, Write};
use std::path::PathBuf;

pub const YT_SCOPES: &str = "https://www.googleapis.com/auth/youtube";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API_BASE: &str = "https://www.googleapis.com/youtube/v3";

/// YouTube Data API v3 quota units per day (default Google Cloud ceiling).
pub const DAILY_QUOTA: u64 = 10_000;
/// Cost of one playlistItems.insert or .delete (the expensive operations).
pub const COST_WRITE: u64 = 50;
/// Cost of one search.list call.
pub const COST_SEARCH: u64 = 100;
/// Cost of one cheap read (playlists.list, playlistItems.list).
pub const COST_READ: u64 = 1;

// ---------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct YoutubeTokens {
    access_token: String,
    refresh_token: String,
    /// Unix seconds at which the access token expires.
    expires_at: i64,
}

fn token_cache_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("Could not determine config directory")?;
    Ok(dir.join("sync-pl").join("youtube_token_cache.json"))
}

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct YoutubePlaylist {
    pub id: String,
    pub title: String,
    pub item_count: u64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct YoutubeVideo {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub duration_secs: u64,
    /// Search relevance or empty (URL-pasted).
    pub url: String,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct YoutubeClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    tokens: tokio::sync::Mutex<YoutubeTokens>,
    /// Serializes API calls and keeps track of quota spent this session.
    quota_used: tokio::sync::Mutex<u64>,
    /// Last request time for a light client-side throttle (~5 req/s).
    last_request: tokio::sync::Mutex<Option<std::time::Instant>>,
}

impl YoutubeClient {
    pub async fn new(client_id: &str, client_secret: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("sync-pl/1.0")
            .build()
            .context("Failed to build HTTP client")?;

        let cache_path = token_cache_path()?;

        let tokens = match std::fs::read_to_string(&cache_path) {
            Ok(content) => {
                let tokens: YoutubeTokens = serde_json::from_str(&content)
                    .with_context(|| "Failed to parse YouTube token cache")?;
                Some(tokens)
            }
            Err(_) => None,
        };

        let tokens = match tokens {
            Some(t) => t,
            None => {
                let t = authorize_interactively(&http, client_id, client_secret).await?;
                save_tokens(&cache_path, &t)?;
                println!(
                    "{} YouTube tokens saved to {}",
                    style_ok(),
                    cache_path.display()
                );
                t
            }
        };

        Ok(YoutubeClient {
            http,
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            tokens: tokio::sync::Mutex::new(tokens),
            quota_used: tokio::sync::Mutex::new(0),
            last_request: tokio::sync::Mutex::new(None),
        })
    }

    /// Refresh the access token if it expires within the next minute.
    async fn ensure_fresh(&self) -> Result<()> {
        let mut tokens = self.tokens.lock().await;
        let now = chrono_now();
        if tokens.expires_at > now + 60 {
            return Ok(());
        }
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", tokens.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .context("YouTube token refresh request failed")?;
        let body: serde_json::Value = resp.json().await.context("YouTube token refresh: bad JSON")?;
        let access = body["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("YouTube token refresh failed: {body}"))?
            .to_string();
        tokens.access_token = access;
        tokens.expires_at = now + body["expires_in"].as_i64().unwrap_or(3600);
        save_tokens(&token_cache_path()?, &tokens)?;
        Ok(())
    }

    /// Light throttle so we never hammer the API.
    async fn throttle(&self) {
        const MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_INTERVAL {
                tokio::time::sleep(MIN_INTERVAL - elapsed).await;
            }
        }
        *last = Some(std::time::Instant::now());
    }

    async fn api_call(&self, method: &str, path: &str, cost: u64, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
        self.ensure_fresh().await?;
        // Retry loop for transient failures / 429+503 (YouTube returns 503 on
        // transient backend errors, and 403 quotaExceeded when out of quota).
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.throttle().await;
            let tokens = self.tokens.lock().await;
            let mut req = match method {
                "GET" => self.http.get(format!("{API_BASE}{path}")),
                "POST" => self.http.post(format!("{API_BASE}{path}")),
                "PUT" => self.http.put(format!("{API_BASE}{path}")),
                "DELETE" => self.http.delete(format!("{API_BASE}{path}")),
                _ => bail!("Unsupported HTTP method {method}"),
            };
            req = req.bearer_auth(tokens.access_token.clone());
            if let Some(b) = &body {
                req = req.json(b);
            }
            drop(tokens);

            let resp = req.send().await.context("YouTube API request failed")?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();

            if status.as_u16() == 403 && text.contains("quotaExceeded") {
                bail!("YouTube daily quota exceeded — try again tomorrow (resets at midnight Pacific Time)");
            }
            // Right after playlists.insert, playlistItems.list can 404
            // (playlistNotFound) before the new playlist propagates — retry
            // briefly instead of failing the whole sync.
            if status.as_u16() == 404 && text.contains("playlistNotFound") && attempt < 4 {
                let wait = 2u64 << (attempt - 1);
                log::warn!(
                    "YouTube {path}: playlist not found yet (likely just created) — retrying in {wait}s ({attempt}/3)"
                );
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                continue;
            }
            // Transient: 5xx, 429, and 409 ABORTED/SERVICE_UNAVAILABLE (a
            // known transient YouTube error on writes).
            let transient_409 = status.as_u16() == 409 && text.contains("SERVICE_UNAVAILABLE");
            if status.is_server_error() || status.as_u16() == 429 || transient_409 {
                if attempt >= 4 {
                    bail!("YouTube API kept failing ({}): {text}", status);
                }
                let wait = 1u64 << (attempt - 1);
                log::warn!("YouTube {path} failed ({status}) — retrying in {wait}s ({attempt}/3)");
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                continue;
            }
            if !status.is_success() {
                bail!("YouTube API error ({status}) on {path}: {text}");
            }

            *self.quota_used.lock().await += cost;
            let value = if text.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(&text).with_context(|| "YouTube API returned invalid JSON")?
            };
            return Ok(value);
        }
    }

    /// Quota units consumed by this client instance (session estimate).
    pub async fn quota_used(&self) -> u64 {
        *self.quota_used.lock().await
    }

    // -- playlists ----------------------------------------------------------

    /// Lists the playlists of the authenticated channel ("mine=true").
    pub async fn my_playlists(&self) -> Result<Vec<YoutubePlaylist>> {
        let mut out = Vec::new();
        let mut page_token = String::new();
        loop {
            let mut path = "/playlists?part=snippet,contentDetails&maxResults=50&mine=true".to_string();
            if !page_token.is_empty() {
                path.push_str(&format!("&pageToken={page_token}"));
            }
            let resp = self.api_call("GET", &path, COST_READ, None).await?;
            for item in resp["items"].as_array().cloned().unwrap_or_default() {
                out.push(YoutubePlaylist {
                    id: item["id"].as_str().unwrap_or_default().to_string(),
                    title: item["snippet"]["title"].as_str().unwrap_or_default().to_string(),
                    item_count: item["contentDetails"]["itemCount"].as_u64().unwrap_or(0),
                });
            }
            page_token = resp["nextPageToken"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if page_token.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// Finds a playlist by (case-insensitive, substring) title, like Spotify.
    pub async fn playlist_id(&self, name: &str) -> Result<Option<String>> {
        let lower = name.to_lowercase();
        Ok(self
            .my_playlists()
            .await?
            .into_iter()
            .find(|p| p.title.to_lowercase() == lower || p.title.to_lowercase().contains(&lower))
            .map(|p| p.id))
    }

    /// Creates a playlist. Returns its id.
    pub async fn create_playlist(&self, title: &str, privacy: &str) -> Result<String> {
        let body = json!({
            "snippet": { "title": title },
            "status": { "privacyStatus": privacy }
        });
        let resp = self
            .api_call("POST", "/playlists?part=snippet,status", COST_WRITE, Some(body))
            .await?;
        Ok(resp["id"].as_str().unwrap_or_default().to_string())
    }

    /// Video ids in playlist order.
    pub async fn playlist_video_ids(&self, playlist_id: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut page_token = String::new();
        loop {
            let mut path = format!(
                "/playlistItems?part=contentDetails&maxResults=50&playlistId={playlist_id}"
            );
            if !page_token.is_empty() {
                path.push_str(&format!("&pageToken={page_token}"));
            }
            let resp = self.api_call("GET", &path, COST_READ, None).await?;
            for item in resp["items"].as_array().cloned().unwrap_or_default() {
                if let Some(id) = item["contentDetails"]["videoId"].as_str() {
                    out.push(id.to_string());
                }
            }
            page_token = resp["nextPageToken"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if page_token.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// Appends a video at the end of a playlist.
    pub async fn add_video(&self, playlist_id: &str, video_id: &str) -> Result<()> {
        let body = json!({
            "snippet": {
                "playlistId": playlist_id,
                "resourceId": { "kind": "youtube#video", "videoId": video_id }
            }
        });
        self.api_call("POST", "/playlistItems?part=snippet", COST_WRITE, Some(body))
            .await?;
        Ok(())
    }

    /// Removes an item (by playlistItemId). Kept for future removal support.
    #[allow(dead_code)]
    pub async fn remove_item(&self, playlist_item_id: &str) -> Result<()> {
        self.api_call(
            "DELETE",
            &format!("/playlistItems?id={playlist_item_id}"),
            COST_WRITE,
            None,
        )
        .await?;
        Ok(())
    }

    /// Full playlist items (playlistItemId + videoId) in order. Kept for
    /// future removal support.
    #[allow(dead_code)]
    pub async fn playlist_items(&self, playlist_id: &str) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut page_token = String::new();
        loop {
            let mut path = format!(
                "/playlistItems?part=contentDetails&maxResults=50&playlistId={playlist_id}"
            );
            if !page_token.is_empty() {
                path.push_str(&format!("&pageToken={page_token}"));
            }
            let resp = self.api_call("GET", &path, COST_READ, None).await?;
            for item in resp["items"].as_array().cloned().unwrap_or_default() {
                let pid = item["id"].as_str().unwrap_or_default().to_string();
                let vid = item["contentDetails"]["videoId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                out.push((pid, vid));
            }
            page_token = resp["nextPageToken"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if page_token.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    // -- search -------------------------------------------------------------

    /// Searches videos for a track (artist + title). Returns up to 5 results.
    pub async fn search_video(&self, artist: &str, title: &str) -> Result<Vec<YoutubeVideo>> {
        let query = format!("{artist} {title} official audio");
        let path = format!(
            "/search?part=snippet&type=video&maxResults=5&q={}",
            form_encode(&query)
        );
        let resp = self.api_call("GET", &path, COST_SEARCH, None).await?;
        let mut out = Vec::new();
        for item in resp["items"].as_array().cloned().unwrap_or_default() {
            let video_id = item["id"]["videoId"].as_str().unwrap_or_default().to_string();
            if video_id.is_empty() {
                continue;
            }
            out.push(YoutubeVideo {
                url: format!("https://www.youtube.com/watch?v={video_id}"),
                video_id,
                title: item["snippet"]["title"].as_str().unwrap_or_default().to_string(),
                channel: item["snippet"]["channelTitle"].as_str().unwrap_or_default().to_string(),
                duration_secs: 0, // filled by videos() when needed
            });
        }
        Ok(out)
    }

    /// Batch-fetches durations for up to 50 video ids (1 read unit).
    pub async fn videos_durations(&self, video_ids: &[String]) -> Result<Vec<u64>> {
        if video_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = video_ids.join(",");
        let path = format!("/videos?part=contentDetails&maxResults=50&id={ids}");
        let resp = self.api_call("GET", &path, COST_READ, None).await?;
        let mut out = Vec::new();
        for item in resp["items"].as_array().cloned().unwrap_or_default() {
            let dur = item["contentDetails"]["duration"]
                .as_str()
                .unwrap_or_default();
            out.push(parse_iso8601_duration(dur));
        }
        Ok(out)
    }
}

/// Parses an ISO-8601 duration ("PT3M42S", "PT1H2M3S") into seconds.
pub fn parse_iso8601_duration(dur: &str) -> u64 {
    let mut secs = 0u64;
    let mut num = String::new();
    let mut hours_done = false;
    let mut minutes_done = false;
    for ch in dur.chars() {
        match ch {
            '0'..='9' => num.push(ch),
            'H' => {
                secs += num.parse::<u64>().unwrap_or(0) * 3600;
                num.clear();
                hours_done = true;
            }
            'M' => {
                secs += num.parse::<u64>().unwrap_or(0) * 60;
                num.clear();
                minutes_done = true;
            }
            'S' => {
                secs += num.parse::<u64>().unwrap_or(0);
                num.clear();
            }
            'P' | 'T' | 'D' | '.' | ',' => {
                num.clear(); // days ignored
            }
            _ => {}
        }
        let _ = (hours_done, minutes_done);
    }
    secs
}

/// Minimal percent-encoding for query-string values.
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn chrono_now() -> i64 {    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn save_tokens(path: &PathBuf, tokens: &YoutubeTokens) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(tokens)?)?;
    Ok(())
}

fn style_ok() -> &'static str {
    "OK"
}

/// OAuth device-less desktop flow: print URL, user pastes the redirect URL.
async fn authorize_interactively(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
) -> Result<YoutubeTokens> {
    // We use the loopback-less approach: redirect URI urn:ietf:wg:oauth:2.0:oob
    // is deprecated; instead we use http://localhost:0? No — Google requires a
    // real redirect. Desktop apps should use the "copy/paste" flow via
    // http://localhost redirect; but without a local server, we ask the user to
    // paste the code from the redirected URL. The desktop client created in
    // Google Cloud supports http://localhost and https://… but with an OOB
    // flow removed in 2022, we register no redirect and use the manual-code
    // flow with redirect_uri = "http://localhost" (user sees a connection-
    // refused page; the ?code= is still in the address bar).
    let redirect_uri = "http://localhost:1/";
    let state = "syncpl";
    let url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={}",
        AUTH_URL,
        form_encode(client_id),
        form_encode(redirect_uri),
        form_encode(YT_SCOPES),
        state
    );

    println!();
    println!("1. Open this URL in your browser and approve access:");
    println!("   {url}");
    println!("2. The browser will land on '{redirect_uri}?code=…' — a connection-refused page is normal.");
    println!("3. Copy THAT URL from the address bar and paste it here.");
    loop {
        print!("Paste the redirect URL (or just the code): ");
        io::stdout().flush()?;
        let mut input = String::new();
        let bytes = io::stdin().read_line(&mut input)?;
        if bytes == 0 {
            bail!(
                "No input (stdin closed). Run `sync-pl sync-youtube <playlist>` in a real terminal to complete the Google OAuth consent once — tokens are then cached."
            );
        }
        let input = input.trim().trim_matches('"').trim_matches('\'');
        if input.is_empty() {
            continue;
        }
        let code = if let Some(pos) = input.find("code=") {
            let rest = &input[pos + 5..];
            let end = rest.find('&').unwrap_or(rest.len());
            rest[..end].to_string()
        } else {
            input.to_string()
        };
        if code.is_empty() {
            continue;
        }

        let resp = http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("code", code.as_str()),
                ("code_verifier", ""),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .await
            .context("Token exchange request failed")?;
        let body: serde_json::Value = resp.json().await?;
        if let Some(err) = body["error"].as_str() {
            println!("Error from Google: {err} — try again.");
            continue;
        }
        let access = body["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("No access token in response: {body}"))?
            .to_string();
        let refresh = body["refresh_token"].as_str().map(str::to_string).ok_or_else(|| {
            anyhow!(
                "No refresh token — revoke the app access at https://myaccount.google.com/permissions then retry (prompt=consent is set but Google only issues refresh tokens on first consent)."
            )
        })?;
        return Ok(YoutubeTokens {
            access_token: access,
            refresh_token: refresh,
            expires_at: chrono_now() + body["expires_in"].as_i64().unwrap_or(3600),
        });
    }
}
