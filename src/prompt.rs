use crate::matching::Matchable;
use crate::spotify::format_duration;
use console::style;
use indicatif::ProgressBar;
use std::io::{self, Write};

pub const PROMPT_CHOICES: usize = 5;

pub enum PromptAction {
    Pick(String),
    Manual(String),
    Skip,
    SkipForever,
    SkipAll,
}

pub enum InteractiveChoice {
    Pick(String),
    Skip,
    SkipForever,
    SkipAll,
}

/// Renders the interactive candidate list and reads the user's raw choice.
/// Resolution of manually pasted input happens in the caller (it is async).
pub fn prompt_candidates<Q: Matchable, C: Matchable>(
    query: &Q,
    options: &[C],
    best_id: Option<&str>,
    pb: &ProgressBar,
) -> PromptAction {
    pb.suspend(|| {
        println!();
        println!(
            "{} {} - {}",
            style("No confident match:").yellow().bold(),
            query.match_title(),
            query.match_artist()
        );
        let shown: Vec<&C> = options.iter().take(PROMPT_CHOICES).collect();
        for (i, c) in shown.iter().enumerate() {
            let hint = if Some(c.match_id()) == best_id {
                style("  ← best score").green().to_string()
            } else {
                String::new()
            };
            println!(
                "  [{}] {} - {} [{}]  {}{}",
                i + 1,
                c.match_title(),
                c.match_artist(),
                format_duration(c.match_duration_s() * 1000),
                style(c.match_url()).dim(),
                hint
            );
        }
        println!(
            "You can also {} if you found the track manually.",
            style("paste a track URL").cyan()
        );
        print!(
            "Pick 1-{}, URL, Enter = skip this time, s = skip forever, q = skip all: ",
            shown.len()
        );
        let _ = io::stdout().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return PromptAction::SkipAll;
        }
        let line = line.trim();
        if line.is_empty() {
            return PromptAction::Skip;
        }
        if line.eq_ignore_ascii_case("q") {
            return PromptAction::SkipAll;
        }
        if line.eq_ignore_ascii_case("s") {
            return PromptAction::SkipForever;
        }
        if let Ok(n) = line.parse::<usize>()
            && n >= 1
            && n <= shown.len()
        {
            return PromptAction::Pick(shown[n - 1].match_id().to_string());
        }
        PromptAction::Manual(line.to_string())
    })
}
