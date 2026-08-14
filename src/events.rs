use std::sync::Arc;
use std::time::Instant;
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

use crate::palette::Palette;

/// Posted by the event forwarder to wake the main window and the overlay
/// when SMTC events arrive. Both windows keep their own queue and drain it
/// on this message.
pub const MEDIA_EVENT_MSG: u32 = WM_APP + 1;
/// Posted by the main window's tray menu to toggle overlay notifications.
pub const TOGGLE_MSG: u32 = WM_APP + 3;
/// Posted by the positioner to the main window with the chosen custom position
/// (X in wParam, Y in lParam, logical pixels). Routing through the main window
/// keeps a single owner of the in-memory config, so a position commit can never
/// clobber a concurrent settings change with a stale disk reload.
pub const POSITION_MSG: u32 = WM_APP + 5;
/// Same contract as `POSITION_MSG`, but for the independent Compact position
/// (`compact_position_x`/`compact_position_y`): posted by the compact-mode
/// positioner, handled by the same single-owner rule.
pub const COMPACT_POSITION_MSG: u32 = WM_APP + 9;

#[derive(Debug, Clone, Default)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Album artist, when provided by the source. Used as a fallback when the
    /// album title is empty (some apps populate one but not the other).
    pub album_artist: String,
    /// Subtitle (e.g. a podcast episode name or video title), when provided.
    pub subtitle: String,
    /// Raw artwork bytes (JPEG/PNG) behind an Arc: events are cloned into two
    /// window queues, and history clones get stripped, so the byte copy would
    /// be pure waste on every track change. Also the identity used for
    /// same-media comparisons and cache keying.
    pub artwork: Option<Arc<[u8]>>,
    /// The artwork decoded once by the SMTC worker into premultiplied BGRA
    /// (the layout AlphaBlend/StretchDIBits consume), at the fixed
    /// `ARTWORK_DECODE`² size — both windows derive the side from the buffer
    /// length, so any size works. Neither window ever runs the image decode
    /// on its UI thread. The raw bytes stay attached (above) for identity and
    /// fingerprinting.
    pub decoded_art: Option<Arc<[u8]>>,
    /// The two-color palette derived once per track identity (source + title +
    /// artist) by the SMTC worker at emit time, from the fixed-size decode.
    /// Cached by identity in the worker, so a source that re-encodes its
    /// thumbnail between reads (different bytes, same cover) can never shift
    /// the pill's accent colors — the UI uses this when present instead of
    /// recomputing from `decoded_art`.
    pub palette: Option<Palette>,
    /// App icon (premultiplied BGRA pixel data) extracted from the source's
    /// AUMID via the shell, cached per-app and shared across track clones.
    pub app_icon: Option<Arc<[u8]>>,
    pub source_app: String,
    /// Total duration in seconds, when the app reports timeline info.
    pub duration_secs: Option<u64>,
    /// 1-based track number within the album, when provided.
    pub track_number: Option<u32>,
    /// Total track count within the album, when provided.
    pub track_count: Option<u32>,
    /// Genre, when provided.
    pub genre: Option<String>,
    /// Playback position in seconds at event time. None when the source does
    /// not report timeline position.
    pub position_secs: Option<f64>,
    /// Playback rate (1.0 = normal). None when not reported.
    pub playback_rate: Option<f64>,
    /// Content type reported by the source (music / video / image). `Image`
    /// pills are suppressed by the worker; the type only changes the glyph on
    /// track-change pills.
    pub playback_type: PlaybackType,
    /// The playback state reported by the source in the same `GetPlaybackInfo`
    /// call that produced this snapshot (Playing / Paused / Stopped). The
    /// authoritative state for the pill: a `TrackChanged` is emitted alongside a
    /// `PlaybackStateChanged` only when the two genuinely differ, so the
    /// snapshot must not default to Playing and swallow a genuine pause/stop.
    /// None means the source reported a transitional status (Opened/Changing) or
    /// the read did not capture a state; callers fall back to the remembered
    /// per-source state, then Playing.
    pub playback_state: Option<PlaybackState>,
    /// Monotonic instant captured by the worker at read time; the UI thread
    /// estimates the live position from it. None when position is unknown.
    pub position_updated_at: Option<Instant>,
}

