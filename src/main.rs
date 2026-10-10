mod config;
mod deezer;
mod firefox_cookies;
mod import;
mod import_csv;
mod matching;
mod net;
mod prompt;
mod search_backend;
mod spotify;
mod spotify_web;
mod store;
mod sync;
mod youtube;
mod youtube_sync;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use console::style;

#[derive(Parser)]
#[command(
    name = "sync-pl",
    version,
    about = "Synchronize Spotify playlists to Deezer",
    long_about = "sync-pl lists your Spotify playlists and mirrors them into your \
Deezer library (works with a free Deezer account via ARL token)."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List all Spotify playlists
    List,
    /// List all your Deezer playlists
    ListDeezer,
    /// Show the tracks of a Spotify playlist
    Show {
        /// Playlist name or part of its name
        playlist: String,
    },
    /// Sync a Spotify playlist into Deezer
    Sync {
        /// Playlist name or part of its name
        playlist: String,
        /// Show what would be done without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Interactively review unmatched tracks (ignored with --dry-run)
        #[arg(long)]
        interactive: bool,
        /// Do not copy the Spotify cover onto the Deezer playlist
        #[arg(long)]
        no_cover: bool,
    },
    /// Sync all your Spotify playlists into Deezer
    SyncAll {
        /// Show what would be done without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Interactively review unmatched tracks (ignored with --dry-run)
        #[arg(long)]
        interactive: bool,
        /// Do not copy the Spotify covers onto the Deezer playlists
        #[arg(long)]
        no_cover: bool,
    },
    /// Compare a Spotify playlist with its same-named Deezer playlist
    Diff {
        /// Playlist name or part of its name
        playlist: String,
    },
    /// Sync a Spotify playlist into a YouTube playlist mirror
    #[command(name = "sync-youtube")]
    SyncYoutube {
        /// Playlist name or part of its name
        playlist: String,
        /// Show what would be done without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Interactively review uncertain matches (ignored with --dry-run)
        #[arg(long)]
        interactive: bool,
    },
    /// Import a Deezer playlist into the same-named Spotify playlist
    #[command(name = "import-deezer")]
    ImportDeezer {
        /// Playlist name or part of its name
        playlist: String,
        /// Show what would be done without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Interactively review unmatched tracks (ignored with --dry-run)
        #[arg(long)]
        interactive: bool,
    },
    /// Import a CSV of Spotify track links into a playlist (one row per track,
    /// order is kept). The CSV holds the truth: the playlist is conformed to it.
    ImportCsv {
        /// CSV file with one track per row
        file: String,
        /// Target Spotify playlist name
        playlist: String,
        /// Link column: letter (A, B...), 1-based index, or header name (default: first column)
        #[arg(long)]
        column: Option<String>,
        /// Target playlist by Spotify ID instead of name — skips me/playlists
        /// entirely (useful when that endpoint is rate-limited)
        #[arg(long)]
        playlist_id: Option<String>,
        /// Show what would be done without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Interactively resolve rows without a link
        #[arg(long)]
        interactive: bool,
    },
    /// Export a Spotify playlist to CSV (Artist,Album,Title,link — in playlist order)
    ExportCsv {
        /// Playlist name or part of its name
        playlist: String,
        /// Output CSV file
        #[arg(short, long)]
        output: String,
        /// Only export the links column
        #[arg(long)]
        links_only: bool,
    },
    /// Check Spotify API availability and rate-limit status
    Check,
    /// Debug: run a raw Deezer gw-light call and print the response
    Debug {
        /// gw method name (e.g. deezer.pageProfile)
        method: String,
        /// JSON body of the call
        #[arg(default_value = "{}")]
        body: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Set Spotify API credentials
    Spotify {
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        client_secret: String,
        /// OAuth redirect URI (must match the one registered in your Spotify app)
        #[arg(long)]
        redirect_uri: Option<String>,
    },
    /// Set the Deezer ARL token (from your browser cookies after login)
    Deezer {
        /// ARL token value; omit --arl to read it automatically from Firefox
        #[arg(long)]
        arl: Option<String>,
    },
    /// Set the Spotify web session cookie (sp_dc) — enables web search fallback
    SpotifyWeb {
        /// sp_dc cookie value; omit to read it automatically from Firefox
        #[arg(long)]
        sp_dc: Option<String>,
    },
    /// Set YouTube API credentials (Google Cloud OAuth client)
    Youtube {
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        client_secret: String,
    },
    /// Print current configuration
    Show,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Config { action } => run_config(action).await,
        Commands::List => run_list().await,
        Commands::ListDeezer => run_list_deezer().await,
        Commands::Show { playlist } => run_show(&playlist).await,
        Commands::Sync {
            playlist,
            dry_run,
            interactive,
            no_cover,
        } => run_sync(&playlist, dry_run, interactive, !no_cover).await,
        Commands::SyncAll {
            dry_run,
            interactive,
            no_cover,
        } => run_sync_all(dry_run, interactive, !no_cover).await,
        Commands::Diff { playlist } => run_diff(&playlist).await,
        Commands::SyncYoutube {
            playlist,
            dry_run,
            interactive,
        } => run_sync_youtube(&playlist, dry_run, interactive).await,
        Commands::ImportDeezer {
            playlist,
            dry_run,
            interactive,
        } => run_import_deezer(&playlist, dry_run, interactive).await,
        Commands::ImportCsv {
            file,
            playlist,
            column,
            playlist_id,
            dry_run,
            interactive,
        } => {
            run_import_csv(
                &file,
                &playlist,
                column.as_deref(),
                playlist_id.as_deref(),
                dry_run,
                interactive,
            )
            .await
        }
        Commands::ExportCsv {
            playlist,
            output,
            links_only,
        } => run_export_csv(&playlist, &output, links_only).await,
        Commands::Check => run_check().await,
        Commands::Debug { method, body } => run_debug(&method, &body).await,
    }
}

