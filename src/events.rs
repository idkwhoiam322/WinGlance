use std::sync::Arc;
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

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
    /// be pure waste on every track change.
    pub artwork: Option<Arc<[u8]>>,
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
                (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a.as_ref() == b.as_ref(),
                (None, Some(_)) | (Some(_), None) => true,
                (None, None) => {
                    self.duration_secs == other.duration_secs
                        || self.duration_secs.is_none()
                        || other.duration_secs.is_none()
                }
            }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone)]
pub enum MediaEvent {
    TrackChanged(TrackInfo),
    PlaybackStateChanged(PlaybackState, String),
    /// A session was seen but is not tracked (rejected by `allowed_sources`
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
