use crate::deezer::{decode_html_entities, DeezerClient, DeezerPlaylist, DeezerTrack};
use crate::matching::{self, TrackMatch};
use crate::prompt::{prompt_candidates, InteractiveChoice, PromptAction};
use crate::spotify::{self, SpotifyClient, SpotifyTrack};
use crate::store::{self, Decision, TrackStore};
use anyhow::Result;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;

const ADD_BATCH_SIZE: usize = 50;
const AUTO_ADD_THRESHOLD: f64 = matching::AUTO_THRESHOLD;

pub struct SyncStats {
    pub playlist_name: String,
    pub dry_run: bool,
    pub total: usize,
    pub added: usize,
    pub already: usize,
    pub not_found: Vec<(SpotifyTrack, Option<TrackMatch>)>,
    pub low_confidence: Vec<(SpotifyTrack, Option<TrackMatch>)>,
    pub skipped_marked: Vec<SpotifyTrack>,
}

#[allow(clippy::too_many_arguments)]
pub async fn sync_playlist(
    spotify: &SpotifyClient,
    deezer: &DeezerClient,
    store: &mut TrackStore,
    playlist_name: &str,
    dry_run: bool,
    interactive: bool,
    set_cover: bool,
    cover_url: Option<&str>,
) -> Result<SyncStats> {
    let playlist_id = spotify
        .playlist_id(playlist_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Playlist '{}' not found on Spotify", playlist_name))?;

    let details = spotify.playlist_details(&playlist_id).await?;
    let mut description = details.description.trim().to_string();

    // Heal a Spotify description that itself contains HTML-escaped entities
    // (usually from a copy/paste of Deezer's escaped output): decode it and
    // write the clean version back to Spotify so both platforms agree.
    if description.contains('&')
        && let decoded = decode_html_entities(&description)
        && decoded != description
    {
        match spotify.set_playlist_description(&playlist_id, &decoded).await {
            Ok(()) => {
                println!(
                    "{} Spotify description decoded (was HTML-escaped)",
                    style("OK").green().bold()
                );
                description = decoded;
            }
            Err(e) => {
                println!(
                    "{} Could not decode the Spotify description: {}",
                    style("Warning:").yellow().bold(),
                    e
                );
            }
        }
    }

    let mut tracks = spotify.playlist_tracks(&playlist_id).await?;
    spotify.fill_missing_isrcs(&mut tracks).await;
    let total = tracks.len();
    println!("{} {} tracks", style("Spotify playlist:").bold(), total);

    let mirror_name = playlist_name.to_string();
    let existing = deezer.user_playlists().await?;
    let mirror = find_mirror(&existing, &mirror_name);

    let deezer_playlist_id: Option<String> = match mirror {
        Some(p) => {
            println!(
                "{} Deezer mirror found: {}",
                style("Found").green().bold(),
                mirror_name
            );
            Some(p.id.clone())
        }
        None if dry_run => {
            println!(
                "{} Deezer mirror '{}' would be created",
                style("DRY-RUN").yellow().bold(),
                mirror_name
            );
            None
        }
        None => {
            println!(
                "{} Creating Deezer playlist '{}'...",
                style("Creating").cyan().bold(),
                mirror_name
            );
            log::debug!(
                "Creating Deezer mirror; raw Spotify name (Debug-escaped): {:?}",
                mirror_name
            );
            let id = deezer
                .create_playlist_with_description(&mirror_name, &description)
                .await?;
            println!("{} Created to Deezer playlist '{}'", style("OK").green().bold(), mirror_name);
            Some(id)
        }
    };

    // Rewrite the Deezer description at every sync so it always matches the
    // Spotify one exactly (unconditional per user request).
    if !dry_run
        && deezer_playlist_id.is_some()
        && mirror.is_some()
    {
        match deezer
            .update_playlist_description(deezer_playlist_id.as_deref().unwrap(), &description)
            .await
        {
            Ok(()) => {
                if !description.is_empty() {
                    println!(
                        "{} Description synced to Deezer",
                        style("OK").green().bold()
                    );
                }
            }
            Err(e) => {
                // Best-effort: private GraphQL channel may be unavailable.
                println!(
                    "{} Description not synced: {}",
                    style("Warning:").yellow().bold(),
                    e
                );
            }
        }
    }

    let existing_ids: HashSet<String> = match &deezer_playlist_id {
        Some(id) => deezer
            .playlist_tracks(id)
            .await?
            .into_iter()
            .map(|t| t.id)
            .collect(),
        None => HashSet::new(),
    };

    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {bar:40.cyan/black} {pos}/{len} [{elapsed_precise}]")
            .unwrap()
            .progress_chars("##-"),
    );

    let mut to_add: Vec<String> = Vec::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut already = 0usize;
    let mut not_found: Vec<(SpotifyTrack, Option<TrackMatch>)> = Vec::new();
    let mut low_confidence: Vec<(SpotifyTrack, Option<TrackMatch>)> = Vec::new();
    let mut skipped_marked: Vec<SpotifyTrack> = Vec::new();
    let interactive_prompts = interactive && !dry_run;
    let mut skip_all_prompts = false;

    for track in &tracks {
        let key = store::key_for_track(track);
        if let Some(decision) = store.get(&key) {
            match decision {
                Decision::Add(id) => {
                    if !queue_add(id, &existing_ids, &mut queued, &mut to_add) {
                        already += 1;
                    }
                }
                Decision::Skip => skipped_marked.push(track.clone()),
            }
            pb.inc(1);
            continue;
        }

        let resolution = resolve_track(deezer, track, &existing_ids).await?;

        match resolution.best {
            Some(m) if existing_ids.contains(&m.cand_id) => {
                already += 1;
            }
            Some(m) if m.score >= AUTO_ADD_THRESHOLD => {
                if !queue_add(&m.cand_id, &existing_ids, &mut queued, &mut to_add) {
                    already += 1;
                }
            }
            Some(m) => {
                if interactive_prompts && !skip_all_prompts {
                    match resolve_interactively(
                        deezer,
                        track,
                        &resolution.candidates,
                        Some(&m.cand_id),
                        &pb,
                    )
                    .await
                    {
                        InteractiveChoice::Pick(id) => {
                            store.record_add(key.clone(), id.clone());
                            if !queue_add(&id, &existing_ids, &mut queued, &mut to_add) {
                                already += 1;
                            }
                        }
                        InteractiveChoice::SkipForever => {
                            store.record_skip(key.clone());
                            skipped_marked.push(track.clone());
                        }
                        InteractiveChoice::Skip => low_confidence.push((track.clone(), Some(m))),
                        InteractiveChoice::SkipAll => {
                            skip_all_prompts = true;
                            low_confidence.push((track.clone(), Some(m)));
                        }
                    }
                } else {
                    low_confidence.push((track.clone(), Some(m)));
                }
            }
            None => {
                if let Some(b) = &resolution.best_any
                    && existing_ids.contains(&b.cand_id)
                {
                    already += 1;
                    pb.inc(1);
                    continue;
                }
                if interactive_prompts && !skip_all_prompts && !resolution.candidates.is_empty() {
                    match resolve_interactively(deezer, track, &resolution.candidates, None, &pb)
                        .await
                    {
                        InteractiveChoice::Pick(id) => {
                            store.record_add(key.clone(), id.clone());
                            if !queue_add(&id, &existing_ids, &mut queued, &mut to_add) {
                                already += 1;
                            }
                        }
                        InteractiveChoice::SkipForever => {
                            store.record_skip(key.clone());
                            skipped_marked.push(track.clone());
                        }
                        InteractiveChoice::Skip => not_found.push((track.clone(), None)),
                        InteractiveChoice::SkipAll => {
                            skip_all_prompts = true;
                            not_found.push((track.clone(), None));
                        }
                    }
                } else {
                    not_found.push((track.clone(), None));
                }
            }
        }
        pb.inc(1);
    }
    pb.finish_and_clear();

    let added_count = to_add.len();
    if !dry_run && !to_add.is_empty() {
        let target = deezer_playlist_id.clone().unwrap();
        let batches = to_add.chunks(ADD_BATCH_SIZE).count();
        for (i, chunk) in to_add.chunks(ADD_BATCH_SIZE).enumerate() {
            pb.set_message(format!("adding to Deezer... batch {}/{}", i + 1, batches));
            if let Err(e) = deezer.add_tracks(&target, chunk).await {
                if e.to_string().contains("already exists") {
                    for id in chunk {
                        if let Err(e2) = deezer.add_tracks(&target, std::slice::from_ref(id)).await
                            && !e2.to_string().contains("already exists")
                        {
                            return Err(e2);
                        }
                    }
                } else {
                    return Err(e);
                }
            }
        }
    }

    if set_cover && !dry_run && let Some(target) = &deezer_playlist_id {
        let cover: Option<String> = match cover_url {
            Some(url) => Some(url.to_string()),
            None => match spotify.playlist_cover_url(&playlist_id).await {
                Ok(c) => c,
                Err(e) => {
                    println!(
                        "{} Could not fetch Spotify cover: {} (sync not affected)",
                        style("WARN").yellow().bold(),
                        e
                    );
                    None
                }
            },
        };
        if let Some(url) = cover {
            match download_and_set_cover(spotify, deezer, &url, target).await {
                Ok(()) => println!(
                    "{} Cover copied from Spotify to the Deezer playlist",
                    style("OK").green().bold()
                ),
                Err(e) => println!(
                    "{} Could not set Deezer cover: {} (sync not affected)",
                    style("WARN").yellow().bold(),
                    e
                ),
            }
        } else {
            println!(
                "{} Spotify playlist has no cover image — skipping Deezer cover",
                style("INFO").cyan().bold()
            );
        }
    }

    Ok(SyncStats {
        playlist_name: playlist_name.to_string(),
        dry_run,
        total,
        added: added_count,
        already,
        not_found,
        low_confidence,
        skipped_marked,
    })
}

