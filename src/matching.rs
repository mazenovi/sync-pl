use crate::deezer::DeezerTrack;
use crate::spotify::SpotifyTrack;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

impl Matchable for DeezerTrack {
    fn match_title(&self) -> &str {
        &self.title
    }
    fn match_artist(&self) -> &str {
        &self.artist
    }
    fn match_duration_s(&self) -> i64 {
        self.duration
    }
    fn match_id(&self) -> &str {
        &self.id
    }
    fn match_url(&self) -> String {
        self.url()
    }
    fn match_album(&self) -> &str {
        self.album.as_deref().unwrap_or("")
    }
}

impl Matchable for SpotifyTrack {
    fn match_title(&self) -> &str {
        &self.title
    }
    fn match_artist(&self) -> &str {
        &self.artist
    }
    fn match_duration_s(&self) -> i64 {
        self.duration_ms / 1000
    }
    fn match_id(&self) -> &str {
        self.id.as_deref().unwrap_or("")
    }
    fn match_url(&self) -> String {
        match &self.id {
            Some(id) => format!("https://open.spotify.com/track/{}", id),
            None => String::new(),
        }
    }
    fn match_album(&self) -> &str {
        self.album.as_deref().unwrap_or("")
    }
}

pub const MATCH_THRESHOLD: f64 = 0.68;
pub const AUTO_THRESHOLD: f64 = 0.8;
const DURATION_TOLERANCE_S: i64 = 3;

#[derive(Debug, Clone)]
pub struct TrackMatch {
    pub cand_id: String,
    pub cand_title: String,
    pub cand_artist: String,
    pub score: f64,
    #[allow(dead_code)]
    pub duration_delta_s: i64,
}

/// Anything that can take part in title/artist/duration matching, whichever
/// side of the Spotify/Deezer boundary it comes from.
pub trait Matchable {
    fn match_title(&self) -> &str;
    fn match_artist(&self) -> &str;
    fn match_duration_s(&self) -> i64;
    fn match_id(&self) -> &str;
    fn match_url(&self) -> String;
    /// Album title when known; an empty string means "unknown" and
    /// contributes nothing to the score.
    fn match_album(&self) -> &str {
        ""
    }
}

pub fn normalize(s: &str) -> String {
    let mut out = String::new();
    let lower = s.to_lowercase();
    let nfd = lower.nfd();
    for ch in nfd {
        if is_combining_mark(ch) {
            continue;
        }
        out.push(ch);
    }
    out
}

fn strip_noise(s: &str) -> String {
    let normalized = normalize(s);

    let mut out = String::new();
    let mut paren_depth = 0i32;
    let mut bracket_depth = 0i32;

    for ch in normalized.chars() {
        match ch {
            '(' | '[' => {
                paren_depth += 1;
                bracket_depth += 1;
            }
            ')' | ']' => {
                paren_depth = (paren_depth - 1).max(0);
                bracket_depth = (bracket_depth - 1).max(0);
            }
            _ => {
                if paren_depth == 0 && bracket_depth == 0 {
                    out.push(ch);
                }
            }
        }
    }

    out
}

fn split_on_collaborators(s: &str) -> String {
    let mut out = s.to_string();
    for marker in [
        " ft ",
        "ft.",
        " feat ",
        "feat.",
        "featuring ",
        " duet with ",
        " featuring ",
    ] {
        if let Some(idx) = out.find(marker) {
            out = out[..idx].to_string();
            break;
        }
    }
    out
}

fn trim_suffixes(s: &str) -> String {
    let mut out = s.to_string();
    let lowercase_markers = [
        "remastered",
        "remaster",
        "remastered 2021",
        "remastered 2023",
        "single version",
        "radio edit",
        "album version",
        "demo",
        "bonus track",
    ];
    for marker in lowercase_markers {
        if let Some(idx) = out.find(marker) {
            out = out[..idx].to_string();
            break;
        }
    }
    out
}

