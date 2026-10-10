//! Spotify → YouTube playlist mirroring.
//!
//! The YouTube playlist is an append-oriented mirror: missing tracks are
//! appended in Spotify order, nothing is ever removed (v1). Uncertain matches
//! go through an interactive prompt; non-interactive runs record them as
//! skipped in the persistent store so they can be retried later.

use crate::spotify::{SpotifyClient, SpotifyTrack};
use crate::youtube::{YoutubeClient, COST_SEARCH, COST_WRITE, DAILY_QUOTA};
use crate::store;
use anyhow::{bail, Result};
use console::style;

/// A resolved match candidate.
struct Candidate {
    video_id: String,
    title: String,
    channel: String,
    duration_secs: u64,
}

pub struct SyncStats {
    pub playlist: String,
    pub already_there: usize,
    pub added: usize,
    pub skipped: usize,
    pub quota_used: u64,
    pub quota_left: u64,
    pub stopped_early: bool,
}

impl SyncStats {
    fn print(&self, dry_run: bool) {
        println!(
            "\n=== YouTube sync report{} ===",
            if dry_run { " (dry run)" } else { "" }
        );
        println!("  playlist: {}", style(&self.playlist).cyan());
        println!("  already there: {}", self.already_there);
        println!(
            "  {} to add: {}",
            if dry_run { "would add" } else { "added" },
            self.added
        );
        if self.skipped > 0 {
            println!("  {} unmatched (skipped)", style(self.skipped).yellow());
        }
        println!(
            "  quota: ~{} units used this session ({} left of {} today)",
            self.quota_used, self.quota_left, DAILY_QUOTA
        );
        if self.stopped_early {
            println!(
                "  {} stopped early: daily quota would be exceeded — rerun tomorrow, progress is kept.",
                style("NOTE").yellow().bold()
            );
        }
    }
}

pub async fn sync_playlist(
    spotify: &SpotifyClient,
    youtube: &YoutubeClient,
    playlist_name: &str,
    dry_run: bool,
    interactive: bool,
) -> Result<()> {
    // 1. Resolve the Spotify playlist and read its tracks.
    let Some(sp_id) = spotify.playlist_id(playlist_name).await? else {
        bail!("No Spotify playlist matching '{playlist_name}'");
    };
    let details = spotify.playlist_details(&sp_id).await?;
    println!("{} Spotify: {}", style("Source").bold(), details.name);

    let tracks = spotify.playlist_tracks(&sp_id).await?;
    println!("  {} tracks", tracks.len());

    // 2. Find or create the YouTube mirror.
    let yt_name = format!("{} (mirror)", details.name);
    let yt_id = match youtube.playlist_id(&yt_name).await? {
        Some(id) => {
            println!("{} YouTube playlist found: {}", style("Found").green().bold(), yt_name);
            id
        }
        None if dry_run => {
            println!(
                "{} YouTube playlist '{}' would be created",
                style("DRY-RUN").yellow().bold(),
                yt_name
            );
            String::new()
        }
        None => {
            println!("{} Creating YouTube playlist '{}'...", style("Creating").cyan().bold(), yt_name);
            youtube.create_playlist(&yt_name, "private").await?
        }
    };

    // 3. Read existing YouTube content (video ids in order).
    let existing: Vec<String> = if yt_id.is_empty() {
        Vec::new()
    } else {
        youtube.playlist_video_ids(&yt_id).await?
    };
    if !existing.is_empty() {
        println!("  YouTube mirror: {} videos", existing.len());
    }

    let mut store = store::TrackStore::load();
    let mut stats = SyncStats {
        playlist: details.name.clone(),
        already_there: 0,
        added: 0,
        skipped: 0,
        quota_used: youtube.quota_used().await,
        quota_left: DAILY_QUOTA,
        stopped_early: false,
    };

    // 4. For each Spotify track (in order), resolve and append if missing.
    for (index, track) in tracks.iter().enumerate() {
        // Cheap existence check: an exact store entry mapping this track to a
        // video that is already in the mirror.
        if let Some(vid) = store.youtube_video_for(&track_key(track))
            && existing.iter().any(|v| v == &vid) {
                stats.already_there += 1;
                continue;
            }

        // Resolve — search costs 100 units, the decisive expense.
        let quota_after_search = stats.quota_used + crate::youtube::COST_SEARCH;
        if quota_after_search + crate::youtube::COST_WRITE > DAILY_QUOTA {
            stats.stopped_early = true;
            println!(
                "\n{} daily quota would be exceeded at track {}/{} — stopping. Rerun tomorrow.",
                style("STOP").red().bold(),
                index + 1,
                tracks.len()
            );
            break;
        }

        let video = match resolve_track(youtube, track, interactive, dry_run, &mut store).await? {
            Some(v) => v,
            None => {
                stats.skipped += 1;
                continue;
            }
        };

        if dry_run {
            stats.added += 1;
            stats.quota_used += COST_SEARCH + COST_WRITE;
            continue;
        }

        youtube.add_video(&yt_id, &video).await?;
        stats.added += 1;
        stats.quota_used += COST_SEARCH + COST_WRITE;
        store.remember_youtube(&track_key(track), &video);
        store.save();
        if stats.added.is_multiple_of(25) {
            println!(
                "  ... {} added ({}/{} done, ~{} units used)",
                stats.added,
                index + 1,
                tracks.len(),
                stats.quota_used
            );
        }
    }

    store.save();
    stats.quota_left = DAILY_QUOTA.saturating_sub(stats.quota_used);
    stats.print(dry_run);
    Ok(())
}

