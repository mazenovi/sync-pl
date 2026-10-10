# sync-pl

Synchronize your Spotify playlists into Deezer — and back.

`sync-pl` lists the playlists of your (paid) Spotify account, matches every
track against Deezer's catalog, and mirrors them into playlists with the same
name on your Deezer account — which works even with a free account (Deezer's
official APIs are dead; this uses the same internal `gw-light.php` gateway
the web app uses, authenticated with an ARL cookie). The `import` command
does the reverse: it brings the tracks of a Deezer playlist into the
same-named Spotify playlist.

## How it works

- **Spotify** (official API, via `rspotify`): OAuth Authorization Code flow
  with your own Developer application.
- **Deezer** (unofficial `gw-light.php` gateway): authenticated with an `arl`
  cookie, the token saved after you log into deezer.com in your browser.
  Session + CSRF tokens are handled transparently, with automatic refresh on
  token expiry.
- **Matching**: candidates come from Deezer's internal `search.music` gateway
  (25 results, embedding each candidate's ISRC and album directly — no extra
  API calls). Titles + artist names are normalized (lowercase, accents
  stripped, parentheticals and "Remastered"/"feat." suffixes removed, feat.
  collaborators ignored). Each Spotify track is scored against the search
  results (0.6 × title similarity + 0.35 × artist similarity + 0.05 ×
  duration agreement, plus a bounded 0.05 album-agreement bonus). An exact
  ISRC match (the universal recording identifier) on any candidate wins
  outright with score 1.0, ahead of any fuzzy score. Matches scoring ≥ 0.8
  are auto-added; anything below 0.8 or not found is reported for manual
  review.
- Batches of 50 tracks are appended per API call; the Deezer client retries
  once after a CSRF-token refresh.
- **Cover**: after a successful sync the Spotify playlist cover is copied
  onto the Deezer playlist (pass `--no-cover` to disable). Deezer has no
  public API for covers, so this uses the same private flow as the Deezer
  web player: upload via `upload.deezer.com` authenticated with a JWT
  obtained by exchanging the ARL token, then the `updatePlaylist` GraphQL
  mutation on `pipe.deezer.com`. Because this flow is undocumented it can
  break without notice — failures only print a warning and never fail the
  sync.

## Manual review mode

With `--interactive`, tracks that cannot be matched confidently (score < 0.8
or not found) pause the sync and prompt you:

```
Pick 1-5, URL, Enter = skip this time, s = skip forever, q = skip all:
```

- `1..5` — pick one of the listed candidates
- paste a Deezer track URL (`https://www.deezer.com/track/ID`) or a bare ID —
  it is validated against the public API before being added
- `Enter` — skip once; you will be asked again on the next sync
- `s` — skip forever: the decision is stored in
  `~/.config/sync-pl/track_decisions.json` and the track is never prompted
  again in any playlist (delete its entry from that file to undo)
- `q` — skip every remaining prompt of this run

Manual picks (`1..5` and URLs) are also remembered, so a track you resolved by
hand is not asked about again.

## Installation

Requires a Rust toolchain (edition 2021).

```sh
cargo build --release
# binary at target/release/sync-pl
```

## Setup

### 1. Spotify app

1. Go to <https://developer.spotify.com/dashboard> and create an app.
2. Note the **Client ID** and **Client secret**.
3. In the app settings, add the redirect URI `syncpl://callback`
   (Spotify now rejects plain-HTTP URIs like `http://localhost:...`; a custom
   scheme is accepted and never sends anything over the network).

```sh
sync-pl config spotify --client-id YOUR_ID --client-secret YOUR_SECRET
```

The first command that needs Spotify prints an authorization URL. Open it in
your browser, click **Agree**; the browser will then fail to open
`syncpl://callback` — that is expected. Copy the full URL from the address
bar (it contains `?code=...`) and paste it back into the terminal. The token
is cached in `~/.config/sync-pl/spotify_token_cache.json` and refreshed
automatically afterwards; you only paste a code again when the cache is gone
or expired beyond refresh.

### 2. Deezer ARL token

1. Log into <https://www.deezer.com> in your browser (a free account works).
2. Open DevTools (F12) → **Application** → **Cookies** →
   `https://www.deezer.com`, copy the value of the `arl` cookie.

```sh
sync-pl config deezer --arl YOUR_ARL_COOKIE
```

The ARL token validates on save, so an invalid/expired one is rejected right
away. Note it expires sooner or later; when a sync fails with an auth error,
refresh it the same way.

## Usage

```sh
sync-pl list                          # list Spotify playlists
sync-pl show "My Playlist"            # show its tracks
sync-pl sync "My Playlist"            # mirror one playlist into Deezer
sync-pl sync "My Playlist" --dry-run  # preview without touching anything
sync-pl sync "My Playlist" --interactive  # review unmatched tracks by hand
sync-pl sync "My Playlist" --no-cover # skip copying the playlist cover
sync-pl diff "My Playlist"            # compare Spotify vs its Deezer playlist
sync-pl import "My Playlist"          # import a Deezer playlist into Spotify
sync-pl import "My Playlist" --interactive
sync-pl sync-all                      # mirror every Spotify playlist
sync-pl sync-all --dry-run
sync-pl config show                   # print current configuration
```

`import` works in the opposite direction: it reads a Deezer playlist and adds
its tracks to the Spotify playlist with the same name (created if missing).
It uses the same matching, the same interactive prompts (you can paste
`open.spotify.com/track/...` URLs) and the same decisions file — import
decisions are stored under `dz:` keys so they never interfere with sync ones.
Because it writes to Spotify, it needs extra OAuth scopes: the first `import`
asks you to authorize the app again (same redirect flow, once per token
cache).

Mirror playlists on Deezer reuse the same name as the Spotify playlist: if a
Deezer playlist with that exact name exists it is reused (tracks already
present are skipped), otherwise it is created.

## Configuration file

Stored in `~/.config/sync-pl/config.toml`. Spotify tokens are cached in
`~/.config/sync-pl/spotify_token_cache.json`.