struct Resolution {
    best: Option<TrackMatch>,
    best_any: Option<TrackMatch>,
    candidates: Vec<DeezerTrack>,
}

/// How deep into the search results ISRC matching looks.
const ISRC_CANDIDATES: usize = 20;

/// Case-insensitive ISRC comparison (both APIs may vary the casing/spacing).
/// Empty ISRCs never match.
fn isrc_equal(a: &str, b: &str) -> bool {
    let a = a.trim();
    let b = b.trim();
    !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b)
}

/// Returns a score-1.0 match when one of the candidates carries the same
/// ISRC as the Spotify track. The gw search results embed the ISRC, so this
/// costs no extra API call.
fn match_isrc(track: &SpotifyTrack, candidates: &[DeezerTrack]) -> Option<TrackMatch> {
    let query_isrc = track.isrc.as_deref()?;
    for cand in candidates.iter().take(ISRC_CANDIDATES) {
        if let Some(isrc) = &cand.isrc
            && isrc_equal(isrc, query_isrc)
        {
            return Some(TrackMatch {
                cand_id: cand.id.clone(),
                cand_title: cand.title.clone(),
                cand_artist: cand.artist.clone(),
                score: 1.0,
                duration_delta_s: (cand.duration - track.duration_ms / 1000).abs(),
            });
        }
    }
    None
}

