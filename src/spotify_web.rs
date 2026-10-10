//! Spotify web-internal search: uses the browser session cookie (`sp_dc`) to
//! obtain a web-player token (TOTP-guarded endpoint), then queries the
//! private Pathfinder GraphQL API — the same endpoints the web player uses.
//!
//! This bypasses the developer-API search quota. Same trade-off sync-pl
//! already accepts for Deezer's private APIs: undocumented, may change
//! without notice; the official API stays primary via `search_backend`.

use crate::spotify::SpotifyTrack;
use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::Value;
use sha1::Sha1;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const TOKEN_URL: &str = "https://open.spotify.com/api/token";
const SERVER_TIME_URL: &str = "https://open.spotify.com/api/server-time";
const PATHFINDER_URL: &str = "https://api-partner.spotify.com/pathfinder/v1/query";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:132.0) Gecko/20100101 Firefox/132.0";
/// Persisted-query hash for `searchDesktop`. Spotify rotates it occasionally;
/// on PersistedQueryNotFound, re-extract it from the web player bundle.
const SEARCH_DESKTOP_HASH: &str =
    "eff59fa0a3d026b88b56fddbcf4bdfa16a186b8175a5c1a358c072e053c2e5b0";
/// Pinned cipher secrets (bundle extraction fallback). Validated 2026-10-05.
const FALLBACK_SECRETS: &[(&str, &[i64])] = &[
    (
        "61",
        &[44, 55, 47, 42, 70, 40, 34, 114, 76, 74, 50, 111, 120, 97, 75, 76, 94, 102, 43, 69,
        49, 120, 118, 80, 64, 78],
    ),
    (
        "60",
        &[79, 109, 69, 123, 90, 65, 46, 74, 94, 34, 58, 48, 70, 71, 92, 92, 85, 122, 63, 91,
        64, 87, 87],
    ),
    (
        "59",
        &[123, 105, 79, 70, 110, 59, 52, 125, 60, 49, 80, 70, 89, 75, 80, 86, 63, 53, 123, 37,
        117, 49, 52, 93, 77, 62, 47, 86, 48, 104, 68, 72],
    ),
];