async fn load_web_client(config: &Config) -> Option<spotify_web::SpotifyWebClient> {
    config
        .spotify_web
        .as_ref()
        .map(|w| spotify_web::SpotifyWebClient::new(&w.sp_dc))
}

async fn run_import_deezer(playlist: &str, dry_run: bool, interactive: bool) -> Result<()> {
    let config = Config::load()?;
    let (spotify, deezer) = load_clients(&config).await?;
    let mut store = store::TrackStore::load();
    let web = load_web_client(&config).await;

    let stats = import::import_playlist(
        &spotify,
        &deezer,
        &mut store,
        playlist,
        dry_run,
        interactive,
        web.as_ref(),
    )
    .await?;
    import::print_import_report(&stats);
    Ok(())
}

async fn run_import_csv(
    file: &str,
    playlist: &str,
    column: Option<&str>,
    playlist_id: Option<&str>,
    dry_run: bool,
    interactive: bool,
) -> Result<()> {
    let config = Config::load()?;
    let (spotify, _deezer) = load_clients(&config).await?;

    let stats = import_csv::import_csv_playlist(
        &spotify,
        file,
        playlist,
        column,
        playlist_id,
        dry_run,
        interactive,
    )
    .await?;
    import_csv::print_report(&stats);
    Ok(())
}

async fn run_diff(playlist: &str) -> Result<()> {
    let config = Config::load()?;
    let (spotify, deezer) = load_clients(&config).await?;
    let store = store::TrackStore::load();

    let stats = sync::diff_playlist(&spotify, &deezer, &store, playlist).await?;
    sync::print_diff(&stats);
    Ok(())
}

async fn run_sync_youtube(playlist: &str, dry_run: bool, interactive: bool) -> Result<()> {
    let config = Config::load()?;
    let Some(y) = &config.youtube else {
        bail!(
            "YouTube not configured. Run `sync-pl config youtube --client-id ... --client-secret ...`"
        );
    };
    let Some(s) = &config.spotify else {
        bail!("Spotify not configured. Run `sync-pl config spotify --client-id ... --client-secret ...`");
    };
    println!("{} Spotify...", style("Connecting").bold());
    let spotify = spotify::SpotifyClient::new(&s.client_id, &s.client_secret, &s.redirect_uri).await?;
    println!("{} YouTube...", style("Connecting").bold());
    let youtube = youtube::YoutubeClient::new(&y.client_id, &y.client_secret).await?;

    youtube_sync::sync_playlist(&spotify, &youtube, playlist, dry_run, interactive).await
}

