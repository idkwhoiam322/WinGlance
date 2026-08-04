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
        if let Some(d) = self.duration_secs {
            parts.push(format!("{}:{:02}", d / 60, d % 60));
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
}