pub struct SpotifyWebClient {
    http: Client,
    sp_dc: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl SpotifyWebClient {
    /// Builds the client from a browser `sp_dc` session cookie. The token is
    /// fetched lazily on the first search and cached ~55 minutes.
    pub fn new(sp_dc: &str) -> Self {
        SpotifyWebClient {
            http: Client::builder()
                .user_agent(USER_AGENT)
                .build()
                .expect("reqwest client"),
            sp_dc: sp_dc.to_string(),
            token: Mutex::new(None),
        }
    }

    /// Fetches (or refreshes) the web-player access token.
    async fn access_token(&self) -> Result<String> {
        let mut cached = self.token.lock().await;
        if let Some((tok, fetched_at)) = cached.as_ref()
            && fetched_at.elapsed() < Duration::from_secs(55 * 60)
        {
            return Ok(tok.clone());
        }

        let secrets = self.fetch_secrets().await?;
        let (version, cipher) = secrets
            .iter()
            .max_by_key(|(v, _)| v.parse::<i64>().unwrap_or(0))
            .ok_or_else(|| anyhow::anyhow!("No TOTP secrets available"))?;

        let server_ts = self.server_time().await?;
        let totp = generate_totp(server_ts, cipher);

        let url = format!(
            "{TOKEN_URL}?reason=transport&productType=web-player&totp={totp}&totpServer={totp}&totpVer={version}"
        );
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .header("Referer", "https://open.spotify.com/")
            .header("App-Platform", "WebPlayer")
            .header("Cookie", format!("sp_dc={}", self.sp_dc))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("Spotify web token request failed")?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Invalid JSON from Spotify web token endpoint")?;
        if !status.is_success() {
            let note = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("");
            anyhow::bail!(
                "Spotify web token endpoint returned HTTP {status} {note}\n\
                 If the sp_dc cookie is expired, log in to open.spotify.com in Firefox and re-run \
                 `sync-pl config spotify-web`."
            );
        }
        let token = body
            .get("accessToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("Spotify web token endpoint returned no accessToken")
            })?
            .to_string();
        *cached = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    async fn server_time(&self) -> Result<i64> {
        let data: Value = self
            .http
            .get(SERVER_TIME_URL)
            .header("User-Agent", USER_AGENT)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .context("Spotify server-time request failed")?
            .json()
            .await
            .context("Invalid JSON from Spotify server-time endpoint")?;
        Ok(data.get("serverTime").and_then(Value::as_i64).unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        }))
    }

    /// TOTP cipher secrets: extracted live from the web player bundle, with a
    /// pinned fallback when the bundle format changes.
    async fn fetch_secrets(&self) -> Result<Vec<(String, Vec<i64>)>> {
        let html = self
            .http
            .get("https://open.spotify.com/")
            .header("User-Agent", USER_AGENT)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .context("Spotify homepage request failed")?
            .text()
            .await
            .context("Failed to read Spotify homepage")?;
        if let Some(bundle_url) = find_web_player_bundle(&html)
            && let Ok(js) = self
                .http
                .get(&bundle_url)
                .header("User-Agent", USER_AGENT)
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .with_context(|| format!("Failed to fetch {bundle_url}"))?
                .text()
                .await
        {
            let secrets = extract_secrets(&js);
            if !secrets.is_empty() {
                return Ok(secrets);
            }
        }
        log::warn!("Web player secrets extraction failed — using pinned fallback");
        Ok(FALLBACK_SECRETS
            .iter()
            .map(|(v, s)| (v.to_string(), s.to_vec()))
            .collect())
    }

    /// Search via the web player's Pathfinder API. Returns tracks in response
    /// order (best match first).
    pub async fn search_track(&self, query: &str) -> Result<Vec<SpotifyTrack>> {
        let token = self.access_token().await?;
        let variables = serde_json::json!({
            "searchTerm": query,
            "offset": 0,
            "limit": 10,
            "numberOfTopResults": 5,
            "includeAudiobooks": true,
            "includePreReleases": true,
            "includeAlbumPreReleases": false,
            "includeAuthors": false,
            "includeEpisodeContentRatingsV2": false,
        });
        let variables_str = variables.to_string();
        let extensions_str = format!(
            r#"{{"persistedQuery":{{"version":1,"sha256Hash":"{SEARCH_DESKTOP_HASH}"}}}}"#
        );
        let params = [
            ("operationName", "searchDesktop".to_string()),
            ("variables", variables_str),
            ("extensions", extensions_str),
        ];
        let resp = self
            .http
            .get(PATHFINDER_URL)
            .query(&params)
            .header("Authorization", format!("Bearer {token}"))
            .header("app-platform", "WebPlayer")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .context("Spotify Pathfinder search request failed")?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .context("Invalid JSON from Spotify Pathfinder")?;
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN
        {
            anyhow::bail!(
                "Spotify web search rejected (HTTP {status}).\n\
                 Your sp_dc cookie is likely expired — log in to open.spotify.com in Firefox, then retry."
            );
        }
        if body
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|e| !e.is_empty())
        {
            let msg = body["errors"][0]
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            anyhow::bail!(
                "Spotify Pathfinder error: {msg} (persisted-query hash may need an update)"
            );
        }
        let items = body
            .pointer("/data/searchV2/tracksV2/items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(items.iter().filter_map(parse_pathfinder_track).collect())
    }
}

/// Finds the full (non-CSS) web player JS bundle URL in the homepage HTML.
/// The marker also matches the stylesheet (`web-player.<hash>.css`), so we
/// match complete URLs and require the `.js` extension.
fn find_web_player_bundle(html: &str) -> Option<String> {
    const MARKER: &str = "/build/web-player/web-player.";
    let mut from = 0;
    while let Some(rel) = html[from..].find(MARKER) {
        let idx = from + rel;
        // The URL starts at the beginning of the https:// scheme preceding
        // the marker.
        let scheme_start = html[..idx].rfind("https://")?;
        let rest = &html[idx..];
        let js_rel = rest.find(".js")?;
        let end = idx + js_rel + 3;
        // Reject spans that cross a string boundary (i.e. swallowed markup).
        if html[scheme_start..end].contains(['"', '\'', ' ', '<', '>']) {
            from = idx + MARKER.len();
            continue;
        }
        return Some(html[scheme_start..end].to_string());
    }
    None
}