/// Resolves a Spotify track to a YouTube video id.
/// Returns Ok(None) when the track must be skipped (no confident match and
/// not interactive, or user chose skip).
async fn resolve_track(
    youtube: &YoutubeClient,
    track: &SpotifyTrack,
    interactive: bool,
    dry_run: bool,
    store: &mut store::TrackStore,
) -> Result<Option<String>> {
    let key = track_key(track);

    // Fast path: this track was already resolved in an earlier run.
    if let Some(vid) = store.youtube_video_for(&key) {
        return Ok(Some(vid));
    }

    // Second path: candidates from a previous search are cached — no new
    // search cost (saves 100 units per skipped/uncertain track per run).
    let mut candidates: Vec<Candidate> = match store.youtube_candidates(&key) {
        Some(cached) => cached
            .into_iter()
            .map(|(video_id, title, channel, duration_secs)| Candidate {
                video_id,
                title,
                channel,
                duration_secs,
            })
            .collect(),
        None => {
            let results = youtube.search_video(&track.artist, &track.title).await?;
            if results.is_empty() {
                println!(
                    "  {} no YouTube result for '{} — {}'",
                    style("MISS").yellow(),
                    track.artist,
                    track.title
                );
                // Remember the miss so future runs don't re-pay the search.
                store.remember_youtube_candidates(&key, &[]);
                return Ok(None);
            }
            // Enrich with durations (1 read unit for the batch).
            let ids: Vec<String> = results.iter().map(|v| v.video_id.clone()).collect();
            let durations = youtube.videos_durations(&ids).await.unwrap_or_default();
            let cands: Vec<Candidate> = results
                .iter()
                .zip(durations.into_iter().chain(std::iter::repeat(0)))
                .map(|(v, dur)| Candidate {
                    video_id: v.video_id.clone(),
                    title: v.title.clone(),
                    channel: v.channel.clone(),
                    duration_secs: dur,
                })
                .collect();
            // Cache for all future runs — a skip must not cost 100 units again.
            let serialized: Vec<(String, String, String, u64)> = cands
                .iter()
                .map(|c| {
                    (
                        c.video_id.clone(),
                        c.title.clone(),
                        c.channel.clone(),
                        c.duration_secs,
                    )
                })
                .collect();
            store.remember_youtube_candidates(&key, &serialized);
            cands
        }
    };

    let want_ms = track.duration_ms.max(0) as u64;
    candidates.sort_by_key(|c| {
        // Prefer duration within ±5 s of the Spotify track, then order.
        (c.duration_secs as i64 * 1000 - want_ms as i64).abs()
    });

    let best = &candidates[0];
    let best_diff_ms = (best.duration_secs as i64 * 1000 - want_ms as i64).abs();
    let confident = best.duration_secs > 0 && best_diff_ms <= 5_000;

    if confident || dry_run {
        if !confident {
            println!(
                "  {} uncertain duration for '{} — {}' (auto-picked {} [{}])",
                style("WARN").yellow(),
                track.artist,
                track.title,
                best.title,
                fmt_secs(best.duration_secs)
            );
        }
        return Ok(Some(best.video_id.clone()));
    }

    if !interactive {
        println!(
            "  {} not confident for '{} — {}' (best: {} [{}]) — use --interactive to review",
            style("SKIP").yellow(),
            track.artist,
            track.title,
            truncate(&best.title, 60),
            fmt_secs(best.duration_secs)
        );
        return Ok(None);
    }

    // Interactive review.
    println!(
        "\n{} '{} — {}' ({}s on Spotify)",
        style("REVIEW").cyan().bold(),
        track.artist,
        track.title,
        want_ms / 1000
    );
    for (i, c) in candidates.iter().enumerate() {
        println!(
            "  [{}] {} — {} [{}]",
            i + 1,
            truncate(&c.title, 70),
            c.channel,
            fmt_secs(c.duration_secs)
        );
    }
    println!("  [s] skip   [u] paste a YouTube URL/ID");
    print!("Choice: ");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();
    Ok(match input {
        "s" | "" => None,
        "u" => {
            print!("URL or video ID: ");
            std::io::Write::flush(&mut std::io::stdout())?;
            let mut url = String::new();
            std::io::stdin().read_line(&mut url)?;
            Some(extract_video_id(url.trim()))
        }
        n => n
            .parse::<usize>()
            .ok()
            .and_then(|i| candidates.get(i.checked_sub(1)?))
            .map(|c| c.video_id.clone()),
    })
}

