use crate::deezer::{extract_playlist_id, DeezerClient, DeezerTrack};
use crate::matching::{self, Matchable, TrackMatch};
use crate::prompt::{prompt_candidates, InteractiveChoice, PromptAction};
use crate::spotify::{self, SpotifyClient, SpotifyTrack};
use crate::spotify_web;
use crate::store::{self, Decision, TrackStore};
use crate::sync::{find_mirror, queue_add};
use anyhow::{Context, Result};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;

const ADD_BATCH_SIZE: usize = 100;

pub struct ImportStats {
    pub playlist_name: String,
    pub dry_run: bool,
    pub total: usize,
    pub added: usize,
    pub already: usize,
    pub not_found: Vec<(DeezerTrack, Option<TrackMatch>)>,
    pub low_confidence: Vec<(DeezerTrack, Option<TrackMatch>)>,
    pub skipped_marked: Vec<DeezerTrack>,
}

pub async fn import_playlist(
    spotify: &SpotifyClient,
    deezer: &DeezerClient,
    store: &mut TrackStore,
    playlist_name: &str,
    dry_run: bool,
    interactive: bool,
    web: Option<&spotify_web::SpotifyWebClient>,
) -> Result<ImportStats> {
    let backend = crate::search_backend::SearchBackend::new(spotify, web);
    // Accept a playlist URL/ID as well as a name.
    let (source, playlist_name) = if let Some(id) = extract_playlist_id(playlist_name) {
        let title = deezer
            .playlist_title(&id)
            .await
            .with_context(|| format!("Deezer playlist {id} not found or inaccessible"))?;
        println!(
            "{} Deezer playlist by URL/ID: {} ({})",
            style("Found").green().bold(),
            title,
            id
        );
        (id, title)
    } else {
        let existing = deezer.user_playlists().await?;
        let source = find_mirror(&existing, playlist_name)
            .ok_or_else(|| anyhow::anyhow!("No Deezer playlist named '{}'", playlist_name))?;
        (source.id.clone(), playlist_name.to_string())
    };
    let dz_tracks = deezer.playlist_tracks(&source).await?;
    let total = dz_tracks.len();
    println!(
        "{} {} tracks",
        style("Deezer playlist:").bold(),
        total
    );

    let sp_playlist_id: Option<String> = match spotify.playlist_id(&playlist_name).await? {
        Some(id) => {
            println!(
                "{} Spotify playlist found: {}",
                style("Found").green().bold(),
                playlist_name
            );
            Some(id)
        }
        None if dry_run => {
            println!(
                "{} Spotify playlist '{}' would be created",
                style("DRY-RUN").yellow().bold(),
                playlist_name
            );
            None
        }
        None => {
            println!(
                "{} Creating Spotify playlist '{}'...",
                style("Creating").cyan().bold(),
                playlist_name
            );
            let id = spotify.create_playlist(&playlist_name).await?;
            println!(
                "{} Created Spotify playlist '{}'",
                style("OK").green().bold(),
                playlist_name
            );
            Some(id)
        }
    };

    let existing_ids: HashSet<String> = match &sp_playlist_id {
        Some(id) => spotify.playlist_track_ids(id).await?,
        None => HashSet::new(),
    };

    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} {bar:40.cyan/black} {pos}/{len} [{elapsed_precise}]",
        )
        .unwrap()
        .progress_chars("##-"),
    );

    let mut to_add: Vec<String> = Vec::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut already = 0usize;
    let mut not_found: Vec<(DeezerTrack, Option<TrackMatch>)> = Vec::new();
    let mut low_confidence: Vec<(DeezerTrack, Option<TrackMatch>)> = Vec::new();
    let mut skipped_marked: Vec<DeezerTrack> = Vec::new();
    let interactive_prompts = interactive && !dry_run;
    let mut skip_all_prompts = false;

    for track in &dz_tracks {
        let key = store::key_for_deezer_track(track);
        match store.get(&key) {
            Some(Decision::Add(id)) => {
                if !queue_add(id, &existing_ids, &mut queued, &mut to_add) {
                    already += 1;
                }
                pb.inc(1);
                continue;
            }
            Some(Decision::Skip) => {
                skipped_marked.push(track.clone());
                pb.inc(1);
                continue;
            }
            None => {}
        }

        let resolution = resolve_spotify_track(&backend, track).await?;

        match resolution.best {
            Some(m) if existing_ids.contains(&m.cand_id) => {
                already += 1;
            }
            Some(m) if m.score >= matching::AUTO_THRESHOLD => {
                if !queue_add(&m.cand_id, &existing_ids, &mut queued, &mut to_add) {
                    already += 1;
                }
            }
            Some(m) => {
                if interactive_prompts && !skip_all_prompts {
                    match interact(spotify, track, &resolution.candidates, Some(&m.cand_id), &pb)
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
                    match interact(spotify, track, &resolution.candidates, None, &pb).await {
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
        let target = sp_playlist_id.clone().unwrap();
        let batches = to_add.chunks(ADD_BATCH_SIZE).count();
        for (i, chunk) in to_add.chunks(ADD_BATCH_SIZE).enumerate() {
            pb.set_message(format!("adding to Spotify... batch {}/{}", i + 1, batches));
            if let Err(e) = spotify.add_tracks(&target, chunk).await {
                pb.suspend(|| {
                    println!(
                        "{} {} — retrying track by track",
                        style("Batch failed:").red().bold(),
                        e
                    );
                });
                for id in chunk {
                    spotify.add_tracks(&target, std::slice::from_ref(id)).await?;
                }
            }
        }
    }

    Ok(ImportStats {
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

struct SpResolution {
    best: Option<TrackMatch>,
    best_any: Option<TrackMatch>,
    candidates: Vec<SpotifyTrack>,
}

async fn resolve_spotify_track(
    backend: &crate::search_backend::SearchBackend<'_>,
    track: &DeezerTrack,
) -> Result<SpResolution> {
    let title_main = track.title.split(" - ").next().unwrap_or(&track.title).trim();
    let artist = track.artist.trim();

    let queries: Vec<String> = if artist.is_empty() {
        vec![title_main.to_string()]
    } else {
        vec![
            format!("{} {}", title_main, artist),
            title_main.to_string(),
        ]
    };

    let mut candidates: Vec<SpotifyTrack> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for query in queries {
        for c in backend.search_track(&query).await?.into_iter().take(8) {
            if seen.insert(c.match_id().to_string()) {
                candidates.push(c);
            }
        }
    }

    let best_any = matching::scored_best(track, &candidates);
    let best = best_any
        .clone()
        .filter(|m| m.score >= matching::MATCH_THRESHOLD);

    Ok(SpResolution {
        best,
        best_any,
        candidates,
    })
}

async fn interact(
    spotify: &SpotifyClient,
    track: &DeezerTrack,
    candidates: &[SpotifyTrack],
    best_id: Option<&str>,
    pb: &ProgressBar,
) -> InteractiveChoice {
    loop {
        match prompt_candidates(track, candidates, best_id, pb) {
            PromptAction::Manual(raw) => {
                let Some(id) = spotify::extract_spotify_track_id(&raw) else {
                    pb.suspend(|| {
                        println!(
                            "{} not a Spotify track URL or ID — paste another or pick a candidate.",
                            style("Invalid input:").red().bold()
                        );
                    });
                    continue;
                };
                match spotify.track_by_id(&id).await {
                    Ok(t) => {
                        pb.suspend(|| {
                            println!(
                                "{} {} - {} ({})",
                                style("Manual pick:").green().bold(),
                                t.title,
                                t.artist,
                                spotify::format_duration(t.duration_ms)
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
                }
            }
            PromptAction::Pick(id) => return InteractiveChoice::Pick(id),
            PromptAction::Skip => return InteractiveChoice::Skip,
            PromptAction::SkipForever => return InteractiveChoice::SkipForever,
            PromptAction::SkipAll => return InteractiveChoice::SkipAll,
        }
    }
}

pub fn print_import_report(stats: &ImportStats) {
    println!();
    println!(
        "{} {}",
        style("=== Import report ===").bold(),
        if stats.dry_run { "(dry run)" } else { "" }
    );
    println!(
        "  {}: {} tracks",
        style(&stats.playlist_name).bold(),
        stats.total
    );
    println!("  {} added{}", style(stats.added).green(), crate::sync::pct(stats.added, stats.total));
    println!(
        "  {} already in Spotify playlist{}",
        style(stats.already).dim(),
        crate::sync::pct(stats.already, stats.total)
    );
    println!(
        "  {} not found on Spotify{}",
        style(stats.not_found.len()).red(),
        crate::sync::pct(stats.not_found.len(), stats.total)
    );
    println!(
        "  {} low confidence matches (skipped){}",
        style(stats.low_confidence.len()).yellow(),
        crate::sync::pct(stats.low_confidence.len(), stats.total)
    );
    println!(
        "  {} skipped (marked as not on Spotify, edit ~/.config/sync-pl/track_decisions.json to undo){}",
        style(stats.skipped_marked.len()).dim(),
        crate::sync::pct(stats.skipped_marked.len(), stats.total)
    );
    let matched = stats.added + stats.already;
    println!(
        "  {} matched overall{}",
        style(matched).cyan(),
        crate::sync::pct(matched, stats.total)
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
                style(format!("https://open.spotify.com/track/{}", m.cand_id)).dim()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_spotify_urls() {
        assert_eq!(
            spotify::extract_spotify_track_id("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=abc"),
            Some("4cOdK2wGLETKBW3PvgPWqT".to_string())
        );
        assert_eq!(
            spotify::extract_spotify_track_id(
                "https://open.spotify.com/intl-fr/track/5mg0SOxAc2Fbp5PaOlWYW9?si=56116e3c37044e17"
            ),
            Some("5mg0SOxAc2Fbp5PaOlWYW9".to_string())
        );
        assert_eq!(
            spotify::extract_spotify_track_id("https://open.spotify.com/embed/track/4cOdK2wGLETKBW3PvgPWqT"),
            Some("4cOdK2wGLETKBW3PvgPWqT".to_string())
        );
        assert_eq!(
            spotify::extract_spotify_track_id("spotify:track:4cOdK2wGLETKBW3PvgPWqT"),
            Some("4cOdK2wGLETKBW3PvgPWqT".to_string())
        );
        assert_eq!(
            spotify::extract_spotify_track_id("4cOdK2wGLETKBW3PvgPWqT"),
            Some("4cOdK2wGLETKBW3PvgPWqT".to_string())
        );
        assert_eq!(
            spotify::extract_spotify_track_id("https://open.spotify.com/album/xyz"),
            None
        );
        assert_eq!(spotify::extract_spotify_track_id(""), None);
    }
}