/// Extracts `{secret: '...', version: N}` cipher entries from the bundle JS.
fn extract_secrets(js: &str) -> Vec<(String, Vec<i64>)> {
    let mut out = Vec::new();
    let mut rest = js;
    while let Some(pos) = rest.find("secret:") {
        let after = &rest[pos + "secret:".len()..];
        let Some((secret_str, after_secret)) = parse_quoted(after) else {
            rest = &rest[pos + 7..];
            continue;
        };
        let after_secret = after_secret.trim_start();
        let Some(after_comma) = after_secret.strip_prefix(',') else {
            rest = &rest[pos + 7..];
            continue;
        };
        let Some(version_part) = after_comma.trim_start().strip_prefix("version:") else {
            rest = &rest[pos + 7..];
            continue;
        };
        let digits_end = version_part
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(version_part.len());
        let Ok(version) = version_part[..digits_end].parse::<i64>() else {
            rest = &rest[pos + 7..];
            continue;
        };
        out.push((version.to_string(), secret_str.chars().map(|c| c as i64).collect()));
        rest = &version_part[digits_end..];
    }
    out
}

/// Parses a single- or double-quoted JS string at the start of `s`, handling
/// backslash escapes. Returns (value, rest-after-closing-quote).
fn parse_quoted(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let quote = s.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let rest = &s[1..];
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                let (_, next) = chars.next()?;
                out.push(next);
            }
            c if c == quote => return Some((out, &rest[i + 1..])),
            c => out.push(c),
        }
    }
    None
}

/// Generates the 6-digit TOTP the token endpoint expects, from a cipher
/// secret (byte list) and the server timestamp in seconds.
///
/// Algorithm (reverse-engineered from the web player, matches the reference
/// Go implementation): XOR each cipher byte with `(i % 33) + 9`, join the
/// resulting numbers as decimal strings, HMAC-SHA1 the 30s counter with the
/// hex-decoded joined string as key, take the 6 low digits.
fn generate_totp(server_ts: i64, cipher: &[i64]) -> String {
    let transformed: Vec<u8> = cipher
        .iter()
        .enumerate()
        .map(|(t, e)| (*e ^ ((t as i64 % 33) + 9)) as u8)
        .collect();
    let joined: String = transformed.iter().map(|n| n.to_string()).collect();
    let key = joined.as_bytes();

    let counter = (server_ts as u64) / 30;
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&counter.to_be_bytes());
    let hash = mac.finalize().into_bytes();

    let offset = (hash[hash.len() - 1] & 0xf) as usize;
    let code = ((hash[offset] & 0x7f) as u32) << 24
        | (hash[offset + 1] as u32) << 16
        | (hash[offset + 2] as u32) << 8
        | (hash[offset + 3] as u32);
    format!("{:06}", code % 1_000_000)
}

