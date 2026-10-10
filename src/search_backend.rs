//! Shared Spotify search backend: official API first, automatic fallback to
//! the web-player Pathfinder search when the official one is rate-limited
//! (long 429 windows — the client flags itself via `search_blocked`).
//! The web client is optional — without a configured sp_dc cookie the
//! backend simply returns empty results (with a one-time warning).

use crate::spotify::SpotifyClient;
use crate::spotify_web::SpotifyWebClient;
use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

pub struct SearchBackend<'a> {
    official: &'a SpotifyClient,
    web: Option<&'a SpotifyWebClient>,
    /// Whether we already warned about the missing web client (once per run).
    warned_no_web: AtomicBool,
}

impl<'a> SearchBackend<'a> {
    pub fn new(official: &'a SpotifyClient, web: Option<&'a SpotifyWebClient>) -> Self {
        SearchBackend {
            official,
            web,
            warned_no_web: AtomicBool::new(false),
        }
    }

    /// Search with automatic fallback. Returns an empty vec when both
    /// backends are rate-limited (the caller treats it as "no results").
    pub async fn search_track(&self, query: &str) -> Result<Vec<crate::spotify::SpotifyTrack>> {
        if !self.official.search_blocked.load(Ordering::Relaxed) {
            let results = self.official.search_track(query).await?;
            // search_track may have flagged itself blocked (long 429
            // swallowed) — in that case fall through to the web backend.
            if !results.is_empty() || !self.official.search_blocked.load(Ordering::Relaxed) {
                return Ok(results);
            }
            self.warn_switch();
        }
        self.search_via_web(query).await
    }

    fn warn_switch(&self) {
        if self.web.is_some() {
            println!(
                "{} Official Spotify search rate-limited — switching to web search.",
                console::style("Warning:").yellow().bold()
            );
        }
    }

    #[allow(dead_code)]
    pub fn has_web(&self) -> bool {
        self.web.is_some()
    }

    async fn search_via_web(&self, query: &str) -> Result<Vec<crate::spotify::SpotifyTrack>> {
        match &self.web {
            Some(w) => w.search_track(query).await,
            None => {
                if !self.warned_no_web.swap(true, Ordering::Relaxed) {
                    println!(
                        "{} Web search not configured (run `sync-pl config spotify-web`) — searches will fail while the official API is rate-limited.",
                        console::style("Warning:").yellow().bold()
                    );
                }
                Ok(Vec::new())
            }
        }
    }
}