async fn run_export_csv(playlist: &str, output: &str, links_only: bool) -> Result<()> {
    let config = Config::load()?;
    let (spotify, _deezer) = load_clients(&config).await?;

    let count = import_csv::export_playlist_to_csv(&spotify, playlist, output, links_only).await?;
    println!(
        "{} exported {} tracks to {}",
        style("OK").green().bold(),
        count,
        output
    );
    Ok(())
}

async fn run_check() -> Result<()> {
    let config = Config::load()?;
    let Some(s) = &config.spotify else {
        bail!("Spotify not configured. Run `sync-pl config spotify --client-id ... --client-secret ...`");
    };
    let spotify = spotify::SpotifyClient::new(&s.client_id, &s.client_secret, &s.redirect_uri).await?;

    println!("{}", style("Spotify API status (per endpoint):").bold());
    let mut blocked = false;
    let mut unreachable = false;

    // (name, probe future) — each probe returns (blocked, retry_after).
    struct Probe {
        name: &'static str,
        detail: &'static str,
    }
    let probes: Vec<Probe> = vec![
        Probe { name: "current_user", detail: "GET /me — auth + general API health" },
        Probe { name: "search", detail: "GET /search — matching (import-deezer, interactive)" },
        Probe { name: "playlists", detail: "GET /me/playlists — playlist listing (large imports)" },
    ];

    let results = futures::future::join_all(probes.iter().map(|p| async {
        match p.name {
            "current_user" => spotify.rate_limit_probe().await,
            "search" => spotify.search_rate_limit_probe().await.map(|(b, w)| (b, w, None)),
            "playlists" => spotify.playlists_rate_limit_probe().await.map(|(b, w)| (b, w, None)),
            _ => unreachable!(),
        }
    }))
    .await;

    for (probe, result) in probes.iter().zip(results) {
        println!("  {} — {}", style(probe.name).cyan(), probe.detail);
        match result {
            Ok((false, _, user)) => {
                println!("    {} available", style("OK").green().bold());
                if let Some(id) = user {
                    println!("    logged in as: {}", style(&id).dim());
                }
            }
            Ok((true, Some(wait), _)) => {
                blocked = true;
                println!(
                    "    {} rate-limited (429) — Retry-After {}s ({})",
                    style("BLOCKED").red().bold(),
                    wait,
                    format_hms(wait)
                );
            }
            Ok((true, None, _)) => {
                blocked = true;
                println!(
                    "    {} rate-limited (429), no Retry-After given",
                    style("BLOCKED").red().bold()
                );
            }
            Err(e) => {
                unreachable = true;
                println!("    {} {}", style("UNREACHABLE").red().bold(), e);
            }
        }
    }

    if blocked {
        std::process::exit(2);
    }
    if unreachable {
        std::process::exit(1);
    }
    Ok(())
}