/// Whether two artwork buffers denote the same cover: the same allocation
/// (shared Arc clones) or byte-identical contents. Strict about presence —
/// art gained or lost on one side is a different identity, which is what
/// lets a recreated session with a genuinely new cover escape dedup. Shared
/// by `same_media` and the overlay's art cache so every site compares
/// covers identically.
pub fn artwork_same(a: Option<&[u8]>, b: Option<&[u8]>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => std::ptr::eq(a.as_ptr(), b.as_ptr()) || a == b,
        (None, None) => true,
        _ => false,
    }
}

impl TrackInfo {
    /// Compact secondary info line: album (or subtitle/album-artist fallback) ·
    /// duration · track n/c · genre. Only the parts the app actually provided
    /// are included. When the album title is empty, the subtitle or album
    /// artist is shown instead so the line still carries useful context.
    pub fn meta_line(&self, include_album: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(d) = self.duration_secs {
            // The stopwatch glyph labels the number as a duration; without it
            // "3:45" reads ambiguously in a line of text.
            parts.push(format!("⏱ {}:{:02}", d / 60, d % 60));
        }
        if include_album {
            let album_line = if !self.album.trim().is_empty() {
                Some(self.album.clone())
            } else if !self.subtitle.trim().is_empty() {
                Some(self.subtitle.clone())
            } else if !self.album_artist.trim().is_empty() {
                Some(self.album_artist.clone())
            } else {
                None
            };
            if let Some(line) = album_line {
                parts.push(line);
            }
        }
        if let (Some(n), Some(c)) = (self.track_number, self.track_count) {
            parts.push(format!("{n}/{c}"));
        }
        if let Some(g) = &self.genre
            && !g.trim().is_empty()
        {
            parts.push(g.clone());
        }
        parts.join(" · ")
    }

    /// Splits the meta line for the overlay: whether a duration is present
    /// (the overlay then draws its own vector clock icon) and the text with
    /// the stopwatch glyph removed, so the emoji never renders through the
    /// GDI text path. The session history keeps `meta_line`'s glyph. The
    /// glyph carries no content, so the stripped text is non-empty exactly
    /// when `meta_line` is.
    pub fn meta_line_for_overlay(&self, include_album: bool) -> (bool, String) {
        let line = self.meta_line(include_album);
        (line.contains('⏱'), line.replace("⏱ ", ""))
    }

    /// Whether `other` denotes the same media item as `self` for the
    /// update-vs-new-pill decision: same source, title and artist, and no
    /// contradicting artwork. A different cover (both sides present)
    /// identifies different media (e.g. a video vs audio version of the same
    /// song); missing artwork on either side — SMTC fills the thumbnail a
    /// moment after the title — is tolerated as the same item so the pill
    /// updates in place instead of re-notifying. With no artwork on either
    /// side, an equal-or-unknown duration is required, so two recordings can
    /// still be told apart.
    pub fn same_media(&self, other: &TrackInfo) -> bool {
        self.source_app == other.source_app
            && self.title == other.title
            && self.artist == other.artist
            && match (&self.artwork, &other.artwork) {
                (Some(_), Some(_)) => artwork_same(self.artwork.as_deref(), other.artwork.as_deref()),
                (None, Some(_)) | (Some(_), None) => true,
                (None, None) => {
                    self.duration_secs == other.duration_secs
                        || self.duration_secs.is_none()
                        || other.duration_secs.is_none()
                }
            }
    }

