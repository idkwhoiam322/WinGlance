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
    pub artwork: Option<Vec<u8>>,
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
    /// Compact secondary info line: album · duration · track n/c · genre.
    /// Only the parts the app actually provided are included.
    pub fn meta_line(&self, include_album: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        if include_album && !self.album.trim().is_empty() {
            parts.push(self.album.clone());
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
    PlaybackStateChanged(PlaybackState),
    /// The same track restarted (Prev button, or a looping track): re-show the
    /// track pill briefly instead of deduplicating it away.
    TrackRestarted(TrackInfo),
    /// A playback-state change from a session that is not current. Recorded in
    /// the history (with the source app label); never shown in the pill.
    HistoryPlaybackState(PlaybackState, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_states_are_compared_by_value() {
        assert_eq!(PlaybackState::Playing, PlaybackState::Playing);
        assert_ne!(PlaybackState::Playing, PlaybackState::Paused);
    }
}