/// Best-scoring candidate regardless of the match threshold (hard rejects on
/// artist and duration still apply).
pub fn scored_best<Q: Matchable, C: Matchable>(query: &Q, candidates: &[C]) -> Option<TrackMatch> {
    let mut best: Option<TrackMatch> = None;

    for candidate in candidates {
        let title_score = string_similarity(candidate.match_title(), query.match_title())
            .max(loose_title_score(candidate.match_title(), query.match_title()));
        let artist_score = cross_artist_score(query.match_artist(), candidate.match_artist());

        // Hard reject: artist is clearly wrong
        if artist_score < 0.45 {
            continue;
        }

        let duration_delta = (candidate.match_duration_s() - query.match_duration_s()).abs();

        // Hard reject: durations diverge too much for likely the same recording
        if duration_delta > 30 && query.match_duration_s() > 0 && candidate.match_duration_s() > 30 {
            continue;
        }

        let duration_factor = if duration_delta <= DURATION_TOLERANCE_S {
            1.0
        } else if duration_delta <= 10 {
            0.5
        } else {
            0.0
        };

        let score = 0.6 * title_score + 0.35 * artist_score + 0.05 * duration_factor;

        // Album agreement is a one-way bonus (never a penalty): it helps
        // disambiguate candidates from the same title/artist but different
        // releases (compilations, live albums, single-only releases). Bounded
        // so the historical thresholds keep their meaning.
        let album_score = if query.match_album().is_empty() || candidate.match_album().is_empty() {
            0.0
        } else {
            string_similarity(candidate.match_album(), query.match_album())
        };
        let score = (score + 0.05 * album_score).min(1.0);

        if best.as_ref().is_none_or(|b| score > b.score) {
            best = Some(TrackMatch {
                cand_id: candidate.match_id().to_string(),
                cand_title: candidate.match_title().to_string(),
                cand_artist: candidate.match_artist().to_string(),
                score,
                duration_delta_s: duration_delta,
            });
        }
    }

    best
}

pub fn string_similarity(a: &str, b: &str) -> f64 {
    let a_norm = strip_noise(a);
    let a_norm = split_on_collaborators(&a_norm);
    let a_norm = trim_suffixes(&a_norm);
    let a_norm = collapse_whitespace(&a_norm);

    let b_norm = strip_noise(b);
    let b_norm = split_on_collaborators(&b_norm);
    let b_norm = trim_suffixes(&b_norm);
    let b_norm = collapse_whitespace(&b_norm);

    if a_norm.is_empty() || b_norm.is_empty() {
        return 0.0;
    }
    if a_norm == b_norm {
        return 1.0;
    }
    if a_norm.contains(&b_norm) || b_norm.contains(&a_norm) {
        let shorter = a_norm.len().min(b_norm.len()) as f64;
        let longer = a_norm.len().max(b_norm.len()) as f64;
        return 0.9 * (0.9 * shorter / longer + 0.1);
    }

    let distance = levenshtein(&a_norm, &b_norm);
    let max_len = a_norm.chars().count().max(b_norm.chars().count()) as f64;
    if max_len == 0.0 {
        return 0.0;
    }
    1.0 - (distance as f64 / max_len)
}

fn loose_title_score(a: &str, b: &str) -> f64 {
    fn cut(s: &str) -> &str {
        s.split(" - ").next().unwrap_or(s)
    }
    string_similarity(cut(a), cut(b))
}

/// Artist similarity allowing for multi-artist lists on both sides: the best
/// pairwise score across comma-separated names wins.
fn cross_artist_score(a_artists: &str, b_artists: &str) -> f64 {
    fn tokens(s: &str) -> Vec<&str> {
        s.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()).collect()
    }
    let a = tokens(a_artists);
    let b = tokens(b_artists);
    if a.is_empty() || b.is_empty() {
        return string_similarity(a_artists, b_artists);
    }
    let mut best = 0.0_f64;
    for x in &a {
        for y in &b {
            best = best.max(string_similarity(x, y));
        }
    }
    best
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    out.trim().to_string()
}

