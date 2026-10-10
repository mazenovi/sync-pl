//! Reads session cookies from the local Firefox profile (cookies.sqlite).
//!
//! Rule: when a cookie is needed but missing or expired, stop and ask the
//! user to refresh their browser session — never fall back silently.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

/// A cookie read from a Firefox profile.
pub struct FirefoxCookie {
    #[allow(dead_code)]
    pub name: String,
    pub value: String,
    /// Expiry in epoch milliseconds, `None` for session cookies.
    pub expiry_ms: Option<i64>,
}

impl FirefoxCookie {
    pub fn is_expired(&self) -> bool {
        match self.expiry_ms {
            // Session cookies (no expiry column) die with the browser: treat
            // them as missing rather than trusting them blind.
            None => true,
            Some(ms) => ms <= now_ms(),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Finds the default Firefox profile directory (profiles.ini → install
/// section `Default=`, falling back to the first `*.default-release`).
pub fn default_profile_dir() -> Result<PathBuf> {
    let root = firefox_root()?;
    let ini = std::fs::read_to_string(root.join("profiles.ini"))
        .context("Firefox profiles.ini not found (is Firefox installed?)")?;

    // Preferred: the Default= of an [InstallXXXX] section (the actually
    // launched profile, not the legacy "default" one).
    let mut install_default: Option<String> = None;
    let mut first_profile: Option<String> = None;
    let mut section_is_install = false;
    for line in ini.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section_is_install = line.starts_with("[Install");
            continue;
        }
        if let Some(v) = line.strip_prefix("Default=")
            && section_is_install
            && install_default.is_none()
        {
            install_default = Some(v.trim().to_string());
        }
        if let Some(v) = line.strip_prefix("Path=")
            && first_profile.is_none()
        {
            first_profile = Some(v.trim().to_string());
        }
    }
    let relative = install_default.or(first_profile).ok_or_else(|| {
        anyhow::anyhow!("No Firefox profile found in {}", root.display())
    })?;
    let dir = root.join(relative);
    if !dir.is_dir() {
        anyhow::bail!("Firefox profile directory '{}' does not exist", dir.display());
    }
    Ok(dir)
}

/// Reads a cookie (latest expiration first) from the default Firefox profile.
/// Stops with an explicit request when missing or expired.
pub fn read_cookie(name: &str, host_suffix: &str) -> Result<FirefoxCookie> {
    let profile = default_profile_dir()?;
    let db = profile.join("cookies.sqlite");
    if !db.exists() {
        bail!(
            "Firefox cookie database not found at {}.\n\
             Log in to the site in Firefox, then retry.",
            db.display()
        );
    }
    // Copy to a temp file: Firefox locks the live database.
    let tmp = std::env::temp_dir().join(format!(
        "sync-pl-cookies-{}.sqlite",
        std::process::id()
    ));
    std::fs::copy(&db, &tmp).with_context(|| format!("Failed to copy {}", db.display()))?;
    let result = load_cookie(&tmp, name, host_suffix);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn load_cookie(db: &std::path::Path, name: &str, host_suffix: &str) -> Result<FirefoxCookie> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .with_context(|| format!("Failed to open {}", db.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT value, expiry, lastAccessed FROM moz_cookies \
             WHERE name = ?1 AND host LIKE '%' || ?2 \
             ORDER BY expiry IS NULL, expiry DESC",
        )
        .with_context(|| format!("No moz_cookies table in {} (Firefox schema changed?)", db.display()))?;
    let rows = stmt
        .query_map([name, host_suffix], |row| {
            Ok(FirefoxCookie {
                value: row.get::<_, String>(0)?,
                expiry_ms: row.get::<_, Option<i64>>(1)?.map(|s| s * 1000),
                name: name.to_string(),
            })
        })
        .context("Failed to query Firefox cookies")?
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to read Firefox cookies")?;

    let Some(cookie) = rows.into_iter().next() else {
        bail!(
            "Cookie '{name}' not found in Firefox (host *{host_suffix}).\n\
             Log in to the site in Firefox, then retry."
        );
    };
    if cookie.is_expired() {
        bail!(
            "Cookie '{name}' found in Firefox but expired.\n\
             Log in again to the site in Firefox, then retry."
        );
    }
    Ok(cookie)
}

fn firefox_root() -> Result<PathBuf> {
    let mut root = dirs::home_dir().context("Could not determine home directory")?;
    root.push(".mozilla");
    root.push("firefox");
    if !root.is_dir() {
        bail!("Firefox profile root {} does not exist", root.display());
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_cookie_detected() {
        let c = FirefoxCookie {
            name: "x".into(),
            value: "v".into(),
            expiry_ms: Some(1_000),
        };
        assert!(c.is_expired());
        let c = FirefoxCookie {
            name: "x".into(),
            value: "v".into(),
            expiry_ms: None,
        };
        // Session cookie: unusable for our purposes.
        assert!(c.is_expired());
    }
}