/// Downloads the Spotify cover and applies it to the Deezer mirror.
async fn download_and_set_cover(
    spotify: &SpotifyClient,
    deezer: &DeezerClient,
    cover_url: &str,
    deezer_playlist_id: &str,
) -> Result<()> {
    let image = spotify.download_cover(cover_url).await?;
    deezer.set_playlist_cover(deezer_playlist_id, &image).await
}

pub(crate) fn queue_add(
    id: &str,
    existing: &HashSet<String>,
    queued: &mut HashSet<String>,
    to_add: &mut Vec<String>,
) -> bool {
    if existing.contains(id) || queued.contains(id) {
        return false;
    }
    queued.insert(id.to_string());
    to_add.push(id.to_string());
    true
}

/// Normalizes a playlist title for matching: keeps only alphanumeric
/// characters (unicode-aware), lowercased. This is deliberately aggressive:
/// Deezer trims trailing whitespace when storing titles, and invisible
/// characters (non-breaking/zero-width spaces, BOM) or punctuation variants
/// in a Spotify playlist name would otherwise make a freshly created mirror
/// unrecognizable on the next sync, piling up duplicates.
fn normalize_title(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

pub(crate) fn find_mirror<'a>(
    playlists: &'a [DeezerPlaylist],
    name: &str,
) -> Option<&'a DeezerPlaylist> {
    let target = normalize_title(name);
    // Deezer playlist ids grow over time: the lowest id is the oldest
    // playlist, which is the one whose track list is most complete.
    let by_age = |a: &DeezerPlaylist, b: &DeezerPlaylist| {
        let ka = a.id.parse::<u64>().unwrap_or(u64::MAX);
        let kb = b.id.parse::<u64>().unwrap_or(u64::MAX);
        ka.cmp(&kb)
    };
    playlists
        .iter()
        .filter(|p| p.owned && normalize_title(&p.title) == target)
        .min_by(|a, b| by_age(a, b))
        .or_else(|| {
            playlists
                .iter()
                .filter(|p| normalize_title(&p.title) == target)
                .min_by(|a, b| by_age(a, b))
        })
}

