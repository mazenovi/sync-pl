//! Imports a CSV into a Spotify playlist and exports playlists back to CSV.
//!
//! The CSV holds the truth: one row per track, in target order. The link
//! column (first by default, selectable via --column) must contain Spotify
//! track links; any other non-empty content there is a hard error.
//! Rows whose link cell is empty can be resolved interactively (--interactive)
//! with the same candidate-picking principle as `import-deezer`.
//!
//! If the playlist exists it is conformed exactly to the CSV; if it doesn't
//! exist it is created.

use crate::matching::{self, Matchable};
use crate::prompt::{prompt_candidates, PromptAction};
use crate::spotify::{self, SpotifyClient};
use crate::store::{Decision, TrackStore};
use anyhow::{Context, Result};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;

pub struct ImportCsvStats {
    pub playlist_name: String,
    pub dry_run: bool,
    /// Ordered track IDs parsed from the CSV (the target order).
    pub target_ids: Vec<String>,
    /// Number of data rows in the CSV (header excluded).
    pub rows: usize,
    /// Rows with no link, resolved interactively (pick or skip).
    pub resolved_manually: usize,
    /// Rows with no link, left unresolved.
    pub unresolved: Vec<String>,
    /// Duplicate links ignored.
    pub duplicates: usize,
    /// Tracks currently in the playlist but absent from the CSV.
    pub removed: Vec<String>,
    /// Tracks to add to the playlist.
    pub to_add: Vec<String>,
    /// Whether the existing order already matches the target order.
    pub reorder_needed: bool,
    pub create_playlist: bool,
}

