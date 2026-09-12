from pathlib import Path

p = Path("src/overlay/render.rs")
text = p.read_text(encoding="utf-8")

old = '''            let (source_icon, fallback_playback, fallback_type) = match content {
                MediaEvent::TrackChanged(track) => (
                    track.app_icon.as_deref(),
                    playback_state_for_track(track),
                    track.playback_type,
                ),
                MediaEvent::PlaybackStateChanged(playback, source_app) => (
                    if source_app.is_empty() {
                        None
                    } else {
                        state
                            .track_cache
                            .get(source_app)
                            .and_then(|track| track.app_icon.as_deref())
                    },
                    *playback,
                    PlaybackType::Unknown,
                ),
                _ => (None, PlaybackState::NowPlaying, PlaybackType::Unknown),
            };'''
new = '''            let (source_icon, fallback_playback, fallback_type) = no_art_source_identity(state, content);'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"expanded no-art decision: expected 1 match, found {count}")
text = text.replace(old, new, 1)

old_compact = '''        let (source_icon, fallback_playback, fallback_type) = match content {
            MediaEvent::TrackChanged(track) => (
                track.app_icon.as_deref(),
                playback_state_for_track(track),
                track.playback_type,
            ),
            MediaEvent::PlaybackStateChanged(playback, source_app) => (
                if source_app.is_empty() {
                    None
                } else {
                    state
                        .track_cache
                        .get(source_app)
                        .and_then(|track| track.app_icon.as_deref())
                },
                *playback,
                PlaybackType::Unknown,
            ),
            _ => (None, PlaybackState::NowPlaying, PlaybackType::Unknown),
        };'''
new_compact = '''        let (source_icon, fallback_playback, fallback_type) = no_art_source_identity(state, content);'''
count = text.count(old_compact)
if count != 1:
    raise SystemExit(f"compact no-art decision: expected 1 match, found {count}")
text = text.replace(old_compact, new_compact, 1)

marker = '''/// Draws the art tile at (art_x, art_y): the accent halo behind the square,'''
helper = '''/// Returns the already-owned source identity used when album art is absent.
/// Keeping this decision out of the large geometry functions avoids duplicating
/// branches there and borrows the existing icon bytes without cloning them.
fn no_art_source_identity<'a>(
    state: &'a OverlayState,
    content: &'a MediaEvent,
) -> (Option<&'a [u8]>, PlaybackState, PlaybackType) {
    match content {
        MediaEvent::TrackChanged(track) => (
            track.app_icon.as_deref(),
            playback_state_for_track(track),
            track.playback_type,
        ),
        MediaEvent::PlaybackStateChanged(playback, source_app) => (
            if source_app.is_empty() {
                None
            } else {
                state
                    .track_cache
                    .get(source_app)
                    .and_then(|track| track.app_icon.as_deref())
            },
            *playback,
            PlaybackType::Unknown,
        ),
        _ => (None, PlaybackState::NowPlaying, PlaybackType::Unknown),
    }
}

/// Draws the art tile at (art_x, art_y): the accent halo behind the square,'''
count = text.count(marker)
if count != 1:
    raise SystemExit(f"helper insertion marker: expected 1 match, found {count}")
text = text.replace(marker, helper, 1)

p.write_text(text, encoding="utf-8")