/// Handles the interactive prompt, including validation of manually pasted
/// Deezer URLs (re-prompting until a valid choice is made).
async fn resolve_interactively(
    deezer: &DeezerClient,
    track: &SpotifyTrack,
    candidates: &[DeezerTrack],
    best_id: Option<&str>,
    pb: &ProgressBar,
) -> InteractiveChoice {
    loop {
        match prompt_candidates(track, candidates, best_id, pb) {
            PromptAction::Manual(raw) => match deezer.resolve_manual_track(&raw).await {
                Ok(id) => match deezer.track_by_id(&id).await {
                    Ok(t) => {
                        pb.suspend(|| {
                            println!(
                                "{} {} - {} ({})",
                                style("Manual pick:").green().bold(),
                                t.title,
                                t.artist,
                                spotify::format_duration(t.duration * 1000)
                            );
                        });
                        return InteractiveChoice::Pick(id);
                    }
                    Err(e) => {
                        pb.suspend(|| {
                            println!(
                                "{} {} — paste another URL or pick a candidate.",
                                style("Invalid track:").red().bold(),
                                e
                            );
                        });
                        continue;
                    }
                },
                Err(e) => {
                    pb.suspend(|| {
                        println!(
                            "{} {} — paste another URL or pick a candidate.",
                            style("Invalid track:").red().bold(),
                            e
                        );
                    });
                    continue;
                }
            },
            PromptAction::Pick(id) => return InteractiveChoice::Pick(id),
            PromptAction::Skip => return InteractiveChoice::Skip,
            PromptAction::SkipForever => return InteractiveChoice::SkipForever,
            PromptAction::SkipAll => return InteractiveChoice::SkipAll,
        }
    }
}

async fn resolve_track(
    deezer: &DeezerClient,
    track: &SpotifyTrack,
    existing_deezer_ids: &HashSet<String>,
) -> Result<Resolution> {
    let title_main = track.title.split(" - ").next().unwrap_or(&track.title).trim();
    let primary_artist = track.artist.split(',').next().unwrap_or("").trim();

    let queries: Vec<String> = if primary_artist.is_empty() {
        vec![title_main.to_string()]
    } else {
        vec![
            format!("{} {}", title_main, primary_artist),
            title_main.to_string(),
        ]
    };

    let mut candidates: Vec<DeezerTrack> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for query in queries {
        for c in deezer.search_track(&query).await?.into_iter().take(20) {
            if seen.insert(c.id.clone()) {
                candidates.push(c);
            }
        }
    }

    // ISRC check first: an identical recording identifier is definitive and
    // beats any fuzzy score. The gw search embeds ISRCs, so this is free.
    if let Some(m) = match_isrc(track, &candidates) {
        return Ok(Resolution {
            best: Some(m),
            best_any: None,
            candidates: Vec::new(),
        });
    }

    let best_any = matching::scored_best(track, &candidates);
    let best = best_any
        .clone()
        .filter(|m| m.score >= matching::MATCH_THRESHOLD);
    if let Some(m) = &best
        && existing_deezer_ids.contains(&m.cand_id)
    {
        return Ok(Resolution {
            best,
            best_any,
            candidates: Vec::new(),
        });
    }

    Ok(Resolution {
        best,
        best_any,
        candidates,
    })
}

/// Formats a share of `total` as a percentage suffix, e.g. `" (85.7%)"`.
/// Empty string when there is nothing to divide by.
pub(crate) fn pct(n: usize, total: usize) -> String {
    if total == 0 {
        String::new()
    } else {
        format!(" ({:.1}%)", n as f64 / total as f64 * 100.0)
    }
}