    /// Merges a later metadata refresh of the same media item into this
    /// entry: every displayed field is updated when the incoming snapshot
    /// carries a value, so a late refresh cannot silently drop fields the
    /// SMTC worker emits (duration, genre, subtitle, album artist, track
    /// position, app icon). Artwork and its decode are the exception: SMTC
    /// reads them only on some passes, so a "no art this pass" refresh must
    /// not clobber the already-queued cover. The worker's own merge inherits
    /// stored values when a read is empty, so "incoming wins when present"
    /// never regresses a field that is already displayed.
    pub fn merge_late_metadata(&mut self, incoming: &TrackInfo) {
        if !incoming.album.trim().is_empty() {
            self.album = incoming.album.clone();
        }
        if !incoming.album_artist.trim().is_empty() {
            self.album_artist = incoming.album_artist.clone();
        }
        if !incoming.subtitle.trim().is_empty() {
            self.subtitle = incoming.subtitle.clone();
        }
        if incoming.artwork.is_some() {
            self.artwork = incoming.artwork.clone();
        }
        if incoming.decoded_art.is_some() {
            self.decoded_art = incoming.decoded_art.clone();
        }
        if incoming.app_icon.is_some() {
            self.app_icon = incoming.app_icon.clone();
        }
        if let Some(duration) = incoming.duration_secs {
            self.duration_secs = Some(duration);
        }
        if let Some(number) = incoming.track_number {
            self.track_number = Some(number);
        }
        if let Some(count) = incoming.track_count {
            self.track_count = Some(count);
        }
        if let Some(genre) = &incoming.genre
            && !genre.trim().is_empty()
        {
            self.genre = Some(genre.clone());
        }
        if incoming.position_secs.is_some() {
            self.position_secs = incoming.position_secs;
        }
        if incoming.playback_rate.is_some() {
            self.playback_rate = incoming.playback_rate;
        }
        if incoming.position_updated_at.is_some() {
            self.position_updated_at = incoming.position_updated_at;
        }
        // The authoritative playback state is read with the rest of the snapshot;
        // a later read that carries it supersedes a previously stored value.
        if incoming.playback_state.is_some() {
            self.playback_state = incoming.playback_state;
        }
    }
}

/// Recovers the owned event from a queue's transport `Arc`: zero-copy when
/// this window is the last holder of the shared allocation (the usual case —
/// each event lives in exactly one window queue), a full clone while the
/// other window still holds its reference.
pub fn media_event_into_owned(event: Arc<MediaEvent>) -> MediaEvent {
    Arc::try_unwrap(event).unwrap_or_else(|shared| (*shared).clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
    /// Overlay-only "a new track is now playing" state, used to draw the
    /// music-note symbol on track-change pills. Never produced by SMTC.
    NowPlaying,
}

/// Content type as reported by SMTC's `PlaybackInfo.PlaybackType`. Rendered
/// only on track-change pills: `Music`/`Unknown` draw the music note, `Video`
/// draws a video-player glyph, `Image` never renders — the worker suppresses
/// image pills entirely. State pills (▶/‖/■) ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackType {
    /// The source did not report a type. Renders like `Music`.
    #[default]
    Unknown,
    Music,
    Video,
    Image,
}