pub fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr: Vec<usize> = vec![0; b_len + 1];

    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let substitution_cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1)
                .min(curr[j - 1] + 1)
                .min(prev[j - 1] + substitution_cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spotify(title: &str, artist: &str, dur_s: i64) -> SpotifyTrack {
        SpotifyTrack {
            id: None,
            title: title.to_string(),
            artist: artist.to_string(),
            duration_ms: dur_s * 1000,
            isrc: None,
            album: None,
        }
    }

    fn deezer(id: i64, title: &str, artist: &str, dur_s: i64) -> DeezerTrack {
        DeezerTrack {
            id: id.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            duration: dur_s,
            isrc: None,
            album: None,
        }
    }

    fn match_track(spotify: &SpotifyTrack, candidates: &[DeezerTrack]) -> Option<TrackMatch> {
        scored_best(spotify, candidates).filter(|m| m.score >= MATCH_THRESHOLD)
    }

    #[test]
    fn exact_match() {
        assert!(string_similarity("Bohemian Rhapsody", "Bohemian Rhapsody") == 1.0);
    }

    #[test]
    fn accent_insensitive() {
        assert!(string_similarity("Les Champs-Élysées", "Les Champs-Elysees") > 0.9);
    }

    #[test]
    fn strips_parentheticals() {
        let s = spotify("Shape of You", "Ed Sheeran", 234);
        let d = vec![deezer(1, "Shape of You (Radio Edit)", "Ed Sheeran", 231)];
        let m = match_track(&s, &d);
        assert!(m.is_some());
    }

    #[test]
    fn strips_feat() {
        assert!(string_similarity("Montero (Call Me By Your Name)", "Montero") > 0.9);
    }

    #[test]
    fn rejects_wrong_artist() {
        let s = spotify("Smooth Criminal", "Michael Jackson", 293);
        let d = vec![deezer(1, "Smooth Criminal", "Alien Ant Farm", 293)];
        let m = match_track(&s, &d);
        assert!(m.is_none());
    }

    #[test]
    fn respects_duration_tolerance() {
        let s = spotify("Take On Me", "a-ha", 228);
        let d = vec![deezer(1, "Take On Me", "a-ha", 45)];
        let m = match_track(&s, &d);
        assert!(m.is_none());
    }

    #[test]
    fn remastered_suffix() {
        assert!(string_similarity("Thriller (Remastered)", "Thriller") > 0.9);
    }

    #[test]
    fn empty_handling() {
        assert_eq!(string_similarity("", "x"), 0.0);
        assert_eq!(string_similarity("x", "x"), 1.0);
    }

    #[test]
    fn dash_qualifier_title() {
        let s = spotify("Root Down (And Get It) - Live", "Jimmy Smith", 500);
        let d = vec![deezer(1, "Root Down (And Get It) (Live)", "Jimmy Smith", 500)];
        assert!(match_track(&s, &d).is_some());
    }

    #[test]
    fn multi_artist_collab() {
        let s = spotify("The King and I", "Eminem, CeeLo Green", 250);
        let d = vec![deezer(1, "The King and I (feat. CeeLo Green)", "Eminem", 250)];
        assert!(match_track(&s, &d).is_some());
    }

    #[test]
    fn album_bonus_boosts_same_album_candidate() {
        // Deliberately imperfect title so the base score sits below the cap
        // and the album bonus is visible.
        let mut q = spotify("One More Tyme", "Daft Punk", 320);
        q.album = Some("Discovery".to_string());

        let mut with = deezer(1, "One More Time", "Daft Punk", 320);
        with.album = Some("Discovery".to_string());
        let mut other = deezer(2, "One More Time", "Daft Punk", 320);
        other.album = Some("Musique Vol. 1 (1993-2005)".to_string());
        let without = deezer(3, "One More Time", "Daft Punk", 320);

        let with_score = scored_best(&q, &[with]).unwrap().score;
        let without_score = scored_best(&q, &[without]).unwrap().score;
        let other_score = scored_best(&q, &[other]).unwrap().score;

        assert!(
            with_score > without_score,
            "matching album should beat unknown album"
        );
        assert!(
            with_score > other_score,
            "matching album should beat a different album"
        );
    }

    #[test]
    fn album_bonus_does_not_apply_when_album_unknown() {
        let q = spotify("One More Time", "Daft Punk", 320);
        let mut with_album = deezer(1, "One More Time", "Daft Punk", 320);
        with_album.album = Some("Discovery".to_string());
        let without = deezer(2, "One More Time", "Daft Punk", 320);

        let s1 = scored_best(&q, &[with_album]).unwrap().score;
        let s2 = scored_best(&q, &[without]).unwrap().score;
        assert!((s1 - s2).abs() < f64::EPSILON);
    }
}