/// Formats a duration in seconds as "Xh YYm ZZs" (hours omitted when 0).
fn format_hms(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

async fn run_debug(method: &str, body: &str) -> Result<()> {
    let config = Config::load()?;
    let Some(d) = &config.deezer else {
        bail!("Deezer not configured. Run `sync-pl config deezer --arl ...`");
    };
    let client = deezer::DeezerClient::new(d.arl_token.clone()).await?;
    let mut body: serde_json::Value = serde_json::from_str(body).context("Invalid JSON body")?;
    if method == "deezer.pageProfile"
        && body.is_object()
        && body.get("USER_ID").is_none()
        && let Some(uid) = client.user_id()
    {
        body["USER_ID"] = serde_json::json!(uid);
    }
    let result = client.gw_call(method, Some(body)).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn run_config(action: ConfigAction) -> Result<()> {
    let mut config = Config::load()?;
    match action {
        ConfigAction::Spotify {
            client_id,
            client_secret,
            redirect_uri,
        } => {
            if client_id.is_empty() || client_secret.is_empty() {
                bail!("client-id and client-secret are required");
            }
            let redirect_uri = redirect_uri.unwrap_or_else(config::default_redirect_uri);
            config.spotify = Some(config::SpotifyConfig {
                client_id,
                client_secret,
                redirect_uri,
            });
            config.save()?;
            println!("{} Spotify credentials saved.", style("OK").green());
            println!(
                "  Make sure the redirect URI {} is whitelisted in your Spotify app.",
                config.spotify.as_ref().unwrap().redirect_uri
            );
            Ok(())
        }
        ConfigAction::Deezer { arl } => {
            let arl = match arl {
                Some(a) if !a.trim().is_empty() => a.trim().to_string(),
                _ => {
                    println!("{} Reading ARL cookie from Firefox...", style("...").dim());
                    firefox_cookies::read_cookie("arl", ".deezer.com")?
                        .value
                }
            };
            let client = deezer::DeezerClient::new(arl.clone())
                .await
                .context("Failed to authenticate with Deezer using the provided ARL token")?;
            println!(
                "{} ARL token accepted for user {}.",
                style("OK").green(),
                style(client.user_id().as_deref().unwrap_or("?"))
            );
            config.deezer = Some(config::DeezerConfig { arl_token: arl });
            config.save()?;
            println!("{} Deezer ARL token saved.", style("OK").green());
            Ok(())
        }
        ConfigAction::SpotifyWeb { sp_dc } => {
            let sp_dc = match sp_dc {
                Some(v) if !v.trim().is_empty() => v.trim().to_string(),
                _ => {
                    println!("{} Reading sp_dc cookie from Firefox...", style("...").dim());
                    firefox_cookies::read_cookie("sp_dc", ".spotify.com")?
                        .value
                }
            };
            // Validate the cookie by fetching a web token before saving.
            let client = spotify_web::SpotifyWebClient::new(&sp_dc);
            let probe = client.search_track("the beatles").await.context(
                "sp_dc cookie rejected — log in to open.spotify.com in Firefox, then retry",
            );
            match probe {
                Ok(tracks) if !tracks.is_empty() => {
                    println!(
                        "{} sp_dc accepted — web search working ({} result(s) on probe).",
                        style("OK").green(),
                        tracks.len()
                    );
                }
                Ok(_) => {
                    println!(
                        "{} sp_dc accepted — web search reachable (0 result on probe, unusual).",
                        style("OK").yellow()
                    );
                }
                Err(e) => return Err(e),
            }
            config.spotify_web = Some(config::SpotifyWebConfig { sp_dc });
            config.save()?;
            println!("{} Spotify web session saved.", style("OK").green());
            Ok(())
        }
        ConfigAction::Youtube {
            client_id,
            client_secret,
        } => {
            if client_id.is_empty() || client_secret.is_empty() {
                bail!("client-id and client-secret are required");
            }
            config.youtube = Some(config::YoutubeConfig {
                client_id,
                client_secret,
            });
            config.save()?;
            println!("{} YouTube credentials saved.", style("OK").green());
            println!(
                "  They will be validated on first use (OAuth consent in your browser)."
            );
            Ok(())
        }
        ConfigAction::Show => {
            print_config(&config);
            Ok(())
        }
    }
}

fn print_config(config: &Config) {
    if let Some(s) = &config.spotify {
        println!("{}", style("Spotify:").bold());
        println!("  client_id:     {}", mask(&s.client_id));
        println!("  client_secret: {}", mask(&s.client_secret));
        println!("  redirect_uri:  {}", s.redirect_uri);
    } else {
        println!("{}", style("Spotify: (not configured)").dim());
    }
    if let Some(d) = &config.deezer {
        println!("\n{}", style("Deezer:").bold());
        println!("  arl_token: {}", mask(&d.arl_token));
    } else {
        println!("\n{}", style("Deezer: (not configured)").dim());
    }
    if let Some(w) = &config.spotify_web {
        println!("\n{}", style("Spotify web (search fallback):").bold());
        println!("  sp_dc: {}", mask(&w.sp_dc));
    } else {
        println!(
            "\n{}",
            style("Spotify web: (not configured — official API search only)").dim()
        );
    }
    if let Some(y) = &config.youtube {
        println!("\n{}", style("YouTube:").bold());
        println!("  client_id:     {}", mask(&y.client_id));
        println!("  client_secret: {}", mask(&y.client_secret));
    } else {
        println!("\n{}", style("YouTube: (not configured)").dim());
    }
    if config.spotify.is_none() || config.deezer.is_none() {
        println!();
        println!("Run `sync-pl config spotify --client-id ... --client-secret ...`");
        println!("and `sync-pl config deezer --arl ...` to get started.");
    }
}

fn mask(s: &str) -> String {
    if s.len() <= 8 {
        "****".to_string()
    } else {
        format!("{}...{}", &s[..4], &s[s.len() - 4..])
    }
}

async fn load_clients(
    config: &Config,
) -> Result<(spotify::SpotifyClient, deezer::DeezerClient)> {
    let Some(s) = &config.spotify else {
        bail!("Spotify not configured. Run `sync-pl config spotify --client-id ... --client-secret ...`");
    };
    let Some(d) = &config.deezer else {
        bail!("Deezer not configured. Run `sync-pl config deezer --arl ...`");
    };
    let spotify =
        spotify::SpotifyClient::new(&s.client_id, &s.client_secret, &s.redirect_uri).await?;
    let deezer = deezer::DeezerClient::new(d.arl_token.clone()).await?;
    Ok((spotify, deezer))
}

async fn run_list_deezer() -> Result<()> {
    let config = Config::load()?;
    let Some(d) = &config.deezer else {
        bail!("Deezer not configured. Run `sync-pl config deezer --arl ...`");
    };
    let deezer = deezer::DeezerClient::new(d.arl_token.clone()).await?;

    let playlists = deezer.user_playlists().await?;
    if playlists.is_empty() {
        println!("No playlists found.");
        return Ok(());
    }
    println!("{} Deezer playlists:", playlists.len());
    for (i, p) in playlists.iter().enumerate() {
        let tag = if p.owned {
            String::new()
        } else {
            style(" (not yours)").dim().to_string()
        };
        println!("  {:>2}. {}{}", i + 1, style(&p.title).cyan(), tag);
    }
    Ok(())
}

async fn run_list() -> Result<()> {
    let config = Config::load()?;
    let (spotify, _deezer) = load_clients(&config).await?;

    let playlists = spotify.list_playlists().await?;
    if playlists.is_empty() {
        println!("No playlists found.");
        return Ok(());
    }
    println!("{} Spotify playlists:", playlists.len());
    for (i, p) in playlists.iter().enumerate() {
        println!("  {:>2}. {}", i + 1, style(&p.name).cyan());
    }
    Ok(())
}

async fn run_show(playlist: &str) -> Result<()> {
    let config = Config::load()?;
    let (spotify, _deezer) = load_clients(&config).await?;

    let id = spotify
        .playlist_id(playlist)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Playlist '{}' not found", playlist))?;
    let details = spotify.playlist_details(&id).await?;
    if !details.description.is_empty() {
        println!("{}: {}", style("Description").bold(), details.description);
    }
    let tracks = spotify.playlist_tracks(&id).await?;
    println!("{} ({} tracks):", playlist, tracks.len());
    for (i, t) in tracks.iter().enumerate() {
        println!(
            "  {:>3}. {} - {} [{}]",
            i + 1,
            style(&t.title).green(),
            style(&t.artist).dim(),
            spotify::format_duration(t.duration_ms)
        );
    }
    Ok(())
}

async fn run_sync(playlist: &str, dry_run: bool, interactive: bool, set_cover: bool) -> Result<()> {
    let config = Config::load()?;
    let (spotify, deezer) = load_clients(&config).await?;
    let mut store = store::TrackStore::load();

    let stats = sync::sync_playlist(
        &spotify,
        &deezer,
        &mut store,
        playlist,
        dry_run,
        interactive,
        set_cover,
        None,
    )
    .await?;
    sync::print_report(&stats);
    Ok(())
}

async fn run_sync_all(dry_run: bool, interactive: bool, set_cover: bool) -> Result<()> {
    let config = Config::load()?;
    let (spotify, deezer) = load_clients(&config).await?;
    let mut store = store::TrackStore::load();

    let playlists = spotify.list_playlists().await?;
    if playlists.is_empty() {
        println!("No playlists to sync.");
        return Ok(());
    }
    println!(
        "Syncing {} playlists{}",
        playlists.len(),
        if dry_run { " (dry run)" } else { "" }
    );
    for p in &playlists {
        println!("\n--- {} ---", p.name);
        let stats = sync::sync_playlist(
            &spotify,
            &deezer,
            &mut store,
            &p.name,
            dry_run,
            interactive,
            set_cover,
            p.cover_url.as_deref(),
        )
        .await?;
        sync::print_report(&stats);
    }
    Ok(())
}