use anyhow::{Context, Result};
use chrono::Duration;
use rspotify::model::{
    Id, PlayableId, PlayableItem, PlaylistId, SearchResult, SearchType, SimplifiedPlaylist,
    TrackId,
};
use rspotify::prelude::*;
use rspotify::{scopes, AuthCodeSpotify, Config, Credentials, OAuth};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct SpotifyTrack {
    #[allow(dead_code)]
    pub id: Option<String>,
    pub title: String,
    pub artist: String,
    pub duration_ms: i64,
    /// ISRC of the recording when the API provides it — the universal
    /// cross-platform identifier used to confirm matches on Deezer.
    pub isrc: Option<String>,
    /// Album title — a disambiguation signal for matching.
    pub album: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpotifyPlaylist {
    pub id: String,
    pub name: String,
    /// Highest-resolution cover image URL, when the playlist has one.
    pub cover_url: Option<String>,
    /// Playlist description as set on Spotify.
    pub description: String,
}

pub struct SpotifyClient {
    inner: AuthCodeSpotify,
    last_search: tokio::sync::Mutex<Option<std::time::Instant>>,
    /// Global API rate limiter: serializes all API calls, waiting at least
    /// MIN_API_INTERVAL between two requests so Spotify's rate limits are
    /// never hit proactively (429s still handled reactively as a fallback).
    api_gate: tokio::sync::Mutex<std::time::Instant>,
    /// Set when a search hit a 429 with a Retry-After beyond MAX_RETRY_WAIT
    /// (long quota window): the official search is unusable for now.
    pub search_blocked: Arc<AtomicBool>,
    /// In-memory playlist listing cache (name → id), fetched once per run.
    /// `me/playlists` is the heaviest read (paginated) and gets its own long
    /// quota windows — resolving several names in one run should cost a
    /// single listing. Lifetime: the process (a run never sees renames).
    playlist_cache: Mutex<Option<Vec<SpotifyPlaylist>>>,
}

/// Minimum delay between two Spotify API requests (~6.6 req/s, well under
/// Spotify's limits).
const MIN_API_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

/// Cap on the 429 Retry-After we're willing to sleep for. Anything above
/// means a long quota window (e.g. the daily cap of a development-mode app):
/// waiting hours is pointless, fail fast instead.
const MAX_RETRY_WAIT: u64 = 120;

pub const SPOTIFY_SCOPES: &str = "playlist-read-private playlist-read-collaborative user-library-read playlist-modify-private playlist-modify-public";

impl SpotifyClient {
    /// Waits until at least MIN_API_INTERVAL has elapsed since the previous
    /// API call, then records "now" as the new last-call time.
    async fn api_slot(&self) {
        let mut last = self.api_gate.lock().await;
        let elapsed = last.elapsed();
        if elapsed < MIN_API_INTERVAL {
            tokio::time::sleep(MIN_API_INTERVAL - elapsed).await;
        }
        *last = std::time::Instant::now();
    }

    /// Pulls the next item from a paginated stream with 429-aware retries.
    /// rspotify streams can't resume after an error, but each `next()` is an
    /// independent HTTP request — a transient failure surfaces on the page
    /// fetch, so we simply retry the same `next()` call (the failed request
    /// is re-issued by the stream internally via its page offset).
    /// Long 429 windows (Retry-After > cap) abort with an explicit error.
    async fn retry_stream_item<T>(
        &self,
        stream: &mut rspotify::clients::pagination::Paginator<'_, Result<T, rspotify::ClientError>>,
        what: &str,
    ) -> Result<Option<T>>
    where
        T: Send,
    {
        use futures::StreamExt;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match stream.next().await {
                Some(Ok(item)) => return Ok(Some(item)),
                Some(Err(e)) => {
                    let wait = spotify_429_wait(&e);
                    let transient = crate::net::is_transient_http(&e.to_string());
                    if !transient {
                        return Err(anyhow::Error::new(e).context(format!("Failed to fetch {what}")));
                    }
                    let wait = wait.unwrap_or(1u64 << attempt);
                    if attempt >= 3 || wait > MAX_RETRY_WAIT {
                        return Err(anyhow::Error::new(e).context(format!(
                            "Failed to fetch {what} — rate-limited (retry-after {wait}s)"
                        )));
                    }
                    log::warn!(
                        "Spotify {what} fetch failed ({e}) — retrying in {wait}s ({}/3)",
                        attempt + 1
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                }
                None => return Ok(None),
            }
        }
    }

    pub async fn new(client_id: &str, client_secret: &str, redirect_uri: &str) -> Result<Self> {
        let creds = Credentials::new(client_id, client_secret);
        let oauth = OAuth {
            scopes: scopes!(SPOTIFY_SCOPES),
            redirect_uri: redirect_uri.to_string(),
            ..Default::default()
        };

        let cache_path = token_cache_path();
        let config = Config {
            cache_path,
            token_cached: true,
            token_refreshing: true,
            ..Default::default()
        };

        let inner = AuthCodeSpotify::with_config(creds, oauth, config);

        match inner.read_token_cache(true).await {
            Ok(Some(token)) => {
                *inner.get_token().lock().await.unwrap() = Some(token);
            }
            _ => {
                authorize_interactively(&inner, redirect_uri).await?;
            }
        }

        Ok(SpotifyClient {
            inner,
            last_search: tokio::sync::Mutex::new(None),
            api_gate: tokio::sync::Mutex::new(std::time::Instant::now()),
            search_blocked: Arc::new(AtomicBool::new(false)),
            playlist_cache: Mutex::new(None),
        })
    }

    pub async fn search_track(&self, query: &str) -> Result<Vec<SpotifyTrack>> {
        const SEARCH_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        let mut attempt = 0;
        let result = loop {
            attempt += 1;
            {
                let mut last = self.last_search.lock().await;
                if let Some(prev) = *last {
                    let elapsed = prev.elapsed();
                    if elapsed < SEARCH_MIN_INTERVAL {
                        tokio::time::sleep(SEARCH_MIN_INTERVAL - elapsed).await;
                    }
                }
                *last = Some(std::time::Instant::now());
            }
                self.api_slot().await;
                let result = self
                    .inner
                    .search(query, SearchType::Track, None, None, Some(10), None)
                    .await;
                match result {
                Ok(r) => break r,
                Err(e) => {
                    if !crate::net::is_transient_http(&e.to_string()) {
                        return Err(anyhow::Error::new(e).context("Spotify search failed"));
                    }
                    if attempt >= 3 {
                        log::warn!(
                            "Spotify search unavailable for '{query}' after {attempt} attempts — treating as no results"
                        );
                        return Ok(Vec::new());
                    }
                    let wait = spotify_429_wait(&e).unwrap_or(1u64 << attempt);
                    if wait > MAX_RETRY_WAIT {
                        self.search_blocked.store(true, Ordering::Relaxed);
                        log::warn!(
                            "Spotify search rate-limited for '{query}' — retry-after {wait}s exceeds {MAX_RETRY_WAIT}s cap"
                        );
                        return Ok(Vec::new());
                    }
                    log::warn!(
                        "Spotify search failed ({e}) — retrying in {wait}s ({}/3)",
                        attempt + 1
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                }
            }
        };

        let page = match result {
            SearchResult::Tracks(page) => page,
            _ => return Ok(Vec::new()),
        };

        Ok(page
            .items
            .into_iter()
            .filter_map(|t| {
                let id = t.id.as_ref().map(|id| id.id().to_string())?;
                let artist = t
                    .artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(SpotifyTrack {
                    id: Some(id),
                    title: t.name,
                    artist,
                    duration_ms: t.duration.num_milliseconds(),
                    isrc: t.external_ids.get("isrc").cloned(),
                    album: Some(t.album.name),
                })
            })
            .collect())
    }

    pub async fn create_playlist(&self, name: &str) -> Result<String> {
        let user = self
            .inner
            .current_user()
            .await
            .context("Failed to fetch Spotify user")?;
        let playlist = {
            self.api_slot().await;
            self.inner
                .user_playlist_create(
                    user.id,
                    name,
                    Some(false),
                    None,
                    Some("Mirror of a Deezer playlist, created by sync-pl"),
                )
                .await
                .context("Failed to create Spotify playlist")?
        };
        Ok(playlist.id.id().to_string())
    }

    pub async fn playlist_track_ids(&self, playlist_id: &str) -> Result<HashSet<String>> {
        let id = PlaylistId::from_id(playlist_id)
            .map_err(|_| anyhow::anyhow!("Invalid Spotify playlist id"))?;
        let mut out = HashSet::new();
        self.api_slot().await;
        let mut stream = self.inner.playlist_items(id, None, None);
        while let Some(item) = self.retry_stream_item(&mut stream, "playlist items").await? {
            if let Some(PlayableItem::Track(track)) = item.item
                && let Some(id) = track.id
            {
                out.insert(id.id().to_string());
            }
            self.api_slot().await;
        }
        Ok(out)
    }

    pub async fn add_tracks(&self, playlist_id: &str, track_ids: &[String]) -> Result<()> {
        let pid = PlaylistId::from_id(playlist_id)
            .map_err(|_| anyhow::anyhow!("Invalid Spotify playlist id"))?;
        for chunk in track_ids.chunks(100) {
            let ids: Vec<String> = chunk.to_vec();
            let mut attempt = 0;
            loop {
                attempt += 1;
                let items: Vec<PlayableId<'static>> = ids
                    .iter()
                    .filter_map(|id| TrackId::from_id(id).ok().map(|t| t.into_static()))
                    .map(PlayableId::Track)
                    .collect();
                self.api_slot().await;
                let result = self
                    .inner
                    .playlist_add_items(pid.as_ref(), items, None)
                    .await;
                match result {
                    Ok(_) => break,
                    Err(e) => {
                        if attempt >= 3 || !crate::net::is_transient_http(&e.to_string()) {
                            return Err(anyhow::Error::new(e)
                                .context("Failed to add tracks to the Spotify playlist"));
                        }
                        let wait = spotify_429_wait(&e).unwrap_or(1u64 << attempt);
                        if wait > MAX_RETRY_WAIT {
                            return Err(anyhow::Error::new(e).context(format!(
                                "Spotify rate-limited (retry-after {wait}s exceeds {MAX_RETRY_WAIT}s cap)"
                            )));
                        }
                        log::warn!(
                            "Spotify add failed ({e}) — retrying in {wait}s ({}/3)",
                            attempt + 1
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Replaces the full content of a playlist with the given track IDs, in
    /// order (PUT /playlists/{id}/tracks). Chunks by 100 — the first chunk is
    /// sent via replace, the rest are appended.
    pub async fn replace_tracks(&self, playlist_id: &str, track_ids: &[String]) -> Result<()> {
        let pid = PlaylistId::from_id(playlist_id)
            .map_err(|_| anyhow::anyhow!("Invalid Spotify playlist id"))?;
        let mut chunks = track_ids.chunks(100).peekable();
        if chunks.peek().is_none() {
            // Empty target: replace with nothing.
            self.api_slot().await;
            self.inner
                .playlist_replace_items(pid.as_ref(), None)
                .await
                .context("Failed to clear the Spotify playlist")?;
            return Ok(());
        }
        for (i, chunk) in chunks.enumerate() {
            let items: Vec<PlayableId<'static>> = chunk
                .iter()
                .filter_map(|id| TrackId::from_id(id).ok().map(|t| t.into_static()))
                .map(PlayableId::Track)
                .collect();
            let mut attempt = 0;
            loop {
                attempt += 1;
                let sent = if i == 0 {
                    self.api_slot().await;
                    self.inner
                        .playlist_replace_items(pid.as_ref(), items.clone())
                        .await
                        .map(|_| ())
                } else {
                    self.api_slot().await;
                    self.inner
                        .playlist_add_items(pid.as_ref(), items.clone(), None)
                        .await
                        .map(|_| ())
                };
                match sent {
                    Ok(()) => break,
                    Err(e) => {
                        if attempt >= 3 || !crate::net::is_transient_http(&e.to_string()) {
                            return Err(anyhow::Error::new(e).context(
                                "Failed to write tracks to the Spotify playlist",
                            ));
                        }
                        let wait = spotify_429_wait(&e).unwrap_or(1u64 << attempt);
                        if wait > MAX_RETRY_WAIT {
                            return Err(anyhow::Error::new(e).context(format!(
                                "Spotify rate-limited (retry-after {wait}s exceeds {MAX_RETRY_WAIT}s cap)"
                            )));
                        }
                        log::warn!(
                            "Spotify replace/add failed ({e}) — retrying in {wait}s ({}/3)",
                            attempt + 1
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Ordered track IDs of a playlist (position order), local (non-local,
    /// resolvable) tracks only.
    pub async fn playlist_track_ids_ordered(&self, playlist_id: &str) -> Result<Vec<String>> {
        let id = PlaylistId::from_id(playlist_id)
            .map_err(|_| anyhow::anyhow!("Invalid Spotify playlist id"))?;
        let mut out = Vec::new();
        self.api_slot().await;
        let mut stream = self.inner.playlist_items(id, None, None);
        while let Some(item) = self.retry_stream_item(&mut stream, "playlist items").await? {
            if let Some(PlayableItem::Track(track)) = item.item
                && let Some(id) = track.id
            {
                out.push(id.id().to_string());
            }
            self.api_slot().await;
        }
        Ok(out)
    }

    pub async fn track_by_id(&self, id: &str) -> Result<SpotifyTrack> {
        let tid = TrackId::from_id(id).map_err(|_| anyhow::anyhow!("Invalid Spotify track id"))?;
        self.api_slot().await;
        let t = self
            .inner
            .track(tid, None)
            .await
            .context("Failed to fetch Spotify track")?;
        let artist = t
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(SpotifyTrack {
            id: t.id.map(|i| i.id().to_string()),
            title: t.name,
            artist,
            duration_ms: t.duration.num_milliseconds(),
            isrc: t.external_ids.get("isrc").cloned(),
            album: Some(t.album.name),
        })
    }

    /// Lists the user's playlists, cached for the lifetime of the process.
    /// `me/playlists` is the heaviest read; within a single run the listing
    /// cannot go stale (the user isn't renaming playlists mid-command), so
    /// every name resolution after the first is free.
    pub async fn list_playlists(&self) -> Result<Vec<SpotifyPlaylist>> {
        {
            let cached = self.playlist_cache.lock().await;
            if let Some(playlists) = cached.as_ref() {
                return Ok(playlists.clone());
            }
        }
        let mut results = Vec::new();
        self.api_slot().await;
        let mut stream = self.inner.current_user_playlists();
        while let Some(item) = self.retry_stream_item(&mut stream, "playlists").await? {
            let playlist: SimplifiedPlaylist = item;
            results.push(SpotifyPlaylist {
                id: playlist.id.id().to_string(),
                name: playlist.name.clone(),
                cover_url: best_image_url(&playlist.images),
                // The simplified listing doesn't carry the description;
                // filled lazily by playlist_details() when needed.
                description: String::new(),
            });
            self.api_slot().await;
        }
        *self.playlist_cache.lock().await = Some(results.clone());
        Ok(results)
    }

    /// Fetches a single playlist with its full metadata (description, …).
    pub async fn playlist_details(&self, playlist_id: &str) -> Result<SpotifyPlaylist> {
        let id = PlaylistId::from_id(playlist_id)
            .map_err(|_| anyhow::anyhow!("Invalid Spotify playlist id"))?;
        self.api_slot().await;
        let playlist = self
            .inner
            .playlist(id, None, None)
            .await
            .context("Failed to fetch Spotify playlist")?;
        Ok(SpotifyPlaylist {
            id: playlist.id.id().to_string(),
            name: playlist.name,
            cover_url: best_image_url(&playlist.images),
            description: playlist.description.unwrap_or_default(),
        })
    }

    /// Sets a playlist description (PUT /playlists/{id}).
    pub async fn set_playlist_description(&self, playlist_id: &str, description: &str) -> Result<()> {
        let id = PlaylistId::from_id(playlist_id)
            .map_err(|_| anyhow::anyhow!("Invalid Spotify playlist id"))?;
        self.api_slot().await;
        // Signature: (id, name, public, description, collaborative) — the
        // description goes in the 4th slot. Passing it 2nd used to RENAME
        // the playlist to the description (bug that renamed "Why not a
        // beastie revolution" to its own description text).
        self.inner
            .playlist_change_detail(id.as_ref(), None, None, Some(description), None)
            .await
            .context("Failed to update Spotify playlist description")?;
        Ok(())
    }

    /// Cover image URL of a playlist, via the official API. Used as a
    /// fallback when only the playlist id is known.
    pub async fn playlist_cover_url(&self, playlist_id: &str) -> Result<Option<String>> {
        let id = PlaylistId::from_id(playlist_id)
            .map_err(|_| anyhow::anyhow!("Invalid Spotify playlist id"))?;
        self.api_slot().await;
        let playlist = self
            .inner
            .playlist(id, None, None)
            .await
            .context("Failed to fetch Spotify playlist")?;
        Ok(best_image_url(&playlist.images))
    }

    /// Downloads raw image bytes from a cover URL (the Spotify CDN is public,
    /// no OAuth needed).
    pub async fn download_cover(&self, url: &str) -> Result<Vec<u8>> {
        let bytes = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .context("Failed to download Spotify cover image")?
            .error_for_status()
            .context("Spotify cover download returned an HTTP error")?
            .bytes()
            .await
            .context("Failed to read Spotify cover image body")?;
        Ok(bytes.to_vec())
    }

    pub async fn playlist_id(&self, name: &str) -> Result<Option<String>> {
        let playlists = self.list_playlists().await?;
        let name_lower = name.to_lowercase();
        Ok(playlists
            .into_iter()
            .find(|p| p.name.to_lowercase() == name_lower || p.name.to_lowercase().contains(&name_lower))
            .map(|p| p.id))
    }

    /// Probe the API for rate-limiting (429). Returns Ok(true) with the
    /// Retry-After seconds when rate-limited, Ok(false) with the user id when
    /// the call succeeds, Err for other (auth/network) failures.
    pub async fn rate_limit_probe(&self) -> Result<(bool, Option<u64>, Option<String>)> {
        self.api_slot().await;
        match self.inner.current_user().await {
            Ok(user) => Ok((false, None, Some(user.id.id().to_string()))),
            Err(e) => {
                if let Some(wait) = spotify_429_wait(&e) {
                    Ok((true, Some(wait), None))
                } else {
                    Err(anyhow::Error::new(e).context("Spotify API call failed"))
                }
            }
        }
    }

    /// Search probe: search is the most tightly rate-limited endpoint, and
    /// the one imports depend on — a plain current_user probe can pass while
    /// search is blocked for hours (long quota window).
    pub async fn search_rate_limit_probe(&self) -> Result<(bool, Option<u64>)> {
        self.api_slot().await;
        match self.inner.search("the", SearchType::Track, None, None, Some(1), None).await {
            Ok(_) => Ok((false, None)),
            Err(e) => {
                if let Some(wait) = spotify_429_wait(&e) {
                    Ok((true, Some(wait)))
                } else {
                    Err(anyhow::Error::new(e).context("Spotify search probe failed"))
                }
            }
        }
    }

    /// Playlists probe: `me/playlists` is the endpoint imports hammer the
    /// hardest (large playlists paginate 50 items per request, ~150 requests
    /// per full re-read) and gets its own long quota windows.
    pub async fn playlists_rate_limit_probe(&self) -> Result<(bool, Option<u64>)> {
        self.api_slot().await;
        match self.list_playlists().await {
            Ok(_) => Ok((false, None)),
            Err(e) => {
                let msg = e.to_string();
                if let Some(rest) = msg.split("retry-after ").nth(1) {
                    let secs = rest
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect::<String>();
                    if let Ok(s) = secs.parse::<u64>() {
                        return Ok((true, Some(s)));
                    }
                    return Ok((true, None));
                }
                if msg.contains("429") {
                    return Ok((true, None));
                }
                Err(e.context("Spotify playlists probe failed"))
            }
        }
    }

    pub async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<SpotifyTrack>> {
        let id = PlaylistId::from_id(playlist_id)
            .map_err(|_| anyhow::anyhow!("Invalid Spotify playlist id"))?;
        let mut results = Vec::new();

        self.api_slot().await;
        let mut stream = self.inner.playlist_items(id, None, None);
        while let Some(item) = self.retry_stream_item(&mut stream, "playlist items").await? {
            if let Some(PlayableItem::Track(track)) = item.item {
                let artist = track
                    .artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                results.push(SpotifyTrack {
                    id: track.id.as_ref().map(|id| id.id().to_string()),
                    title: track.name,
                    artist,
                    duration_ms: track.duration.num_milliseconds(),
                    isrc: track.external_ids.get("isrc").cloned(),
                    album: Some(track.album.name.clone()),
                });
            }
            self.api_slot().await;
        }
        Ok(results)
    }

    /// Fills in ISRCs for tracks the playlist response didn't include them
    /// for. Best-effort: on error the ISRCs simply stay unset (matching falls
    /// back to fuzzy).
    pub async fn fill_missing_isrcs(&self, tracks: &mut [SpotifyTrack]) {
        let missing: Vec<TrackId<'static>> = tracks
            .iter()
            .filter(|t| t.isrc.is_none())
            .filter_map(|t| t.id.as_ref().and_then(|id| TrackId::from_id(id).ok()))
            .map(|tid| tid.into_static())
            .collect();

        let mut fetched = 0usize;
        for tid in &missing {
            match self.track_by_id(tid.id()).await {
                Ok(t) => {
                    if let Some(src) = &t.id {
                        let id = src.as_str();
                        if let Some(spot) = tracks.iter_mut().find(|s| s.id.as_deref() == Some(id)) {
                            spot.isrc = t.isrc.clone();
                            fetched += 1;
                        }
                    }
                }
                Err(_) => continue,
            }
        }
        if !missing.is_empty() && fetched < missing.len() {
            log::warn!(
                "ISRC lookups incomplete: {}/{} filled — matching may fall back to fuzzy",
                fetched,
                missing.len()
            );
        }
    }
}

/// Highest-resolution cover URL: Spotify returns images widest-first, so the
/// first entry is the best one.
fn best_image_url(images: &[rspotify::model::Image]) -> Option<String> {
    images.first().map(|img| img.url.clone())
}

async fn authorize_interactively(inner: &AuthCodeSpotify, redirect_uri: &str) -> Result<()> {
    let url = inner
        .get_authorize_url(false)
        .context("Failed to build Spotify authorization URL")?;

    println!();
    println!("1. Open this URL in your browser and click Agree:");
    println!("   {}", url);
    println!("2. The browser will then land on '{}?code=...' (a 404 page is normal).", redirect_uri);
    println!("3. Copy THAT new URL from the address bar — not the authorization URL above.");
    loop {
        print!("Paste the redirect URL (or just the code): ");
        io::stdout().flush().context("Failed to write to stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read authorization code")?;
        let input = input.trim().trim_matches('"').trim_matches('\'');

        if input.is_empty() {
            continue;
        }
        if input.contains("accounts.spotify.com/authorize") || input.contains("response_type=code") {
            println!(
                "That is the authorization URL from this terminal — do not paste it back.\nOpen it in the browser, click Agree, then copy the address-bar URL\nstarting with '{}' (it contains ?code=...).",
                redirect_uri
            );
            continue;
        }

        let code = match inner.parse_response_code(input) {
            Some(code) => code,
            None if !input.contains("code=") && !input.is_empty() => input.to_string(),
            None => {
                println!("No 'code=' parameter found — paste the full redirect URL from the browser address bar.");
                continue;
            }
        };

        match inner.request_token(&code).await {
            Ok(()) => {
                inner
                    .write_token_cache()
                    .await
                    .context("Failed to save Spotify token cache")?;
                return Ok(());
            }
            Err(e) => {
                println!("Token exchange failed: {e}. Paste the redirect URL again (Ctrl+C to abort).");
                continue;
            }
        }
    }
}

fn token_cache_path() -> PathBuf {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    config_dir
        .join("sync-pl")
        .join("spotify_token_cache.json")
}

pub fn format_duration(ms: i64) -> String {
    let d = Duration::milliseconds(ms);
    format!("{}:{:02}", d.num_minutes(), d.num_seconds() % 60)
}
/// For 429 responses, the wait time Spotify asked for (Retry-After header,
/// defaulting to 5s).
pub(crate) fn spotify_429_wait(err: &rspotify::ClientError) -> Option<u64> {
    let rspotify::ClientError::Http(http_err) = err else {
        return None;
    };
    let rspotify::http::HttpError::StatusCode(resp) = &**http_err else {
        return None;
    };
    if resp.status().as_u16() != 429 {
        return None;
    }
    let secs = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    Some(secs.unwrap_or(5))
}

/// Extracts a Spotify track ID from a pasted URL (open.spotify.com form,
/// including /intl-xx/ locale and /embed/ prefixes), a `spotify:track:` URI,
/// or a bare base-62 ID.
pub fn extract_spotify_track_id(input: &str) -> Option<String> {
    let s = input.trim();
    fn valid_id(id: &str) -> bool {
        !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric())
    }
    if let Some(pos) = s.find("/track/") {
        let rest = &s[pos + "/track/".len()..];
        let end = rest.find(['?', '&', ' ', '/']).unwrap_or(rest.len());
        let id = &rest[..end];
        if valid_id(id) {
            return Some(id.to_string());
        }
        return None;
    }
    if let Some(pos) = s.find("spotify:track:") {
        let rest = &s[pos + "spotify:track:".len()..];
        let end = rest.find(['?', '&', ' ', ':']).unwrap_or(rest.len());
        let id = &rest[..end];
        if valid_id(id) {
            return Some(id.to_string());
        }
        return None;
    }
    if valid_id(s) {
        return Some(s.to_string());
    }
    None
}