pub async fn import_csv_playlist(
    spotify: &SpotifyClient,
    file: &str,
    playlist_name: &str,
    column: Option<&str>,
    playlist_id: Option<&str>,
    dry_run: bool,
    interactive: bool,
) -> Result<ImportCsvStats> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("Failed to read CSV file '{file}'"))?;

    // Phase 1 — pure parsing: the CSV must be valid before touching Spotify.
    let rows = parse_csv(&content, column, file)?;
    let total = rows.len();

    // Phase 2 — resolve rows without links (interactive only).
    let mut store = TrackStore::load();
    let interactive_prompts = interactive && !dry_run;
    let mut target_ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut duplicates = 0usize;
    let mut resolved_manually = 0usize;
    let mut unresolved: Vec<String> = Vec::new();

    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} {bar:40.cyan/black} {pos}/{len} [{elapsed_precise}]",
        )
        .unwrap()
        .progress_chars("##-"),
    );

    for row in &rows {
        match &row.link {
            Some(link) => {
                if seen.insert(link.clone()) {
                    target_ids.push(link.clone());
                } else {
                    duplicates += 1;
                }
            }
            None => {
                let key = format!("csv:{}", matching::normalize(&row.query));
                match store.get(&key) {
                    Some(Decision::Add(id)) => {
                        if seen.insert(id.clone()) {
                            target_ids.push(id.clone());
                        } else {
                            duplicates += 1;
                        }
                        pb.inc(1);
                        continue;
                    }
                    Some(Decision::Skip) => {
                        unresolved.push(row.query.clone());
                        pb.inc(1);
                        continue;
                    }
                    None => {}
                }

                if !interactive_prompts {
                    unresolved.push(row.query.clone());
                    pb.inc(1);
                    continue;
                }

                let candidates = if row.query.is_empty() {
                    Vec::new()
                } else {
                    spotify.search_track(&row.query).await.unwrap_or_default()
                };

                let mut row_unresolved = false;
                if candidates.is_empty() {
                    // Manual entry only.
                    match prompt_manual(spotify, &row.query, &pb).await {
                        Some(id) => {
                            store.record_add(key.clone(), id.clone());
                            resolved_manually += 1;
                            if seen.insert(id.clone()) {
                                target_ids.push(id);
                            }
                        }
                        None => row_unresolved = true,
                    }
                } else {
                    match prompt_candidates(row, &candidates, None, &pb) {
                        PromptAction::Pick(id) => {
                            resolved_manually += 1;
                            if seen.insert(id.clone()) {
                                target_ids.push(id);
                            }
                        }
                        PromptAction::Manual(raw) => match spotify::extract_spotify_track_id(&raw)
                        {
                            Some(id) => match spotify.track_by_id(&id).await {
                                Ok(_) => {
                                    store.record_add(key.clone(), id.clone());
                                    resolved_manually += 1;
                                    if seen.insert(id.clone()) {
                                        target_ids.push(id);
                                    }
                                }
                                Err(_) => row_unresolved = true,
                            },
                            None => {
                                pb.suspend(|| {
                                    println!(
                                        "{} not a Spotify track URL or ID.",
                                        style("Invalid input:").red().bold()
                                    );
                                });
                                row_unresolved = true;
                            }
                        },
                        PromptAction::Skip | PromptAction::SkipForever | PromptAction::SkipAll => {
                            row_unresolved = true;
                        }
                    }
                }
                if row_unresolved {
                    unresolved.push(row.query.clone());
                }
                pb.inc(1);
            }
        }
    }
    pb.finish_and_clear();

    if !unresolved.is_empty() {
        println!(
            "{} {} row(s) without a link could not be resolved{}",
            style("Warning:").yellow().bold(),
            unresolved.len(),
            if interactive { "" } else { " (run with --interactive to resolve them)" }
        );
    }

    if target_ids.is_empty() {
        anyhow::bail!("No Spotify track link found in '{file}'");
    }

    println!(
        "{} {} rows ({} unique links){}",
        style("CSV file:").bold(),
        total,
        target_ids.len(),
        if duplicates > 0 {
            format!(", {} duplicate(s) ignored", duplicates)
        } else {
            String::new()
        }
    );

    // Phase 3 — playlist: create when missing, conform when existing.
    // With --playlist-id the target is given explicitly: no me/playlists
    // listing at all (that endpoint has its own long quota windows).
    let sp_playlist_id = if let Some(pid) = playlist_id {
        println!(
            "{} Spotify playlist by ID: {} (skipping me/playlists)",
            style("Target").green().bold(),
            pid
        );
        (pid.to_string(), false)
    } else {
        match spotify.playlist_id(playlist_name).await? {
            Some(id) => {
                println!(
                    "{} Spotify playlist found: {}",
                    style("Found").green().bold(),
                    playlist_name
                );
                (id, false)
            }
            None if dry_run => {
                println!(
                    "{} Spotify playlist '{}' would be created",
                    style("DRY-RUN").yellow().bold(),
                    playlist_name
                );
                (String::new(), true)
            }
            None => {
                println!(
                    "{} Creating Spotify playlist '{}'...",
                    style("Creating").cyan().bold(),
                    playlist_name
                );
                let id = spotify.create_playlist(playlist_name).await?;
                println!(
                    "{} Created Spotify playlist '{}'",
                    style("OK").green().bold(),
                    playlist_name
                );
                (id, true)
            }
        }
    };
    let (sp_playlist_id, created) = sp_playlist_id;

    // Diff current content vs target.
    let current_ids: Vec<String> = if sp_playlist_id.is_empty() {
        Vec::new()
    } else {
        spotify.playlist_track_ids_ordered(&sp_playlist_id).await?
    };

    let current_set: HashSet<&String> = current_ids.iter().collect();
    let target_set: HashSet<&String> = target_ids.iter().collect();

    let to_add: Vec<String> = target_ids
        .iter()
        .filter(|id| !current_set.contains(id))
        .cloned()
        .collect();
    let removed: Vec<String> = current_ids
        .iter()
        .filter(|id| !target_set.contains(id))
        .cloned()
        .collect();
    let reorder_needed = !created && current_ids != target_ids;

    // Write: full replace guarantees the exact order.
    if !dry_run && !created && (reorder_needed || !to_add.is_empty() || !removed.is_empty()) {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg} [{elapsed_precise}]")
                .unwrap()
                .progress_chars("##-"),
        );
        pb.set_message("conforming playlist to CSV...");
        spotify.replace_tracks(&sp_playlist_id, &target_ids).await?;
        pb.finish_and_clear();
    }

    Ok(ImportCsvStats {
        playlist_name: playlist_name.to_string(),
        dry_run,
        target_ids,
        rows: total,
        resolved_manually,
        unresolved,
        duplicates,
        removed,
        to_add,
        reorder_needed,
        create_playlist: created,
    })
}