pub fn print_report(stats: &SyncStats) {
    println!();
    println!("{} {}", style("=== Sync report ===").bold(), if stats.dry_run { "(dry run)" } else { "" });
    println!(
        "  {}: {} tracks",
        style(&stats.playlist_name).bold(),
        stats.total
    );
    println!("  {} added{}", style(stats.added).green(), pct(stats.added, stats.total));
    println!(
        "  {} already in Deezer playlist{}",
        style(stats.already).dim(),
        pct(stats.already, stats.total)
    );
    println!(
        "  {} not found on Deezer{}",
        style(stats.not_found.len()).red(),
        pct(stats.not_found.len(), stats.total)
    );
    println!(
        "  {} low confidence matches (skipped){}",
        style(stats.low_confidence.len()).yellow(),
        pct(stats.low_confidence.len(), stats.total)
    );
    println!(
        "  {} skipped (marked as not on Deezer, edit ~/.config/sync-pl/track_decisions.json to undo){}",
        style(stats.skipped_marked.len()).dim(),
        pct(stats.skipped_marked.len(), stats.total)
    );
    let matched = stats.added + stats.already;
    println!(
        "  {} matched overall{}",
        style(matched).cyan(),
        pct(matched, stats.total)
    );

    if !stats.not_found.is_empty() {
        println!();
        println!("{} not found:", style("✗").red());
        for (track, m) in &stats.not_found {
            match m {
                Some(m) => println!(
                    "    {} - {} (best: '{}' by {}, score {:.2})",
                    track.title,
                    track.artist,
                    m.cand_title,
                    m.cand_artist,
                    m.score
                ),
                None => println!("    {} - {}", track.title, track.artist),
            }
        }
    }

    if !stats.low_confidence.is_empty() {
        println!();
        println!("{} low confidence (review manually):", style("⚠").yellow());
        for (track, m) in &stats.low_confidence {
            let m = m.as_ref().unwrap();
            println!(
                "    {} - {}  →  '{}' - {}  ({:.2})  {}",
                track.title,
                track.artist,
                m.cand_title,
                m.cand_artist,
                m.score,
                style(format!("https://www.deezer.com/track/{}", m.cand_id)).dim()
            );
        }
    }
}
pub struct DiffStats {
    pub playlist_name: String,
    pub spotify_count: usize,
    pub deezer_count: usize,
    pub matched: usize,
    pub spotify_only: Vec<SpotifyTrack>,
    pub deezer_only: Vec<DeezerTrack>,
}

