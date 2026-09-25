//! Internet radio: radio-browser.info directory + icecast stream playback.
//!
//! Contract scaffold: the shared types live here so the three workstreams
//! (`player` — playback core, `directory` — radio-browser client, `art` —
//! choya DJ sprites) plus the tab UI compile independently and merge without
//! overlap. Stream playback runs on a dedicated tokio runtime owned by
//! `player`; directory searches use the addon's normal blocking-reqwest
//! worker pattern.

pub mod art;
#[cfg(test)]
mod decode_tests;
pub mod directory;
pub mod logos;
pub mod player;
pub mod quips;

use std::sync::{Arc, Mutex};

/// One station row from radio-browser.info. Every field serde-defaulted so a
/// sparse directory record never fails the whole response.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct RbStation {
    pub stationuuid: String,
    pub name: String,
    pub url: String,
    pub url_resolved: String,
    pub favicon: String,
    pub tags: String,
    pub countrycode: String,
    pub codec: String,
    pub bitrate: u32,
    /// Directory vote count — kept so "Popular" sorting can restore the
    /// API's own order after a client-side re-sort.
    pub votes: u64,
    pub lastcheckok: u8,
    pub hls: u8,
}

impl RbStation {
    /// The playable URL: `url_resolved` (the directory pre-unwraps .pls/.m3u
    /// playlist pointers), falling back to raw `url` when empty.
    pub fn stream_url(&self) -> &str {
        if self.url_resolved.is_empty() {
            &self.url
        } else {
            &self.url_resolved
        }
    }
}

/// Playback status surfaced to the UI. Written from playback threads via
/// `with_state`, read every frame by the render thread.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum RadioStatus {
    #[default]
    Idle,
    Connecting,
    /// Headers arrived; prefetching the stream ring before decode starts.
    Buffering,
    Playing,
    /// Sink paused in place, session alive: a short pause resumes from the
    /// buffer instantly; a long one re-tunes via the stall machinery.
    Paused,
    Stalled,
    Stopped,
    /// Audio output device disappeared (unplug / default change).
    /// cpal does not recover on its own; the user presses play to reopen.
    DeviceLost,
    Error(String),
}

/// Shared now-playing cell: written by the ICY metadata callback on the
/// playback path, read every frame by the UI. `None` = no title yet (or the
/// station sends none). The title is raw `StreamTitle` text — never split
/// into artist/title (the convention is unreliable), always sanitized.
pub type NowPlayingCell = Arc<Mutex<Option<String>>>;

/// Client-side ordering for the results list. Popular = the directory's
/// vote order (restorable — `RbStation` keeps `votes`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RadioSort {
    #[default]
    Popular,
    Name,
    Bitrate,
    Country,
}

/// Per-session UI state for the Radio tab, hanging off `AddonState`.
#[derive(Clone, Default)]
pub struct RadioUiState {
    pub status: RadioStatus,
    /// Station currently loaded (connecting/playing/stalled), if any.
    pub current: Option<RbStation>,
    /// True once a search has run this session (auto genre restore included) —
    /// gates the one-shot first-open genre load.
    pub auto_kicked: bool,
    pub now_playing: NowPlayingCell,
    pub search_text: String,
    pub results: Vec<RbStation>,
    /// True while a directory search worker is in flight.
    pub searching: bool,
    /// Transient one-line error surfaced in the tab.
    pub last_error: Option<String>,
    /// Genre chip currently selected (radio-browser tag), if any.
    pub selected_genre: Option<&'static str>,
    /// Results ordering; applied on publish and on combo change.
    pub sort: RadioSort,
    /// Mini radio: put the strip back at its default spot on the next frame.
    pub mini_snap: bool,
    /// Mini radio: width this session, followed live while the corner is
    /// dragged (the saved width only updates on release).
    pub mini_live_w: Option<f32>,
    /// Mini radio: the rect when the player pressed on the strip, while the
    /// button is held; the release saves only if the rect moved from it.
    pub mini_press: Option<crate::ui::mini_radio::Rect>,
    /// Mini radio: display size seen last frame; a change re-applies the
    /// clamped placement.
    pub mini_display: [f32; 2],
    /// Mini radio: the show/hide fade in flight (or settled).
    pub mini_fade: crate::ui::mini_radio::Fade,
    /// Mini radio: the hover-only controls row's fade.
    pub mini_hover: crate::ui::mini_radio::HoverFade,
    /// Mini radio: when the anchor was last flipped (`theme::elapsed_ms`),
    /// for the padlock's confirmation flash.
    pub mini_anchor_flash: Option<u64>,
}

impl RadioUiState {
    pub(crate) fn merge_paint(&mut self, base: &Self, paint: &Self) {
        crate::state::keep_worker(&mut self.status, &base.status, &paint.status);
        crate::state::keep_worker(&mut self.current, &base.current, &paint.current);
        crate::state::take_ui(&mut self.auto_kicked, &base.auto_kicked, &paint.auto_kicked);
        // `now_playing` is an `Arc` shared with the snapshot.
        crate::state::take_ui(&mut self.search_text, &base.search_text, &paint.search_text);
        crate::state::keep_worker(&mut self.results, &base.results, &paint.results);
        crate::state::merge_busy(&mut self.searching, base.searching, paint.searching);
        crate::state::merge_message(&mut self.last_error, &base.last_error, &paint.last_error);
        crate::state::take_ui(
            &mut self.selected_genre,
            &base.selected_genre,
            &paint.selected_genre,
        );
        crate::state::take_ui(&mut self.sort, &base.sort, &paint.sort);
        crate::state::take_ui(&mut self.mini_snap, &base.mini_snap, &paint.mini_snap);
        crate::state::take_ui(&mut self.mini_live_w, &base.mini_live_w, &paint.mini_live_w);
        crate::state::take_ui(&mut self.mini_press, &base.mini_press, &paint.mini_press);
        crate::state::take_ui(
            &mut self.mini_display,
            &base.mini_display,
            &paint.mini_display,
        );
        crate::state::take_ui(&mut self.mini_fade, &base.mini_fade, &paint.mini_fade);
        crate::state::take_ui(&mut self.mini_hover, &base.mini_hover, &paint.mini_hover);
        crate::state::take_ui(
            &mut self.mini_anchor_flash,
            &base.mini_anchor_flash,
            &paint.mini_anchor_flash,
        );
    }
}
