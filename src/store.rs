use crate::deezer::DeezerTrack;
use crate::spotify::SpotifyTrack;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Add(String),
    Skip,
}

pub struct TrackStore {
    decisions: HashMap<String, Decision>,
    path: PathBuf,
}

impl TrackStore {
    pub fn load() -> Self {
        let path = decisions_path();
        let decisions = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| match serde_json::from_str(&content) {
                Ok(map) => Some(map),
                Err(e) => {
                    log::warn!(
                        "Ignoring corrupt track decisions file {}: {}",
                        path.display(),
                        e
                    );
                    None
                }
            })
            .unwrap_or_default();
        TrackStore { decisions, path }
    }

    pub fn get(&self, key: &str) -> Option<&Decision> {
        self.decisions.get(key)
    }

    pub fn record_add(&mut self, key: String, deezer_id: String) {
        self.decisions.insert(key, Decision::Add(deezer_id));
        self.save();
    }

    pub fn record_skip(&mut self, key: String) {
        self.decisions.insert(key, Decision::Skip);
        self.save();
    }

    /// Stored YouTube video id for a Spotify track (sync direction).
    pub fn youtube_video_for(&self, key: &str) -> Option<String> {
        match self.decisions.get(&format!("yt:{key}")) {
            Some(Decision::Add(id)) => Some(id.clone()),
            _ => None,
        }
    }

    /// Remembers the YouTube video chosen for a Spotify track.
    pub fn remember_youtube(&mut self, key: &str, video_id: &str) {
        self.decisions.insert(
            format!("yt:{key}"),
            Decision::Add(video_id.to_string()),
        );
    }

    /// Cached search candidates for a track (video_id, title, channel,
    /// duration_secs) — avoids re-paying the 100-unit search on later runs.
    pub fn youtube_candidates(&self, key: &str) -> Option<Vec<(String, String, String, u64)>> {
        let raw = match self.decisions.get(&format!("ytc:{key}")) {
            Some(Decision::Add(json)) => json,
            _ => return None,
        };
        serde_json::from_str(raw).ok()
    }

    pub fn remember_youtube_candidates(
        &mut self,
        key: &str,
        cands: &[(String, String, String, u64)],
    ) {
        if let Ok(json) = serde_json::to_string(cands) {
            self.decisions
                .insert(format!("ytc:{key}"), Decision::Add(json));
        }
    }

    pub(crate) fn save(&self) {
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            log::warn!("Could not create config dir: {}", e);
            return;
        }
        match serde_json::to_string_pretty(&self.decisions) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, json) {
                    log::warn!("Could not save track decisions: {}", e);
                }
            }
            Err(e) => log::warn!("Could not serialize track decisions: {}", e),
        }
    }
}

fn decisions_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sync-pl")
        .join("track_decisions.json")
}

/// Stable key for a Spotify track: its ID when available, otherwise a
/// normalized title/artist pair.
pub fn key_for_track(track: &SpotifyTrack) -> String {
    match &track.id {
        Some(id) => format!("sp:{}", id),
        None => format!(
            "t:{}|{}",
            crate::matching::normalize(&track.title),
            crate::matching::normalize(&track.artist)
        ),
    }
}

/// Stable key for import decisions (Deezer → Spotify direction). The `dz:`
/// prefix keeps them separate from the sync-direction keys (`sp:`, `t:`).
pub fn key_for_deezer_track(track: &DeezerTrack) -> String {
    format!("dz:{}", track.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_roundtrip() {
        let map: HashMap<String, Decision> = HashMap::from([
            ("a".to_string(), Decision::Add("123".to_string())),
            ("b".to_string(), Decision::Skip),
        ]);
        let json = serde_json::to_string(&map).unwrap();
        let parsed: HashMap<String, Decision> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get("a"), Some(&Decision::Add("123".to_string())));
        assert_eq!(parsed.get("b"), Some(&Decision::Skip));
    }

    #[test]
    fn key_uses_spotify_id_when_present() {
        let track = SpotifyTrack {
            id: Some("abc".to_string()),
            title: "Title".to_string(),
            artist: "Artist".to_string(),
            duration_ms: 1000,
            isrc: None,
            album: None,
        };
        assert_eq!(key_for_track(&track), "sp:abc");
    }

    #[test]
    fn key_falls_back_to_normalized_strings() {
        let track = SpotifyTrack {
            id: None,
            title: "Hedwig's Theme".to_string(),
            artist: "Somebody".to_string(),
            duration_ms: 1000,
            isrc: None,
            album: None,
        };
        let key = key_for_track(&track);
        assert!(key.starts_with("t:"));
        assert!(key.contains("hedwig"));
    }
}