/// Compares a Spotify playlist with its same-named Deezer playlist: matched
/// tracks are claimed greedily (store decisions first, then local matching),
/// everything left over is reported on either side.
pub async fn diff_playlist(
    spotify: &SpotifyClient,
    deezer: &DeezerClient,
    store: &TrackStore,
    playlist_name: &str,
) -> Result<DiffStats> {
    let playlist_id = spotify
        .playlist_id(playlist_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Playlist '{}' not found on Spotify", playlist_name))?;
    let sp_tracks = spotify.playlist_tracks(&playlist_id).await?;

    let existing = deezer.user_playlists().await?;
    let mirror = find_mirror(&existing, playlist_name)
        .ok_or_else(|| anyhow::anyhow!("No Deezer playlist named '{}'", playlist_name))?;
    let dz_tracks = deezer.playlist_tracks(&mirror.id).await?;

    let mut claimed: HashSet<String> = HashSet::new();
    let mut matched = 0usize;
    let mut spotify_only: Vec<SpotifyTrack> = Vec::new();

    for t in &sp_tracks {
        let mut resolved: Option<String> = None;
        if let Some(Decision::Add(id)) = store.get(&store::key_for_track(t))
            && dz_tracks.iter().any(|d| &d.id == id)
            && claimed.insert(id.clone())
        {
            resolved = Some(id.clone());
        }
        if resolved.is_none()
            && let Some(m) = matching::scored_best(t, &dz_tracks)
            && m.score >= matching::MATCH_THRESHOLD
            && claimed.insert(m.cand_id.clone())
        {
            resolved = Some(m.cand_id.clone());
        }
        match resolved {
            Some(_) => matched += 1,
            None => spotify_only.push(t.clone()),
        }
    }

    let deezer_total = dz_tracks.len();
    let deezer_only: Vec<DeezerTrack> = dz_tracks
        .into_iter()
        .filter(|d| !claimed.contains(&d.id))
        .collect();

    Ok(DiffStats {
        playlist_name: playlist_name.to_string(),
        spotify_count: sp_tracks.len(),
        deezer_count: deezer_total,
        matched,
        spotify_only,
        deezer_only,
    })
}

pub fn print_diff(stats: &DiffStats) {
    println!();
    println!(
        "{} {}",
        style("=== Diff ===").bold(),
        stats.playlist_name
    );
    println!(
        "  Spotify: {} tracks | Deezer playlist: {} tracks | matched: {}",
        style(stats.spotify_count).bold(),
        style(stats.deezer_count).bold(),
        style(stats.matched).green()
    );

    if !stats.spotify_only.is_empty() {
        println!();
        println!(
            "{} in Spotify but missing from the Deezer playlist ({}):",
            style("→").yellow(),
            stats.spotify_only.len()
        );
        for (i, t) in stats.spotify_only.iter().enumerate() {
            println!("  {:>3}. {} - {}", i + 1, t.title, t.artist);
        }
    }

    if !stats.deezer_only.is_empty() {
        println!();
        println!(
            "{} in the Deezer playlist but not coming from Spotify ({}):",
            style("←").cyan(),
            stats.deezer_only.len()
        );
        for (i, t) in stats.deezer_only.iter().enumerate() {
            println!("  {:>3}. {} - {}  {}", i + 1, t.title, t.artist, style(t.url()).dim());
        }
        println!();
        println!(
            "{} {} tracks could be imported → run: {}",
            style("ℹ").cyan(),
            stats.deezer_only.len(),
            style(format!("sync-pl import {}", stats.playlist_name)).bold()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(id: &str, title: &str, owned: bool) -> DeezerPlaylist {
        DeezerPlaylist {
            id: id.to_string(),
            title: title.to_string(),
            owned,
            description: String::new(),
        }
    }

    #[test]
    fn finds_mirror_case_insensitive() {
        let existing = vec![
            playlist("1", "'90s Rap", false),
            playlist("2", "Mixed", true),
        ];
        let found = find_mirror(&existing, "mixed");
        assert_eq!(found.map(|p| p.id.as_str()), Some("2"));
    }

    #[test]
    fn finds_mirror_with_invisible_whitespace() {
        // Deezer trims trailing whitespace when storing a title, so a Spotify
        // name like "Hip Hop Old School " must still match the stored
        // "Hip Hop Old School".
        let existing = vec![playlist("5", "Hip Hop Old School", true)];
        let found = find_mirror(&existing, "Hip Hop Old School ");
        assert_eq!(found.map(|p| p.id.as_str()), Some("5"));
        let found = find_mirror(&existing, "hip hop\u{00a0}old\u{00a0}school");
        assert_eq!(found.map(|p| p.id.as_str()), Some("5"));
    }

    #[test]
    fn prefers_oldest_mirror_among_duplicates() {
        let existing = vec![
            playlist("9", "Hip Hop Old School", true),
            playlist("4", "Hip Hop Old School ", true),
            playlist("7", "Hip Hop Old School", true),
        ];
        let found = find_mirror(&existing, "Hip Hop Old School");
        assert_eq!(found.map(|p| p.id.as_str()), Some("4"));
    }

    #[test]
    fn prefers_owned_mirror() {
        let existing = vec![
            playlist("1", "mixed", false),
            playlist("2", "MiXeD", true),
        ];
        let found = find_mirror(&existing, "mixed");
        assert_eq!(found.map(|p| p.id.as_str()), Some("2"));
    }

    #[test]
    fn falls_back_to_followed_playlist() {
        let existing = vec![playlist("1", "mixed", false)];
        let found = find_mirror(&existing, "mixed");
        assert_eq!(found.map(|p| p.id.as_str()), Some("1"));
    }

    #[test]
    fn no_mirror_when_absent() {
        let existing = vec![playlist("1", "Jazzy Hip-Hop", true)];
        assert!(find_mirror(&existing, "mixed").is_none());
    }

    #[test]
    fn queue_add_dedupes_and_skips_existing() {
        let existing: HashSet<String> = ["1"].iter().map(|s| s.to_string()).collect();
        let mut queued: HashSet<String> = HashSet::new();
        let mut to_add: Vec<String> = Vec::new();

        assert!(queue_add("2", &existing, &mut queued, &mut to_add));
        assert!(!queue_add("2", &existing, &mut queued, &mut to_add));
        assert!(!queue_add("1", &existing, &mut queued, &mut to_add));
        assert_eq!(to_add, vec!["2".to_string()]);
    }

    #[test]
    fn isrc_comparison_is_case_insensitive_and_trims() {
        assert!(isrc_equal("GBDUW0000053", "GBDUW0000053"));
        assert!(isrc_equal("gbduw0000053", "GBDUW0000053"));
        assert!(isrc_equal(" GBDUW0000053 ", "GBDUW0000053"));
        assert!(!isrc_equal("GBDUW0000053", "GBDUW0000054"));
        assert!(!isrc_equal("", ""));
        assert!(!isrc_equal("GBDUW0000053", ""));
    }

    #[test]
    fn pct_formats_share_of_total() {
        assert_eq!(pct(85, 100), " (85.0%)");
        assert_eq!(pct(1, 3), " (33.3%)");
        assert_eq!(pct(7, 7), " (100.0%)");
        assert_eq!(pct(0, 0), "");
        assert_eq!(pct(5, 0), "");
    }
}