/// Parses one `searchDesktop` track item. Response shape (validated
/// 2026-10-05): `items[].item.data`; older shape `itemV2.data` handled too.
fn parse_pathfinder_track(item: &Value) -> Option<SpotifyTrack> {
    // The union wrapper is `item` today, `itemV2` in older responses.
    let data = if item.get("itemV2").is_some() {
        &item["itemV2"]["data"]
    } else {
        &item["item"]["data"]
    };
    if data.get("__typename").and_then(Value::as_str) != Some("Track") {
        return None;
    }
    let id = data.get("id").and_then(Value::as_str)?.to_string();
    if id.len() != 22 {
        return None;
    }
    let title = data.get("name").and_then(Value::as_str)?.to_string();
    let artist = data
        .pointer("/artists/items")
        .or_else(|| data.pointer("/albumOfTrack/artists/items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|a| {
                    a.pointer("/profile/name")
                        .and_then(Value::as_str)
                        .or_else(|| a.get("name").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let duration_ms = data
        .pointer("/duration/totalMilliseconds")
        .and_then(Value::as_i64)
        .or_else(|| {
            // Legacy shape: duration seconds under albumOfTrack.
            data.pointer("/albumOfTrack/duration/total")
                .and_then(Value::as_i64)
                .map(|s| s * 1000)
        })
        .unwrap_or(0);
    Some(SpotifyTrack {
        id: Some(id),
        title,
        artist,
        duration_ms,
        // Pathfinder doesn't return the ISRC; matching falls back to
        // title/artist/duration.
        isrc: None,
        album: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_pathfinder_track() {
        let item = json!({
            "item": {
                "data": {
                    "__typename": "Track",
                    "id": "1bJ5FuncHc6wyDHBvJ0xF6",
                    "uri": "spotify:track:1bJ5FuncHc6wyDHBvJ0xF6",
                    "name": "Sweet Leaf - 2009 Remaster",
                    "artists": { "items": [ { "profile": { "name": "Black Sabbath" } } ] },
                    "duration": { "totalMilliseconds": 302933 }
                }
            }
        });
        let t = parse_pathfinder_track(&item).unwrap();
        assert_eq!(t.id.as_deref(), Some("1bJ5FuncHc6wyDHBvJ0xF6"));
        assert_eq!(t.title, "Sweet Leaf - 2009 Remaster");
        assert_eq!(t.artist, "Black Sabbath");
        assert_eq!(t.duration_ms, 302_933);
    }

    #[test]
    fn parses_legacy_itemv2_shape() {
        let item = json!({
            "itemV2": {
                "data": {
                    "__typename": "Track",
                    "id": "4cOdK2wGLETKBW3PvgPWqT",
                    "name": "X",
                    "albumOfTrack": {
                        "artists": { "items": [ { "profile": { "name": "Y" } } ] },
                        "duration": { "total": 100 }
                    }
                }
            }
        });
        let t = parse_pathfinder_track(&item).unwrap();
        assert_eq!(t.id.as_deref(), Some("4cOdK2wGLETKBW3PvgPWqT"));
        assert_eq!(t.artist, "Y");
        assert_eq!(t.duration_ms, 100_000);
    }

    #[test]
    fn skips_non_track_items() {
        let item = json!({
            "item": {
                "data": { "__typename": "PodcastEpisode", "id": "abc", "name": "Ep 1" }
            }
        });
        assert!(parse_pathfinder_track(&item).is_none());
    }

    #[test]
    fn extracts_secrets_from_bundle_snippet() {
        // Real shapes seen in the bundle (escaped backslashes included).
        let js = r#"x={secret:',7/*F("rLJ2oxaKL^f+E1xvP@N',version:61},y={secret:'OmE{ZA.J^":0FG\\Uz?[@WW',version:60}"#;
        let secrets = extract_secrets(js);
        assert_eq!(secrets.len(), 2);
        assert_eq!(secrets[0].0, "61");
        assert_eq!(secrets[0].1.len(), 26);
        assert_eq!(secrets[1].0, "60");
    }

    #[test]
    fn find_bundle_url_in_homepage() {
        let html = r#"<link href="https://open.spotifycdn.com/cdn/build/web-player/web-player.e146cbe0.css"/><script src="https://open.spotifycdn.com/cdn/build/web-player/web-player.e5ec40ac.js"></script>"#;
        assert_eq!(
            find_web_player_bundle(html).unwrap(),
            "https://open.spotifycdn.com/cdn/build/web-player/web-player.e5ec40ac.js"
        );
    }

    #[test]
    fn totp_is_stable_and_six_digits() {
        // Deterministic: same inputs → same output (regression guard).
        let cipher: Vec<i64> = (0..26).map(|i| 40 + i).collect();
        let a = generate_totp(1_791_222_865, &cipher);
        let b = generate_totp(1_791_222_865, &cipher);
        assert_eq!(a, b);
        assert_eq!(a.len(), 6);
        let c = generate_totp(1_791_222_895, &cipher);
        assert_eq!(c.len(), 6);
    }
}