/// Minimal pastable query row (used only for the manual-entry prompt when the
/// CSV row has no candidate-generating text).
async fn prompt_manual(spotify: &SpotifyClient, query: &str, pb: &ProgressBar) -> Option<String> {
    loop {
        pb.suspend(|| {
            println!();
            println!("{} (row: \"{}\")", style("No link in CSV row:").yellow().bold(), query);
            print!("Paste a Spotify track URL, or press Enter to skip: ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        });
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let Some(id) = spotify::extract_spotify_track_id(line) else {
            pb.suspend(|| {
                println!(
                    "{} not a Spotify track URL or ID.",
                    style("Invalid input:").red().bold()
                );
            });
            continue;
        };
        return match spotify.track_by_id(&id).await {
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
                Some(id)
            }
            Err(_) => None,
        };
    }
}

// ---------------------------------------------------------------------------
// CSV parsing
// ---------------------------------------------------------------------------

/// One parsed CSV data row.
#[derive(Debug)]
#[allow(dead_code)]
struct CsvRow {
    /// 1-based line number in the file (for error messages).
    line_no: usize,
    /// Spotify track ID when the link cell holds a valid link.
    link: Option<String>,
    /// Non-link cells of the row, used as a search query for --interactive.
    query: String,
}

impl Matchable for CsvRow {
    fn match_title(&self) -> &str {
        self.query.split_whitespace().next().unwrap_or("")
    }
    fn match_artist(&self) -> &str {
        // Everything after the first word, when present.
        match self.query.split_once(char::is_whitespace) {
            Some((_, rest)) => rest.trim(),
            None => "",
        }
    }
    fn match_duration_s(&self) -> i64 {
        0
    }
    fn match_id(&self) -> &str {
        ""
    }
    fn match_url(&self) -> String {
        String::new()
    }
}

/// Parses the CSV and extracts the link column. Hard-fails — explaining the
/// problem — when a non-empty cell in the link column is not a Spotify track
/// link. A first line that holds no valid link anywhere is treated as a
/// header.
fn parse_csv(content: &str, column: Option<&str>, file: &str) -> Result<Vec<CsvRow>> {
    let mut records: Vec<Vec<String>> = Vec::new();
    for record in csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(content.as_bytes())
        .records()
    {
        let record = record
            .with_context(|| format!("Failed to parse '{file}' as CSV"))?;
        records.push(record.iter().map(|c| c.to_string()).collect());
    }
    if records.is_empty() {
        anyhow::bail!("'{file}' is empty");
    }

    // Header detection: the first line is a header when no cell in it is a
    // valid track link.
    let header = if records[0]
        .iter()
        .all(|c| parse_csv_token(c.trim()).is_none())
    {
        Some(records.remove(0))
    } else {
        None
    };

    let col = resolve_column(column, header.as_ref(), file)?;

    let mut rows = Vec::new();
    for (i, record) in records.iter().enumerate() {
        let line_no = i + if header.is_some() { 2 } else { 1 };
        let cell = record.get(col).map(|c| c.trim()).unwrap_or("");
        if cell.is_empty() {
            let query = other_cells_query(record, col);
            rows.push(CsvRow {
                line_no,
                link: None,
                query,
            });
            continue;
        }
        let Some(link) = parse_csv_token(cell) else {
            let col_desc = match column {
                Some(spec) => spec.to_string(),
                None => "1 (default)".to_string(),
            };
            anyhow::bail!(
                "'{file}' line {line_no}: expected a Spotify track link in column {col_desc}, \
found: \"{cell}\".\nThe CSV must contain one Spotify track link per row in that column \
(order is kept). If the links live in another column, select it with \
--column <letter|index|header-name>."
            );
        };
        rows.push(CsvRow {
            line_no,
            link: Some(link),
            query: String::new(),
        });
    }
    Ok(rows)
}

/// Joins the non-link cells of a row into a search query (title/artist hint
/// for the interactive prompt).
fn other_cells_query(record: &[String], skip_col: usize) -> String {
    record
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != skip_col)
        .map(|(_, c)| c.trim())
        .filter(|c| !c.is_empty() && parse_csv_token(c).is_none())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolves the link column from a letter (A = 1st), an index (1-based), or a
/// header name. Defaults to the first column.
fn resolve_column(
    spec: Option<&str>,
    header: Option<&Vec<String>>,
    file: &str,
) -> Result<usize> {
    let Some(spec) = spec else {
        return Ok(0);
    };
    let spec = spec.trim();
    if spec.len() == 1 && spec.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        let c = spec.to_ascii_uppercase().chars().next().unwrap();
        return Ok(c as usize - 'A' as usize);
    }
    if let Ok(n) = spec.parse::<usize>() {
        if n == 0 {
            anyhow::bail!("Invalid --column '{spec}': index is 1-based (first column = 1)");
        }
        return Ok(n - 1);
    }
    let Some(header) = header else {
        anyhow::bail!(
            "--column '{spec}' is a header name but '{file}' has no header line"
        );
    };
    header
        .iter()
        .position(|h| h.trim().eq_ignore_ascii_case(spec))
        .with_context(|| {
            format!(
                "No column named '{spec}' in '{file}' (header: {})",
                header.join(", ")
            )
        })
}

/// A CSV cell holds a track when it's a full URL/URI or a bare base-62 ID of
/// the exact Spotify length (22) — bare words like "link" must not match.
fn parse_csv_token(token: &str) -> Option<String> {
    if token.contains('/') || token.contains(':') {
        return spotify::extract_spotify_track_id(token);
    }
    if token.len() == 22 && token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return spotify::extract_spotify_track_id(token);
    }
    None
}