fn track_key(track: &SpotifyTrack) -> String {
    format!("{}|{}", track.artist.to_lowercase(), track.title.to_lowercase())
}

/// Extracts a video id from a YouTube URL or a raw id.
pub fn extract_video_id(input: &str) -> String {
    let s = input.trim();
    for marker in ["v=", "&v=", "youtu.be/", "shorts/"] {
        if let Some(pos) = s.find(marker) {
            let rest = &s[pos + marker.len()..];
            let end = rest.find(['&', '?', '/']).unwrap_or(rest.len());
            return rest[..end].to_string();
        }
    }
    s.to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn fmt_secs(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_from_watch_url() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=42"),
            "dQw4w9WgXcQ"
        );
    }

    #[test]
    fn video_id_from_short_url() {
        assert_eq!(extract_video_id("https://youtu.be/dQw4w9WgXcQ"), "dQw4w9WgXcQ");
    }

    #[test]
    fn video_id_raw() {
        assert_eq!(extract_video_id("dQw4w9WgXcQ"), "dQw4w9WgXcQ");
    }

    #[test]
    fn parses_iso_durations() {
        assert_eq!(crate::youtube::parse_iso8601_duration("PT3M42S"), 222);
        assert_eq!(crate::youtube::parse_iso8601_duration("PT1H2M3S"), 3723);
        assert_eq!(crate::youtube::parse_iso8601_duration("PT48S"), 48);
        assert_eq!(crate::youtube::parse_iso8601_duration("P1DT2H"), 7200); // days ignored
    }
}