// `TrackChanged` carries the full `TrackInfo` (the other variants are far
// smaller). Boxing it would ripple through every construction and match site
// for no real gain — the heavy fields are already `Arc`-backed, so cloning
// the enum copies only cheap pointers plus the small inline `String`/`f64`s.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum MediaEvent {
    TrackChanged(TrackInfo),
    PlaybackStateChanged(PlaybackState, String),
    /// A session was seen but is not tracked (rejected by `media_sources`
    /// or on the churn cool-down). Carries whatever display info SMTC exposed
    /// at discovery time. The history records it as a muted row so all media
    /// sources are visible; `accepted` marks entries from tracked sessions.
    SessionRejected {
        source_app: String,
        title: String,
        artist: String,
        state: PlaybackState,
        accepted: bool,
    },
    /// The SMTC worker gave up after repeated failures (stall or exit) and
    /// media notifications will not resume until the app restarts. Emitted
    /// once by the supervisor. History-only: the overlay never renders it.
    WorkerFailed {
        reason: String,
    },
    /// Live playback position update, separate from `TrackChanged` so the
    /// progress bar can track position and seeks without re-emitting a track
    /// pill. Carries position, duration and rate; the overlay re-bases its
    /// estimate from it. `source_app` lets the overlay attribute the update to
    /// the session that produced it and only apply it to the content on
    /// screen. Never rendered as a pill and never stored as the active
    /// content — it is a data update only.
    ProgressChanged {
        source_app: String,
        position_secs: Option<f64>,
        duration_secs: Option<u64>,
        playback_rate: Option<f64>,
    },
}

/// Side length the SMTC worker decodes album art to (square), fixed for every
/// display. 256² covers a 96 px logical tile at up to ~266 % DPI; every
/// display blits this buffer downscaled (sharper than an upscale), and beyond
/// ~266 % the windows upscale it slightly (bilinear, not visible on artwork),
/// which is the pre-existing behavior at 300 %+ DPI. A fixed size keeps the
/// decoded buffer — and everything derived from it, notably the palette —
/// byte-identical for the same cover on every display. An adaptive size would
/// make those depend on the foreground window's DPI at emit time and shift
/// the palette's dominant-color pick between pill shows. Memory stays capped
/// at 256 KB per cover.
pub const ARTWORK_DECODE: u32 = 256;

/// Artwork only ever displays at ~200px, so refusing anything larger than
/// this defeats decompression bombs (a header can claim huge dimensions
/// while the compressed payload is tiny) without affecting real album art.
/// 2048² bounds the transient decode to ~16 MB RGBA; real covers are ≤1024²
/// anyway. The cap runs on the SMTC worker (decode happens there, once per
/// emitted track).
const ART_MAX_DIM: u32 = 2048;

/// Decodes artwork bytes with a hard cap on source dimensions. The `image`
/// crate's dimension limits are strict, so an oversized image fails here
/// instead of allocating a huge buffer.
fn decode_limited(data: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(ART_MAX_DIM);
    limits.max_image_height = Some(ART_MAX_DIM);
    reader.limits(limits);
    reader.decode().ok()
}