pub fn print_report(stats: &ImportCsvStats) {
    println!();
    println!(
        "{} {}",
        style("=== CSV import report ===").bold(),
        if stats.dry_run { "(dry run)" } else { "" }
    );
    println!("  {}: {}", style(&stats.playlist_name).bold(), stats.rows);
    println!("  {} in CSV (unique)", style(stats.target_ids.len()).cyan());
    if stats.resolved_manually > 0 {
        println!(
            "  {} resolved interactively",
            style(stats.resolved_manually).green()
        );
    }
    println!("  {} to add", style(stats.to_add.len()).green());
    println!("  {} to remove", style(stats.removed.len()).red());
    println!(
        "  {} order change{}",
        style(if stats.reorder_needed { "yes" } else { "no" }).yellow(),
        if stats.create_playlist {
            " (playlist will be created)"
        } else {
            ""
        }
    );
    if stats.duplicates > 0 {
        println!("  {} duplicate link(s) ignored", style(stats.duplicates).dim());
    }
    if !stats.unresolved.is_empty() {
        println!();
        println!("{} unresolved rows (no link):", style("✗").red());
        for q in &stats.unresolved {
            println!("    {}", q);
        }
    }
    if !stats.removed.is_empty() {
        println!();
        println!("{} not in CSV (will be removed):", style("−").red());
        for id in &stats.removed {
            println!("    https://open.spotify.com/track/{}", id);
        }
    }
}

// ---------------------------------------------------------------------------
// Export: playlist → CSV
// ---------------------------------------------------------------------------

