/// Heuristic: is this HTTP-level error transient (worth retrying)? Covers
/// both the rspotify ("status code 5xx") and reqwest ("server error") message
/// shapes, plus common network hiccups.
pub(crate) fn is_transient_http(msg: &str) -> bool {
    let s = msg.to_lowercase();
    (500..600).any(|c| s.contains(&format!("status code {c}")))
        || s.contains("status code 429")
        || s.contains("server error")
        || s.contains("bad gateway")
        || s.contains("timed out")
        || s.contains("connection reset")
        || s.contains("connection refused")
}