/// Decodes artwork directly into the premultiplied BGRA layout that
/// StretchDIBits/AlphaBlend consume (top-down 32bpp DIB), so windows can
/// draw the cached bitmap with a single blit instead of re-converting per
/// paint. Runs on the SMTC worker thread, never on a UI thread.
pub(crate) fn decode_artwork_pm(data: &[u8], size: usize) -> Option<Vec<u8>> {
    let image = decode_limited(data)?.to_rgba8();
    let image = image::imageops::resize(&image, size as u32, size as u32, image::imageops::FilterType::Triangle);
    let raw = image.into_raw();
    let mut pm = Vec::with_capacity(raw.len());
    for px in raw.chunks_exact(4) {
        let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        pm.push((b * a / 255) as u8);
        pm.push((g * a / 255) as u8);
        pm.push((r * a / 255) as u8);
        pm.push(a as u8);
    }
    Some(pm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_states_are_compared_by_value() {
        assert_eq!(PlaybackState::Playing, PlaybackState::Playing);
        assert_ne!(PlaybackState::Playing, PlaybackState::Paused);
    }

    #[test]
    fn playback_state_event_carries_source() {
        let event = MediaEvent::PlaybackStateChanged(PlaybackState::Paused, "spotify".into());
        match event {
            MediaEvent::PlaybackStateChanged(_, source) => assert_eq!(source, "spotify"),
            _ => panic!("expected PlaybackStateChanged"),
        }
    }

    #[test]
    fn meta_line_for_overlay_strips_the_duration_glyph() {
        let track = TrackInfo {
            album: "Example".into(),
            duration_secs: Some(225),
            subtitle: "".into(),
            album_artist: "".into(),
            genre: Some("Pop".into()),
            track_number: Some(3),
            track_count: Some(12),
            ..TrackInfo::default()
        };
        let line = track.meta_line(true);
        assert!(line.contains('⏱'), "meta line must carry the duration glyph");
        let (has_duration, text) = track.meta_line_for_overlay(true);
        assert!(has_duration, "a duration must flag the clock icon");
        assert!(!text.contains('⏱'), "the glyph must be removed: {text}");
        assert!(!text.trim().is_empty(), "the rest of the line stays");
        assert!(
            text.contains("3:45") && text.contains("Example") && text.contains("3/12"),
            "the remaining parts are preserved: {text}"
        );
        // Duration must precede album so the clock icon (drawn at the left
        // edge of the overlay row) visually anchors to the duration.
        assert!(
            text.find("3:45").unwrap() < text.find("Example").unwrap(),
            "duration must precede album: {text}"
        );
        // Without a duration the line is left untouched and no icon is drawn.
        let no_duration = TrackInfo {
            album: "Example".into(),
            ..TrackInfo::default()
        };
        let (has_duration, text) = no_duration.meta_line_for_overlay(true);
        assert!(!has_duration);
        assert_eq!(text, no_duration.meta_line(true));
    }

    fn track(title: &str, artist: &str) -> TrackInfo {
        TrackInfo {
            title: title.into(),
            artist: artist.into(),
            source_app: "youtube-music".into(),
            ..TrackInfo::default()
        }
    }

    fn art(bytes: &[u8]) -> Option<Arc<[u8]>> {
        Some(Arc::from(bytes))
    }

    #[test]
    fn same_media_requires_source_title_and_artist() {
        let a = track("Love Me Not", "Ravyn Lenae");
        assert!(a.same_media(&track("Love Me Not", "Ravyn Lenae")));
        assert!(!a.same_media(&track("Other", "Ravyn Lenae")));
        assert!(!a.same_media(&track("Love Me Not", "Other")));
        let other_source = TrackInfo {
            source_app: "spotify".into(),
            ..a.clone()
        };
        assert!(!a.same_media(&other_source));
    }

    #[test]
    fn same_media_tolerates_late_or_lost_artwork() {
        // SMTC fills the thumbnail a moment after the title: gaining art for
        // the same track must stay an in-place update, not a new pill.
        let no_art = track("Love Me Not", "Ravyn Lenae");
        let with_art = TrackInfo {
            artwork: art(b"cover"),
            ..no_art.clone()
        };
        assert!(no_art.same_media(&with_art));
        assert!(with_art.same_media(&no_art));
    }

    #[test]
    fn same_media_distinguishes_different_covers() {
        let a = TrackInfo {
            artwork: art(b"cover-a"),
            ..track("Love Me Not", "Ravyn Lenae")
        };
        let b = TrackInfo {
            artwork: art(b"cover-b"),
            ..track("Love Me Not", "Ravyn Lenae")
        };
        assert!(!a.same_media(&b), "a different cover is different media");
        // Identical bytes in separate Arcs still compare equal.
        let a_copy = TrackInfo {
            artwork: art(b"cover-a"),
            ..track("Love Me Not", "Ravyn Lenae")
        };
        assert!(a.same_media(&a_copy));
    }

    #[test]
    fn same_media_uses_duration_when_both_lack_artwork() {
        let a = track("Love Me Not", "Ravyn Lenae");
        let shorter = TrackInfo {
            duration_secs: Some(115),
            ..a.clone()
        };
        let longer = TrackInfo {
            duration_secs: Some(218),
            ..a.clone()
        };
        assert!(a.same_media(&shorter), "unknown duration matches anything");
        assert!(
            !shorter.same_media(&longer),
            "both known and different -> different media"
        );
    }
}