/// Exports an existing Spotify playlist to CSV, in playlist order, with the
/// header `Artist,Album,Title,link`. The output is directly consumable by
/// `import-csv` (link column = D, or `--column 4` / `--column link`).
pub async fn export_playlist_to_csv(
    spotify: &SpotifyClient,
    playlist_name: &str,
    output: &str,
    links_only: bool,
) -> Result<usize> {
    let id = spotify
        .playlist_id(playlist_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Playlist '{}' not found", playlist_name))?;
    let tracks = spotify.playlist_tracks(&id).await?;

    let file = std::fs::File::create(output)
        .with_context(|| format!("Failed to create output file '{output}'"))?;
    let mut w = csv::WriterBuilder::new().from_writer(file);

    if !links_only {
        w.write_record(["Artist", "Album", "Title", "link"])
            .context("Failed to write CSV header")?;
    }

    let mut count = 0usize;
    for t in &tracks {
        let Some(link_id) = &t.id else {
            println!(
                "{} {} - {} has no Spotify ID (local file?) — skipped",
                style("Skipped:").yellow().bold(),
                t.title,
                t.artist
            );
            continue;
        };
        if links_only {
            w.write_record([format!(
                "https://open.spotify.com/track/{}",
                link_id
            )])
            .context("Failed to write CSV row")?;
        } else {
            w.write_record([
                t.artist.as_str(),
                t.album.as_deref().unwrap_or(""),
                t.title.as_str(),
                format!("https://open.spotify.com/track/{}", link_id).as_str(),
            ])
            .context("Failed to write CSV row")?;
        }
        count += 1;
    }
    w.flush().context("Failed to write CSV file")?;

    println!(
        "{} {} ({} tracks){}",
        style("Playlist:").bold(),
        playlist_name,
        count,
        if links_only { " [links only]" } else { "" }
    );
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL_A: &str = "https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT";
    const URL_B: &str = "https://open.spotify.com/track/5mg0SOxAc2Fbp5PaOlWYW9";

    fn links(rows: &[CsvRow]) -> Vec<Option<String>> {
        rows.iter().map(|r| r.link.clone()).collect()
    }

    #[test]
    fn parses_links_one_per_line() {
        // Third row: empty link cell, only a title (resolvable interactively).
        let csv = format!("{URL_A}\n{URL_B}\n,\"Some Song\",\"Some Artist\"");
        let rows = parse_csv(&csv, None, "test.csv").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].link.as_deref(), Some("4cOdK2wGLETKBW3PvgPWqT"));
        assert_eq!(rows[1].link.as_deref(), Some("5mg0SOxAc2Fbp5PaOlWYW9"));
        assert_eq!(rows[2].link, None);
        assert_eq!(rows[2].query, "Some Song Some Artist");
    }

    #[test]
    fn skips_header_line() {
        let csv = format!("url\n{URL_A}");
        let rows = parse_csv(&csv, None, "test.csv").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].link.as_deref(), Some("4cOdK2wGLETKBW3PvgPWqT"));
    }

    #[test]
    fn column_by_letter() {
        let csv = format!("title,url\nSong A,{URL_A}\nSong B,{URL_B}");
        let rows = parse_csv(&csv, Some("B"), "test.csv").unwrap();
        assert_eq!(links(&rows)[0].as_deref(), Some("4cOdK2wGLETKBW3PvgPWqT"));
    }

    #[test]
    fn column_by_header_name() {
        let csv = format!("title,link\nSong A,{URL_A}\nSong B,{URL_B}");
        let rows = parse_csv(&csv, Some("link"), "test.csv").unwrap();
        assert_eq!(links(&rows)[0].as_deref(), Some("4cOdK2wGLETKBW3PvgPWqT"));
    }

    #[test]
    fn column_by_index() {
        let csv = format!("title,url\nSong A,{URL_A}");
        let rows = parse_csv(&csv, Some("2"), "test.csv").unwrap();
        assert_eq!(links(&rows)[0].as_deref(), Some("4cOdK2wGLETKBW3PvgPWqT"));
    }

    #[test]
    fn column_by_name_without_header_fails() {
        let csv = format!("{URL_A}");
        assert!(parse_csv(&csv, Some("url"), "test.csv").is_err());
    }

    #[test]
    fn non_link_in_link_column_stops() {
        let csv = format!("{URL_A}\nSong B\n{URL_B}");
        let err = parse_csv(&csv, None, "test.csv").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("line 2"), "message: {msg}");
        assert!(msg.contains("Song B"), "message: {msg}");
        assert!(msg.contains("--column"), "message: {msg}");
    }

    #[test]
    fn empty_link_cell_becomes_unresolved_row_with_query() {
        // Empty link cell (first column) → row kept with the other cells as
        // query (resolvable via --interactive).
        let csv = format!("{URL_A}\n,\"Some Song\",\"Some Artist\"\n{URL_B}");
        let rows = parse_csv(&csv, None, "test.csv").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].link, None);
        assert_eq!(rows[1].query, "Some Song Some Artist");
    }
}
