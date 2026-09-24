//! Application configuration — API keys and user preferences.
//! Stored as JSON in the Nexus addon directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which LLM provider is active.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LlmProvider {
    #[default]
    Gemini,
    OpenAI,
    Anthropic,
    /// OpenRouter — OpenAI-compatible API at https://openrouter.ai, gateway
    /// to hundreds of hosted models (Anthropic, OpenAI, Google, Mistral, etc.)
    /// with a single API key. Uses Chat Completions + tools format.
    OpenRouter,
}

impl LlmProvider {
    pub const ALL: [LlmProvider; 4] = [
        LlmProvider::Gemini,
        LlmProvider::OpenAI,
        LlmProvider::Anthropic,
        LlmProvider::OpenRouter,
    ];

    pub fn label(&self) -> &str {
        match self {
            LlmProvider::Gemini => "Google Gemini",
            LlmProvider::OpenAI => "OpenAI",
            LlmProvider::Anthropic => "Anthropic (Claude)",
            LlmProvider::OpenRouter => "OpenRouter",
        }
    }

    pub fn short_label(&self) -> &str {
        match self {
            LlmProvider::Gemini => "Gemini",
            LlmProvider::OpenAI => "OpenAI",
            LlmProvider::Anthropic => "Claude",
            LlmProvider::OpenRouter => "OpenRouter",
        }
    }

    /// Page where a first-time user creates an API key.
    ///
    /// A live GET of the Gemini and OpenRouter URLs on 2026-09-06 opened a
    /// sign-in wall, not a Create Key button. Overlay copy has to say that.
    pub fn key_page_url(&self) -> &'static str {
        match self {
            LlmProvider::Gemini => "https://aistudio.google.com/apikey",
            LlmProvider::OpenAI => "https://platform.openai.com/api-keys",
            LlmProvider::Anthropic => "https://console.anthropic.com/settings/keys",
            LlmProvider::OpenRouter => "https://openrouter.ai/keys",
        }
    }

    /// Locale key for the one-line "what this page is" line. Same key on
    /// setup and Settings so the on-ramp is worded once.
    pub fn setup_howto_key(&self) -> &'static str {
        match self {
            LlmProvider::Gemini => "setup.gemini_howto",
            LlmProvider::OpenAI => "setup.openai_howto",
            LlmProvider::Anthropic => "setup.anthropic_howto",
            LlmProvider::OpenRouter => "setup.openrouter_howto",
        }
    }

    /// Locale key for the sign-in-then-create-key steps. Same key on both
    /// screens; see [`Self::setup_howto_key`].
    pub fn setup_steps_key(&self) -> &'static str {
        match self {
            LlmProvider::Gemini => "setup.gemini_steps",
            LlmProvider::OpenAI => "setup.openai_steps",
            LlmProvider::Anthropic => "setup.anthropic_steps",
            LlmProvider::OpenRouter => "setup.openrouter_steps",
        }
    }
}

/// Whether the config held in memory may be written back over `config.json`.
///
/// Only [`AppConfig::load`] ever selects [`SavePolicy::RefuseUnreadFileOnDisk`],
/// and the field carrying it is deliberately not serialized: it describes what
/// this run knows about the file, never anything stored inside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SavePolicy {
    /// Nothing on disk is unaccounted for — this value came from a successful
    /// load, from a file that was read but could not be parsed, or from
    /// [`AppConfig::default`].
    #[default]
    Writable,
    /// `config.json` exists and could not be read, so the user's real settings
    /// — API keys included — are still inside it and are *not* in this value.
    /// Writing this value out would destroy them, so [`AppConfig::save`]
    /// refuses. Recovery is a reload (restart the addon once the file is
    /// readable again) or an explicit `reset_to_first_run`, which builds a
    /// fresh [`AppConfig::default`] and is therefore `Writable`.
    RefuseUnreadFileOnDisk,
}

/// What a feed can paint in the overlay (text + stills, never playback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NewsKind {
    Articles,
    Notes,
    Video,
    Guides,
}

impl NewsKind {
    pub const ALL: [NewsKind; 4] = [
        NewsKind::Articles,
        NewsKind::Notes,
        NewsKind::Video,
        NewsKind::Guides,
    ];

    pub fn label_key(self) -> &'static str {
        match self {
            NewsKind::Articles => "news.kind.articles",
            NewsKind::Notes => "news.kind.notes",
            NewsKind::Video => "news.kind.video",
            NewsKind::Guides => "news.kind.guides",
        }
    }

    pub fn settings_key(self) -> &'static str {
        match self {
            NewsKind::Articles => "settings.news_articles",
            NewsKind::Notes => "settings.news_notes",
            NewsKind::Video => "settings.news_video",
            NewsKind::Guides => "settings.news_guides",
        }
    }

    pub fn sources(self) -> &'static [NewsSource] {
        match self {
            NewsKind::Articles => &[NewsSource::Official, NewsSource::ForumNews],
            NewsKind::Notes => &[NewsSource::PatchNotes],
            NewsKind::Video => &[NewsSource::Youtube],
            NewsKind::Guides => &[NewsSource::GuildJen],
        }
    }
}

/// Which public feeds the News tab can show. Setup always uses [`NewsSource::Official`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsSource {
    Official,
    ForumNews,
    PatchNotes,
    Youtube,
    GuildJen,
}

impl NewsSource {
    pub const ALL: [NewsSource; 5] = [
        NewsSource::Official,
        NewsSource::ForumNews,
        NewsSource::PatchNotes,
        NewsSource::Youtube,
        NewsSource::GuildJen,
    ];

    pub fn index(self) -> usize {
        match self {
            NewsSource::Official => 0,
            NewsSource::ForumNews => 1,
            NewsSource::PatchNotes => 2,
            NewsSource::Youtube => 3,
            NewsSource::GuildJen => 4,
        }
    }

    pub fn kind(self) -> NewsKind {
        match self {
            NewsSource::Official | NewsSource::ForumNews => NewsKind::Articles,
            NewsSource::PatchNotes => NewsKind::Notes,
            NewsSource::Youtube => NewsKind::Video,
            NewsSource::GuildJen => NewsKind::Guides,
        }
    }

    pub fn label_key(self) -> &'static str {
        match self {
            NewsSource::Official => "news.source.official",
            NewsSource::ForumNews => "news.source.forum",
            NewsSource::PatchNotes => "news.source.patch_notes",
            NewsSource::Youtube => "news.source.youtube",
            NewsSource::GuildJen => "news.source.guildjen",
        }
    }

    pub fn hint_key(self) -> &'static str {
        match self {
            NewsSource::Official => "news.source.official.hint",
            NewsSource::ForumNews => "news.source.forum.hint",
            NewsSource::PatchNotes => "news.source.patch_notes.hint",
            NewsSource::Youtube => "news.source.youtube.hint",
            NewsSource::GuildJen => "news.source.guildjen.hint",
        }
    }
}

/// How the News tab is laid out. Old configs used `timeline` / `by_source`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsLayout {
    #[default]
    #[serde(alias = "timeline")]
    Desk,
    #[serde(alias = "by_source")]
    Magazine,
    Reader,
}

impl NewsLayout {
    pub fn label_key(self) -> &'static str {
        match self {
            NewsLayout::Desk => "news.layout.desk",
            NewsLayout::Magazine => "news.layout.magazine",
            NewsLayout::Reader => "news.layout.reader",
        }
    }
}

/// Which currency cost estimates (Settings, generation pill/tooltip) are shown in.
/// Stored values are always USD; this only affects display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostCurrency {
    #[default]
    Usd,
    Eur,
}

fn default_show_images() -> bool {
    true
}

fn default_true() -> bool {
    true
}

/// Ticked sources + reading tools. All sources off until the player opts in —
/// that is what hides the News tab. Setup still paints official GW2 headlines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsPreferences {
    #[serde(default)]
    pub official: bool,
    #[serde(default)]
    pub forum_news: bool,
    #[serde(default)]
    pub patch_notes: bool,
    #[serde(default)]
    pub youtube: bool,
    #[serde(default)]
    pub guildjen: bool,
    #[serde(default)]
    pub layout: NewsLayout,
    #[serde(default = "default_show_images")]
    pub show_images: bool,
}

impl Default for NewsPreferences {
    fn default() -> Self {
        Self {
            official: false,
            forum_news: false,
            patch_notes: false,
            youtube: false,
            guildjen: false,
            layout: NewsLayout::Desk,
            show_images: true,
        }
    }
}

impl NewsPreferences {
    pub fn get(&self, src: NewsSource) -> bool {
        match src {
            NewsSource::Official => self.official,
            NewsSource::ForumNews => self.forum_news,
            NewsSource::PatchNotes => self.patch_notes,
            NewsSource::Youtube => self.youtube,
            NewsSource::GuildJen => self.guildjen,
        }
    }

    pub fn set(&mut self, src: NewsSource, on: bool) {
        match src {
            NewsSource::Official => self.official = on,
            NewsSource::ForumNews => self.forum_news = on,
            NewsSource::PatchNotes => self.patch_notes = on,
            NewsSource::Youtube => self.youtube = on,
            NewsSource::GuildJen => self.guildjen = on,
        }
    }

    pub fn any_enabled(&self) -> bool {
        NewsSource::ALL.iter().any(|&s| self.get(s))
    }

    pub fn enabled_sources(&self) -> Vec<NewsSource> {
        NewsSource::ALL
            .into_iter()
            .filter(|&s| self.get(s))
            .collect()
    }
}

/// Snapshot of a station the player saved — enough to re-tune and render the
/// row offline, without a directory round-trip, across mirror changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedStation {
    pub stationuuid: String,
    pub name: String,
    /// Resolved stream URL at save time.
    pub url: String,
    #[serde(default)]
    pub favicon: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub bitrate: u32,
    #[serde(default)]
    pub countrycode: String,
    #[serde(default)]
    pub tags: String,
}

fn default_radio_volume() -> u8 {
    60
}

fn default_radio_language_filter() -> String {
    "auto".to_string()
}

fn default_radio_country_filter() -> String {
    "any".to_string()
}

/// Radio tab persistence: favorites + volume + last station.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadioPreferences {
    /// Favorite stations, deduped by `stationuuid` (fallback: trimmed url).
    #[serde(default)]
    pub favorites: Vec<SavedStation>,
    /// Output volume percent 0-100; log taper applied at the sink.
    #[serde(default = "default_radio_volume")]
    pub volume_percent: u8,
    /// Last station played, for the keybind toggle and quick resume.
    #[serde(default)]
    pub last_station: Option<SavedStation>,
    /// Station language filter: "auto" follows the overlay language, "any"
    /// disables it, otherwise a radio-browser language name ("english", ...).
    #[serde(default = "default_radio_language_filter")]
    pub language_filter: String,
    /// Station country filter: "any", or an ISO 3166-1 alpha-2 code.
    #[serde(default = "default_radio_country_filter")]
    pub country_filter: String,
    /// Search bitrate cap in kbps for poor connections; 0 = no cap.
    #[serde(default)]
    pub bitrate_max: u32,
    /// Lower the radio volume while the character is in combat (mumble link).
    #[serde(default)]
    pub duck_in_combat: bool,
    /// Let Choya fetch AI-written quips about the current song (opt-in;
    /// uses the configured LLM provider, hard-capped per day).
    #[serde(default)]
    pub ai_quips: bool,
    /// Last genre chip selected (radio-browser tag). Restored on the tab's
    /// first open each session; "world music" greets new listeners.
    #[serde(default = "default_radio_genre")]
    pub last_genre: String,
}

fn default_radio_genre() -> String {
    "world music".into()
}

impl Default for RadioPreferences {
    fn default() -> Self {
        Self {
            favorites: Vec::new(),
            volume_percent: default_radio_volume(),
            last_station: None,
            language_filter: default_radio_language_filter(),
            country_filter: default_radio_country_filter(),
            bitrate_max: 0,
            duck_in_combat: false,
            ai_quips: false,
            last_genre: default_radio_genre(),
        }
    }
}

fn default_theme_preset() -> String {
    "glacial-ward".into()
}

/// Overlay theme selection. `preset` is a built-in theme id (or `"custom"`,
/// which renders from [`ThemeConfig::custom`]); unknown ids fall back to
/// Tyrian Gold at apply time, never at parse time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThemeConfig {
    #[serde(default = "default_theme_preset")]
    pub preset: String,
    /// The theme being edited. Always the live buffer, whether it is a new
    /// one or a copy of a saved theme.
    #[serde(default)]
    pub custom: CustomTheme,
    /// Themes the player has named and kept.
    ///
    /// Naming a theme used to be the end of it: the one custom slot took the
    /// new name, the "Custom" row in the picker became that name, and there
    /// was no way back to an unnamed slot to start a second one. Named
    /// themes live here, and the picker keeps "Custom" beneath them.
    #[serde(default)]
    pub saved: Vec<CustomTheme>,
}

impl ThemeConfig {
    /// Keep `theme` in the saved list, replacing any entry of the same name.
    ///
    /// Name is the identity because it is what the picker shows: two rows
    /// reading "Rob" would be indistinguishable to the person choosing one.
    /// An unnamed theme is the scratch slot and is never saved.
    pub fn remember(&mut self, theme: &CustomTheme) {
        let name = theme.name.trim();
        if name.is_empty() {
            return;
        }
        match self
            .saved
            .iter_mut()
            .find(|kept| kept.name.trim().eq_ignore_ascii_case(name))
        {
            Some(kept) => *kept = theme.clone(),
            None => self.saved.push(theme.clone()),
        }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: default_theme_preset(),
            custom: CustomTheme::default(),
            saved: Vec::new(),
        }
    }
}

/// The 5 base colors (RGB 0..=1) a custom theme derives its palette from.
/// Defaults are the Tyrian Gold bases so an untouched custom editor starts
/// from the shipped look.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomTheme {
    #[serde(default)]
    pub name: String,
    pub bg: [f32; 3],
    pub panel: [f32; 3],
    pub accent: [f32; 3],
    pub text: [f32; 3],
    pub muted: [f32; 3],
}

impl Default for CustomTheme {
    fn default() -> Self {
        Self {
            name: String::new(),
            bg: [0.07, 0.06, 0.045],
            panel: [0.12, 0.10, 0.07],
            accent: [1.0, 0.84, 0.38],
            text: [0.93, 0.90, 0.82],
            muted: [0.58, 0.54, 0.46],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub gw2_api_key: Option<String>,
    pub cache_build_number: Option<u32>,

    // AI Provider Configuration
    /// Active LLM provider. Defaults to Gemini for backward compat.
    #[serde(default)]
    pub active_provider: LlmProvider,

    /// Gemini API key (Google AI Studio).
    pub gemini_api_key: Option<String>,
    /// Gemini model ID (e.g. "gemini-2.5-flash").
    #[serde(default)]
    pub gemini_model: Option<String>,

    /// OpenAI API key.
    #[serde(default)]
    pub openai_api_key: Option<String>,
    /// OpenAI model ID (e.g. "gpt-4o").
    #[serde(default)]
    pub openai_model: Option<String>,

    /// Anthropic API key.
    #[serde(default)]
    pub anthropic_api_key: Option<String>,
    /// Anthropic model ID (e.g. "claude-sonnet-4-6").
    #[serde(default)]
    pub anthropic_model: Option<String>,

    /// OpenRouter API key (https://openrouter.ai).
    #[serde(default)]
    pub openrouter_api_key: Option<String>,
    /// OpenRouter model ID (e.g. "anthropic/claude-sonnet-4-5").
    #[serde(default)]
    pub openrouter_model: Option<String>,
    /// Show only models that cost nothing to use.
    ///
    /// On by default, because that is what most people installing this will
    /// want: a free account, no card, no spend. OpenRouter lists 431 models
    /// and 22 of them are free — finding those by scrolling is not something
    /// to ask of anyone. Turning it off shows everything.
    #[serde(default = "default_true")]
    pub free_models_only: bool,

    // UI Preferences
    /// Window opacity (0.0–1.0). Default 1.0.
    #[serde(default = "default_opacity")]
    pub window_opacity: f32,
    /// Font scale multiplier. Default 1.0.
    #[serde(default = "default_font_scale")]
    pub font_scale: f32,
    /// Overlay theme. Omitted on old configs; defaults to Glacial Ward.
    #[serde(default)]
    pub theme: ThemeConfig,

    // Layout Tuning
    /// Left panel width in pixels. Default 360.
    #[serde(default = "default_left_panel_width")]
    pub left_panel_width: f32,
    /// Inner padding for left panel (px from edge). Default 6.
    #[serde(default = "default_panel_padding")]
    pub panel_padding: f32,
    /// Vertical spacing between sections (px). Default 4.
    #[serde(default = "default_section_spacing")]
    pub section_spacing: f32,
    /// Content area left indent (px). Default 4.
    #[serde(default = "default_content_indent")]
    pub content_indent: f32,

    /// Show the overlay when GW2 / Nexus loads. Default true.
    #[serde(default = "default_window_visible")]
    pub window_visible: bool,
    /// Last overlay position (screen px). None = default corner.
    #[serde(default)]
    pub window_x: Option<f32>,
    #[serde(default)]
    pub window_y: Option<f32>,
    /// Last overlay size (px). None = 800×600. Tiny values are clamped on restore.
    #[serde(default)]
    pub window_w: Option<f32>,
    #[serde(default)]
    pub window_h: Option<f32>,

    // Optimization Defaults
    /// Default game mode for new optimizations.
    #[serde(default)]
    pub default_game_mode: Option<String>,
    /// Default scale (`Solo` / `Party` / `Squad`) applied at startup.
    #[serde(default)]
    pub default_combat_tier: Option<String>,
    /// Default role (a `RoleObjective` name) applied at startup.
    #[serde(default)]
    pub default_role: Option<String>,

    // Cache & Data
    /// Auto-refresh game data cache on startup.
    #[serde(default)]
    pub auto_refresh_cache: bool,

    /// Overlay language. `"auto"` follows the OS UI language. Additive — old configs omit this.
    #[serde(default = "default_ui_language")]
    pub ui_language: String,

    /// Overlay typeface. `"auto"` picks a Windows font from language; `"game"` keeps Nexus.
    #[serde(default = "default_ui_font")]
    pub ui_font: String,

    /// News tab sources. Omitted on old configs; defaults all-off (tab hidden).
    #[serde(default)]
    pub news: NewsPreferences,

    /// Radio tab: favorites, volume, last station. Omitted on old configs.
    #[serde(default)]
    pub radio: RadioPreferences,

    /// Random id minted once per install for the feedback server (never an account id).
    #[serde(default)]
    pub client_id: Option<String>,

    /// Currency cost estimates are displayed in. Omitted on old configs; defaults USD.
    #[serde(default)]
    pub cost_currency: CostCurrency,

    /// Never persisted. Set by [`AppConfig::load`] when an existing
    /// `config.json` could not be read, so [`AppConfig::save`] will not write
    /// these defaults over settings this run never saw. `#[serde(skip)]` keeps
    /// the on-disk shape byte-for-byte unchanged for existing installs.
    #[serde(skip)]
    pub save_policy: SavePolicy,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            gw2_api_key: None,
            cache_build_number: None,
            active_provider: LlmProvider::default(),
            gemini_api_key: None,
            gemini_model: None,
            openai_api_key: None,
            openai_model: None,
            anthropic_api_key: None,
            anthropic_model: None,
            openrouter_api_key: None,
            openrouter_model: None,
            free_models_only: true,
            window_opacity: 1.0,
            font_scale: 1.0,
            theme: ThemeConfig::default(),
            left_panel_width: 360.0,
            panel_padding: 6.0,
            section_spacing: 4.0,
            content_indent: 4.0,
            window_visible: true,
            window_x: None,
            window_y: None,
            window_w: None,
            window_h: None,
            default_game_mode: None,
            default_combat_tier: None,
            default_role: None,
            auto_refresh_cache: false,
            ui_language: "auto".into(),
            ui_font: "auto".into(),
            news: NewsPreferences::default(),
            radio: RadioPreferences::default(),
            client_id: None,
            cost_currency: CostCurrency::default(),
            save_policy: SavePolicy::Writable,
        }
    }
}

macro_rules! default_f32 {
    ($name:ident = $value:expr) => {
        fn $name() -> f32 {
            $value
        }
    };
}

default_f32!(default_opacity = 1.0);
default_f32!(default_font_scale = 1.0);
default_f32!(default_left_panel_width = 360.0);
default_f32!(default_panel_padding = 6.0);
default_f32!(default_section_spacing = 4.0);
default_f32!(default_content_indent = 4.0);

fn default_window_visible() -> bool {
    true
}

fn default_ui_language() -> String {
    "auto".into()
}

fn default_ui_font() -> String {
    "auto".into()
}

/// Sibling backup path: `config.json` → `config.json.bak`.
///
/// ponytail: one slot, overwrite previous; timestamp rotation if history is needed.
fn sibling_bak(path: &Path) -> PathBuf {
    let mut bak = path.as_os_str().to_owned();
    bak.push(".bak");
    PathBuf::from(bak)
}

/// Copy the on-disk bytes to [`sibling_bak`] before returning Writable defaults.
/// Uses `fs::copy` so InvalidData (no Rust `String`) still preserves exact bytes.
/// Best-effort: a copy failure must not block the reset banner.
fn backup_unparseable_config(path: &Path) -> bool {
    std::fs::copy(path, sibling_bak(path)).is_ok()
}

/// User-facing message for a `config.json` whose contents were readable but
/// unusable. Shared by the JSON-parse and non-UTF-8 arms of [`AppConfig::load`].
/// Saving stays enabled so Settings can repair the file; a sibling `.bak`
/// holds the previous bytes when the copy succeeded.
fn settings_reset_message(cause: &dyn std::fmt::Display, backed_up: bool) -> String {
    if backed_up {
        format!(
            "config.json could not be parsed ({}). All settings reset to defaults. \
             Previous file saved as config.json.bak.",
            cause
        )
    } else {
        format!(
            "config.json could not be parsed ({}). All settings reset to defaults.",
            cause
        )
    }
}

pub const DEFAULT_WINDOW_POS: [f32; 2] = [80.0, 80.0];
/// Fallback when the display size is unknown. First-run / reset uses
/// [`initial_window_size`] (~80% of the monitor, width capped at 1920).
pub const DEFAULT_WINDOW_SIZE: [f32; 2] = [1536.0, 864.0];
pub const MIN_WINDOW_SIZE: [f32; 2] = [640.0, 400.0];

/// First-run and "Reset layout" size: 80% of the monitor. Ultrawide
/// (`width/height > 2`) is sized as 1920-wide so the overlay does not
/// stretch across the whole desk. Unknown / tiny display → [`DEFAULT_WINDOW_SIZE`].
pub fn initial_window_size(display: [f32; 2]) -> [f32; 2] {
    let dw = display[0];
    let dh = display[1];
    if dw < MIN_WINDOW_SIZE[0] || dh < MIN_WINDOW_SIZE[1] {
        return DEFAULT_WINDOW_SIZE;
    }
    let usable_w = if dw / dh > 2.0 {
        1920.0_f32.min(dw)
    } else {
        dw
    };
    let margin = DEFAULT_WINDOW_POS[0];
    let w = (usable_w * 0.8).min(dw - margin).max(MIN_WINDOW_SIZE[0]);
    let h = (dh * 0.8).min(dh - margin).max(MIN_WINDOW_SIZE[1]);
    [w, h]
}

/// Known Gemini models — fallback shown when list_models() API call fails.
/// The Settings tab populates this list dynamically from the API at runtime.
pub const GEMINI_MODELS: &[(&str, &str)] = &[
    ("gemini-2.5-flash", "Gemini 2.5 Flash (fast, free tier)"),
    (
        "gemini-3-pro-preview",
        "Gemini 3 Pro Preview (advanced reasoning)",
    ),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview (latest)"),
];

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";

/// Known OpenAI models available for selection.
pub const OPENAI_MODELS: &[(&str, &str)] = &[
    ("gpt-4o", "GPT-4o (multimodal, fast)"),
    ("gpt-4o-mini", "GPT-4o Mini (cheaper, fast)"),
    ("o3-mini", "o3-mini (reasoning)"),
];

pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";

/// Known Anthropic models available for selection.
pub const ANTHROPIC_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-4-6", "Claude Sonnet 4.6 (balanced)"),
    (
        "claude-haiku-4-5-20251001",
        "Claude Haiku 4.5 (fast, cheap)",
    ),
];

/// Known OpenRouter models — fallback shown when list_models() API call
/// fails. The Settings tab populates this list dynamically from the
/// OpenRouter `/models` endpoint at runtime.
pub const OPENROUTER_MODELS: &[(&str, &str)] = &[
    (
        "anthropic/claude-sonnet-4-5",
        "Claude Sonnet 4.5 (Anthropic via OR)",
    ),
    ("openai/gpt-4o-mini", "GPT-4o Mini (OpenAI via OR)"),
    (
        "google/gemini-2.5-flash",
        "Gemini 2.5 Flash (Google via OR)",
    ),
];

pub const DEFAULT_OPENROUTER_MODEL: &str = "anthropic/claude-sonnet-4-5";

pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";

impl AppConfig {
    /// Restored overlay rect. Missing size falls back to [`DEFAULT_WINDOW_SIZE`]
    /// (1080p 80%). Tiny saved sizes clamp to [`MIN_WINDOW_SIZE`]. First-run
    /// paint uses [`initial_window_size`] so the overlay matches the monitor.
    pub fn window_rect(&self) -> ([f32; 2], [f32; 2]) {
        let pos = [
            self.window_x.unwrap_or(DEFAULT_WINDOW_POS[0]),
            self.window_y.unwrap_or(DEFAULT_WINDOW_POS[1]),
        ];
        let size = [
            self.window_w
                .unwrap_or(DEFAULT_WINDOW_SIZE[0])
                .max(MIN_WINDOW_SIZE[0]),
            self.window_h
                .unwrap_or(DEFAULT_WINDOW_SIZE[1])
                .max(MIN_WINDOW_SIZE[1]),
        ];
        (pos, size)
    }

    pub fn set_window_rect(&mut self, pos: [f32; 2], size: [f32; 2]) {
        self.window_x = Some(pos[0]);
        self.window_y = Some(pos[1]);
        self.window_w = Some(size[0].max(MIN_WINDOW_SIZE[0]));
        self.window_h = Some(size[1].max(MIN_WINDOW_SIZE[1]));
    }

    pub fn gemini_model_id(&self) -> &str {
        self.gemini_model.as_deref().unwrap_or(DEFAULT_GEMINI_MODEL)
    }

    pub fn openai_model_id(&self) -> &str {
        self.openai_model.as_deref().unwrap_or(DEFAULT_OPENAI_MODEL)
    }

    pub fn anthropic_model_id(&self) -> &str {
        self.anthropic_model
            .as_deref()
            .unwrap_or(DEFAULT_ANTHROPIC_MODEL)
    }

    pub fn openrouter_model_id(&self) -> &str {
        self.openrouter_model
            .as_deref()
            .unwrap_or(DEFAULT_OPENROUTER_MODEL)
    }

    pub fn active_model_id(&self) -> &str {
        match self.active_provider {
            LlmProvider::Gemini => self.gemini_model_id(),
            LlmProvider::OpenAI => self.openai_model_id(),
            LlmProvider::Anthropic => self.anthropic_model_id(),
            LlmProvider::OpenRouter => self.openrouter_model_id(),
        }
    }

    pub fn set_active_model_id(&mut self, id: String) {
        match self.active_provider {
            LlmProvider::Gemini => self.gemini_model = Some(id),
            LlmProvider::OpenAI => self.openai_model = Some(id),
            LlmProvider::Anthropic => self.anthropic_model = Some(id),
            LlmProvider::OpenRouter => self.openrouter_model = Some(id),
        }
    }

    pub fn active_api_key(&self) -> Option<&str> {
        match self.active_provider {
            LlmProvider::Gemini => self.gemini_api_key.as_deref(),
            LlmProvider::OpenAI => self.openai_api_key.as_deref(),
            LlmProvider::Anthropic => self.anthropic_api_key.as_deref(),
            LlmProvider::OpenRouter => self.openrouter_api_key.as_deref(),
        }
    }

    /// Load config from disk. Returns `(config, error_message)`; the message is
    /// `Some` whenever a file is on disk whose settings could not be recovered
    /// into this value, and callers surface it to the user.
    ///
    /// The three failure shapes are deliberately not collapsed, because each
    /// one calls for something different:
    ///
    /// * **No file** — an ordinary first run. Silent defaults, saving is normal.
    ///   No backup is written.
    /// * **Unreadable file** — a sharing violation from antivirus or cloud
    ///   sync, a permission change, a failing disk. The user's keys are still
    ///   in that file and are *not* in the value returned here, so the returned
    ///   config carries [`SavePolicy::RefuseUnreadFileOnDisk`] and cannot be
    ///   written back over them. No backup is written — the original file is
    ///   the recovery.
    /// * **Unparseable file** — readable bytes that serde or UTF-8 could not
    ///   accept (missing field, bad enum, Notepad UTF-16). Keys may still be
    ///   in those bytes, so a sibling `.bak` is written (best-effort) before
    ///   defaults are returned. Saving stays [`SavePolicy::Writable`] so the
    ///   user can repair from the UI; the previous file remains at
    ///   `<filename>.bak`. Parse failure must not use
    ///   [`SavePolicy::RefuseUnreadFileOnDisk`] — that traps Settings forever.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            // First run. Nothing to lose, nothing to report.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), None),
            // Readable bytes, unusable content. Same class as a parse failure:
            // treating it as "unread user data" would block saves forever.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                let backed_up = backup_unparseable_config(path);
                return (Self::default(), Some(settings_reset_message(&e, backed_up)));
            }
            Err(e) => {
                let guarded = Self {
                    save_policy: SavePolicy::RefuseUnreadFileOnDisk,
                    ..Self::default()
                };
                // Kept short on purpose: this lands in the one-line, unwrapped
                // status bar, and anything longer pushes its Dismiss button off
                // the window.
                let msg = format!(
                    "config.json could not be read ({}). Your saved settings are safe but \
                     locked — close what is holding the file and restart the addon.",
                    e
                );
                return (guarded, Some(msg));
            }
        };

        match serde_json::from_str::<Self>(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => {
                let backed_up = backup_unparseable_config(path);
                (Self::default(), Some(settings_reset_message(&e, backed_up)))
            }
        }
    }

    /// Persist to `path` by writing a private staging file and renaming it over
    /// the target, so a failed or partial write can never truncate the file that
    /// is already there, and two saves running at once cannot land in one
    /// another's bytes. See `AppConfig::staging_path`.
    ///
    /// Refuses outright when this value came from a config file that could not
    /// be read — writing defaults over a file that still holds the user's API
    /// keys is the one outcome this must never produce. See [`SavePolicy`].
    ///
    /// ponytail: no `fsync` before the rename, so the publish is atomic and
    /// survives a process crash but not a power cut mid-write. Most callers run
    /// on the overlay's render thread, where an fsync is a visible frame hitch;
    /// revisit if config saves ever move off that thread.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        // Matched, not compared, so a future policy has to be triaged here
        // rather than silently defaulting to "go ahead and overwrite".
        match self.save_policy {
            SavePolicy::Writable => {}
            SavePolicy::RefuseUnreadFileOnDisk => {
                return Err(std::io::Error::other(
                    "refusing to overwrite config.json: it could not be read when the addon \
                     started and still holds your saved settings",
                ))
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp_path = Self::staging_path(path);
        // Clean up the orphan .tmp on either failure so it doesn't accumulate
        // after repeated failed saves (e.g. disk full, antivirus interruption).
        if let Err(e) = std::fs::write(&tmp_path, &json) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        if let Err(e) = crate::storage::replace_file(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(())
    }

    /// A private, unique staging destination for one in-progress [`save`].
    ///
    /// Unique because nothing serialises `save` any more. Config writes used to
    /// happen only with the addon's `STATE` mutex held, which made them one at a
    /// time by accident; the overlay now hands them to a background worker, so a
    /// save can be in flight while the render thread starts another one.
    ///
    /// Sharing one `config.tmp` between those two is not merely a lost write.
    /// Both `fs::write` calls open the same path and write from offset zero, so
    /// the bytes interleave — and **both calls can still return `Ok`**, which is
    /// why no amount of retrying on error covers it. Whichever rename runs first
    /// then publishes that mixture as `config.json`, [`AppConfig::load`] reads it
    /// as a parse failure, hands back defaults that are `Writable`, and the next
    /// save writes those defaults over the user's API keys.
    ///
    /// Process id plus a per-process counter gives every in-flight save its own
    /// file, so the only step two saves still share is the rename, which is
    /// atomic: the published config is always exactly one of them, whole. Same
    /// shape as `storage.rs`'s `staging_path`, for the same reason.
    ///
    /// The name keeps the target's stem, sits in the target's directory (so the
    /// rename stays on one volume), and ends in `.tmp`, never `.json`. A process
    /// killed mid-save can leave one behind — `save` removes its own on both
    /// failure paths, and an orphan is inert because nothing ever reads `*.tmp`.
    ///
    /// [`save`]: AppConfig::save
    fn staging_path(path: &Path) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        // `OsString`, not `format!`, so a non-UTF-8 path is not mangled on the
        // way through. `file_stem` is `None` only for a path with no file name,
        // which `save` could not have written to anyway.
        let mut name = path.file_stem().unwrap_or(path.as_os_str()).to_os_string();
        name.push(format!(".{}-{}.tmp", std::process::id(), seq));
        match path.parent() {
            Some(dir) => dir.join(name),
            None => PathBuf::from(name),
        }
    }

    pub fn has_gw2_key(&self) -> bool {
        self.gw2_api_key.as_ref().is_some_and(|k| !k.is_empty())
    }

    pub fn has_gemini_key(&self) -> bool {
        self.gemini_api_key.as_ref().is_some_and(|k| !k.is_empty())
    }

    pub fn has_openai_key(&self) -> bool {
        self.openai_api_key.as_ref().is_some_and(|k| !k.is_empty())
    }

    pub fn has_anthropic_key(&self) -> bool {
        self.anthropic_api_key
            .as_ref()
            .is_some_and(|k| !k.is_empty())
    }

    pub fn has_openrouter_key(&self) -> bool {
        self.openrouter_api_key
            .as_ref()
            .is_some_and(|k| !k.is_empty())
    }

    /// Whether the active provider has a valid API key.
    pub fn has_active_llm_key(&self) -> bool {
        match self.active_provider {
            LlmProvider::Gemini => self.has_gemini_key(),
            LlmProvider::OpenAI => self.has_openai_key(),
            LlmProvider::Anthropic => self.has_anthropic_key(),
            LlmProvider::OpenRouter => self.has_openrouter_key(),
        }
    }

    /// Setup is complete when GW2 key + any LLM key + cache are all present.
    pub fn is_setup_complete(&self) -> bool {
        self.has_gw2_key() && self.has_active_llm_key() && self.cache_build_number.is_some()
    }

    pub fn config_path(addon_dir: &Path) -> PathBuf {
        addon_dir.join("config.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn every_provider_has_a_key_page_url() {
        for provider in &LlmProvider::ALL {
            let url = provider.key_page_url();
            assert!(
                url.starts_with("https://"),
                "{url} is not an https key page"
            );
            assert!(
                provider.setup_howto_key().starts_with("setup."),
                "howto key must be a setup.* locale id"
            );
            assert!(
                provider.setup_steps_key().starts_with("setup."),
                "steps key must be a setup.* locale id"
            );
        }
        assert_eq!(
            LlmProvider::Gemini.key_page_url(),
            "https://aistudio.google.com/apikey"
        );
        assert_eq!(
            LlmProvider::OpenRouter.key_page_url(),
            "https://openrouter.ai/keys"
        );
    }

    /// Two saves of the same config file must never stage into one path.
    ///
    /// Config writes used to be serialised by the addon's `STATE` mutex; the
    /// overlay now hands them to a background worker, so a save can be in flight
    /// while the render thread starts another one. With the old shared
    /// `config.tmp` both `fs::write` calls landed in one file, both could still
    /// return `Ok`, and the first rename published the mixture — which `load`
    /// reads as a parse failure, replaces with `Writable` defaults, and the next
    /// save writes over the user's API keys with.
    ///
    /// Built deterministically rather than as a race this has to win: part 2 is
    /// the part that fails outright on the shared name.
    #[test]
    fn config_save_uses_unique_staging() {
        let dir = env::temp_dir().join(format!("gw2_config_staging_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // 1. Every staging path is private: different on each call, never the
        //    target, always beside it, and carrying this process id so a second
        //    process saving the same file cannot choose the same name either.
        let first = AppConfig::staging_path(&path);
        let second = AppConfig::staging_path(&path);
        assert_ne!(first, second, "two saves must not stage into the same file");
        assert_ne!(first, path, "staging must not be the published file");
        assert_eq!(
            first.parent(),
            path.parent(),
            "rename must stay on one volume"
        );
        assert_eq!(first.extension().and_then(|e| e.to_str()), Some("tmp"));
        let pid = std::process::id().to_string();
        for staged in [&first, &second] {
            let name = staged.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.contains(&pid),
                "staging name must be process-private, got {name}"
            );
        }

        // 2. Another writer's staging file is left completely alone. The old code
        //    wrote straight through `config.tmp` and renamed it away, so this is
        //    the assertion that fails on the shared name every single time.
        let foreign = dir.join("config.tmp");
        let foreign_bytes: &[u8] = b"{ another writer was here";
        std::fs::write(&foreign, foreign_bytes).unwrap();

        let config = AppConfig {
            client_id: Some("staging-probe".into()),
            ..Default::default()
        };
        config.save(&path).unwrap();

        assert!(
            foreign.exists(),
            "save consumed another writer's staging file instead of using its own"
        );
        assert_eq!(
            std::fs::read(&foreign).unwrap(),
            foreign_bytes,
            "save wrote through another writer's staging file"
        );
        let (loaded, err) = AppConfig::load(&path);
        assert!(err.is_none(), "published config must parse: {err:?}");
        assert_eq!(loaded.client_id.as_deref(), Some("staging-probe"));

        // 3. Saves that really do overlap publish whole files. With private
        //    staging the only shared step left is the rename, which is atomic, so
        //    `config.json` ends up as exactly one writer's config and never a
        //    blend of several.
        std::fs::remove_file(&foreign).unwrap();
        const WRITERS: usize = 8;
        // Deliberately different lengths: a byte-wise mixture of two of these
        // cannot pass for either one.
        let ids: Vec<String> = (0..WRITERS)
            .map(|i| format!("writer-{}{}", i, "x".repeat(i * 64)))
            .collect();
        let results: Vec<std::io::Result<()>> = std::thread::scope(|scope| {
            let handles: Vec<_> = ids
                .iter()
                .map(|id| {
                    let path = path.clone();
                    scope.spawn(move || {
                        AppConfig {
                            client_id: Some(id.clone()),
                            ..Default::default()
                        }
                        .save(&path)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        for (i, result) in results.iter().enumerate() {
            if let Err(e) = result {
                // A `NotFound` on the rename is exactly the shared-staging
                // symptom: someone else renamed this save's staging file away.
                // Anything else here is host noise (a scanner holding the
                // published file), not the property under test.
                assert_ne!(
                    e.kind(),
                    std::io::ErrorKind::NotFound,
                    "writer {i} lost its staging file to another save: {e}"
                );
            }
        }
        assert!(
            results.iter().any(|r| r.is_ok()),
            "at least one concurrent save must publish, else this proves nothing"
        );

        let (loaded, err) = AppConfig::load(&path);
        assert!(
            err.is_none(),
            "concurrent saves published an unparseable config: {err:?}"
        );
        let published = loaded
            .client_id
            .expect("published config kept its client_id");
        assert!(
            ids.contains(&published),
            "published config is a blend of writers, not one of them: {published}"
        );

        let leaked: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leaked.is_empty(), "staging files must not leak: {leaked:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert!(!config.is_setup_complete());
        assert!(!config.has_gw2_key());
        assert!(!config.has_gemini_key());
        assert_eq!(config.active_provider, LlmProvider::Gemini);
        assert_eq!(config.window_opacity, 1.0);
        assert_eq!(config.font_scale, 1.0);
    }

    #[test]
    fn test_save_and_load() {
        let dir = env::temp_dir().join(format!("gw2_config_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let config = AppConfig {
            gw2_api_key: Some("test-key-123".into()),
            gemini_api_key: Some("gemini-key-456".into()),
            cache_build_number: Some(12345),
            ..Default::default()
        };
        config.save(&path).unwrap();

        let (loaded, err) = AppConfig::load(&path);
        assert!(err.is_none(), "unexpected parse error: {:?}", err);
        assert_eq!(loaded.gw2_api_key.as_deref(), Some("test-key-123"));
        assert!(loaded.is_setup_complete());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_backward_compat_old_config() {
        // Simulate an old config.json that only has the original 3 fields
        let json = r#"{
            "gw2_api_key": "old-key",
            "gemini_api_key": "old-gemini-key",
            "cache_build_number": 12345
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.active_provider, LlmProvider::Gemini);
        assert!(config.openai_api_key.is_none());
        assert!(config.anthropic_api_key.is_none());
        assert_eq!(config.window_opacity, 1.0);
        assert_eq!(config.font_scale, 1.0);
        assert!(!config.auto_refresh_cache);
        assert_eq!(config.ui_language, "auto");
        assert_eq!(config.ui_font, "auto");
        assert_eq!(config.news, NewsPreferences::default());
        assert!(!config.news.any_enabled());
        assert!(config.is_setup_complete());
        assert!(config.window_visible);
        assert_eq!(
            config.window_rect(),
            (DEFAULT_WINDOW_POS, DEFAULT_WINDOW_SIZE)
        );
    }

    #[test]
    fn test_empty_json_round_trips_to_defaults() {
        // Regression guard for the `default_f32!` macro: every serde default
        // must still yield the AppConfig::default() value when loading `{}`.
        let config: AppConfig = serde_json::from_str("{}").unwrap();
        let defaults = AppConfig::default();
        assert_eq!(config.window_opacity, defaults.window_opacity);
        assert_eq!(config.font_scale, defaults.font_scale);
        assert_eq!(config.left_panel_width, defaults.left_panel_width);
        assert_eq!(config.panel_padding, defaults.panel_padding);
        assert_eq!(config.section_spacing, defaults.section_spacing);
        assert_eq!(config.content_indent, defaults.content_indent);
        assert!(config.window_visible);
        assert_eq!(config.ui_language, "auto");
        assert_eq!(config.ui_font, "auto");
        assert_eq!(config.news, NewsPreferences::default());
        assert!(config.client_id.is_none());
        assert_eq!(
            config.window_rect(),
            (DEFAULT_WINDOW_POS, DEFAULT_WINDOW_SIZE)
        );
    }

    #[test]
    fn old_config_without_cost_currency_defaults_usd() {
        // Pre-currency-toggle config.json: the field is missing entirely.
        let json = r#"{
            "gw2_api_key": "old-key",
            "gemini_api_key": "old-gemini-key",
            "cache_build_number": 12345
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.cost_currency, CostCurrency::Usd);
    }

    #[test]
    fn old_config_without_client_id_loads_none() {
        // Pre-About-tab config.json: the original 3 fields only. client_id
        // must deserialize as None, never fail or fabricate a value.
        let json = r#"{
            "gw2_api_key": "old-key",
            "gemini_api_key": "old-gemini-key",
            "cache_build_number": 12345
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(config.client_id.is_none());
    }

    #[test]
    fn client_id_round_trips() {
        let dir = env::temp_dir().join(format!("gw2_config_client_id_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let config = AppConfig {
            client_id: Some("11111111-1111-4111-8111-111111111111".into()),
            ..Default::default()
        };
        config.save(&path).unwrap();

        let (loaded, err) = AppConfig::load(&path);
        assert!(err.is_none(), "unexpected parse error: {:?}", err);
        assert_eq!(loaded.client_id, config.client_id);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn initial_window_size_is_80pct_on_1080p() {
        assert_eq!(initial_window_size([1920.0, 1080.0]), [1536.0, 864.0]);
    }

    #[test]
    fn initial_window_size_does_not_span_ultrawide() {
        let sz = initial_window_size([3440.0, 1440.0]);
        assert!((sz[0] - 1536.0).abs() < 0.5, "width {}", sz[0]);
        assert!((sz[1] - 1152.0).abs() < 0.5, "height {}", sz[1]);
    }

    #[test]
    fn initial_window_size_fits_laptop() {
        let sz = initial_window_size([1366.0, 768.0]);
        assert!(sz[0] < 1366.0 - 80.0, "width {}", sz[0]);
        assert!(sz[1] < 768.0 - 80.0, "height {}", sz[1]);
        assert!(sz[0] >= MIN_WINDOW_SIZE[0] && sz[1] >= MIN_WINDOW_SIZE[1]);
    }

    #[test]
    fn window_rect_clamps_tiny_saved_size() {
        let config = AppConfig {
            window_x: Some(120.0),
            window_y: Some(40.0),
            window_w: Some(80.0),
            window_h: Some(20.0),
            ..Default::default()
        };
        let (pos, size) = config.window_rect();
        assert_eq!(pos, [120.0, 40.0]);
        assert_eq!(size, MIN_WINDOW_SIZE);
    }

    #[test]
    fn test_openai_provider_setup_complete() {
        let config = AppConfig {
            gw2_api_key: Some("gw2-key".into()),
            active_provider: LlmProvider::OpenAI,
            openai_api_key: Some("sk-test123".into()),
            cache_build_number: Some(12345),
            ..Default::default()
        };
        assert!(config.is_setup_complete());
        assert_eq!(config.active_model_id(), "gpt-4o");
    }

    #[test]
    fn test_anthropic_provider_setup_complete() {
        let config = AppConfig {
            gw2_api_key: Some("gw2-key".into()),
            active_provider: LlmProvider::Anthropic,
            anthropic_api_key: Some("sk-ant-test123".into()),
            cache_build_number: Some(12345),
            ..Default::default()
        };
        assert!(config.is_setup_complete());
        assert_eq!(config.active_model_id(), "claude-sonnet-4-6");
    }

    #[test]
    fn test_provider_mismatch_not_complete() {
        // Has a Gemini key but active provider is OpenAI (no OpenAI key)
        let config = AppConfig {
            gw2_api_key: Some("gw2-key".into()),
            active_provider: LlmProvider::OpenAI,
            gemini_api_key: Some("gemini-key".into()),
            cache_build_number: Some(12345),
            ..Default::default()
        };
        assert!(!config.is_setup_complete());
    }

    #[test]
    fn test_load_parse_error_resets_to_defaults() {
        // A config file that exists but contains invalid JSON must surface
        // the parse error and fall back to defaults — this is the
        // user-visible "settings reset" path. Callers rely on Some(msg);
        // the previous bytes stay in a sibling .bak.
        let dir = env::temp_dir().join(format!("gw2_config_parse_err_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let corrupt = b"{ not valid json";
        std::fs::write(&path, corrupt).unwrap();

        let (loaded, err) = AppConfig::load(&path);
        assert!(err.is_some(), "expected Some(parse-error message)");
        let msg = err.as_deref().unwrap();
        assert!(
            msg.contains("could not be parsed"),
            "error should mention parse failure, got: {:?}",
            msg,
        );
        assert!(
            msg.contains("config.json.bak"),
            "successful backup must be mentioned, got: {:?}",
            msg,
        );
        assert_eq!(std::fs::read(sibling_bak(&path)).unwrap(), corrupt);
        // Must be the exact default — no partial/lenient recovery.
        let defaults = AppConfig::default();
        assert_eq!(loaded.active_provider, defaults.active_provider);
        assert!(loaded.gw2_api_key.is_none());
        assert!(loaded.gemini_api_key.is_none());
        assert_eq!(loaded.window_opacity, defaults.window_opacity);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_io_error_does_not_wipe() {
        // An unreadable config.json — a sharing violation from an antivirus
        // scan or cloud sync, a permission change, a failing disk — used to be
        // indistinguishable from a first run: load() handed back silent
        // defaults, the addon routed to the setup wizard because
        // is_setup_complete() was false, and the next save() flushed those
        // defaults over the file that still held the player's API keys.
        let dir = env::temp_dir().join(format!("gw2_config_io_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // The file a set-up player has on disk.
        let good_path = dir.join("config.json");
        let stored = AppConfig {
            gw2_api_key: Some("stored-gw2-key".into()),
            gemini_api_key: Some("stored-gemini-key".into()),
            cache_build_number: Some(12345),
            ..Default::default()
        };
        stored.save(&good_path).unwrap();
        let bytes_before = std::fs::read_to_string(&good_path).unwrap();
        assert!(
            !bytes_before.contains("save_policy"),
            "the load guard is this run's knowledge, not stored state — it must never \
             reach config.json and change the on-disk shape: {}",
            bytes_before,
        );

        // 1. A genuinely missing file is still the silent first-run path, and
        //    the defaults it returns must remain saveable. No .bak — there is
        //    nothing to copy.
        let missing = dir.join("never-written.json");
        let (fresh, err) = AppConfig::load(&missing);
        assert!(err.is_none(), "a missing config must not raise: {:?}", err);
        assert_eq!(fresh.save_policy, SavePolicy::Writable);
        assert!(
            !sibling_bak(&missing).exists(),
            "NotFound must not invent a backup"
        );
        fresh
            .save(&dir.join("first-run.json"))
            .expect("first run must still be able to save");

        // 2. An unreadable path is not a missing one. Pointing at a directory
        //    fails with a non-NotFound io::Error on every platform, the same
        //    class of failure as a Windows sharing violation.
        let blocked = dir.join("blocked-config.json");
        std::fs::create_dir_all(&blocked).unwrap();
        let (guarded, err) = AppConfig::load(&blocked);
        let msg = err.expect("an unreadable config must be reported, not silently defaulted");
        assert!(
            msg.contains("could not be read"),
            "message must say the file was unreadable, got: {}",
            msg,
        );
        assert_eq!(guarded.save_policy, SavePolicy::RefuseUnreadFileOnDisk);
        assert!(
            !sibling_bak(&blocked).exists(),
            "unread/RefuseUnreadFileOnDisk must not write a .bak"
        );

        // 3. The whole point: those in-memory defaults must not reach disk.
        //    Aim them at a path that is definitely writable, so the refusal —
        //    not the filesystem — is what protects the stored keys.
        assert!(
            guarded.save(&good_path).is_err(),
            "saving a config we failed to read must be refused",
        );
        assert_eq!(
            std::fs::read_to_string(&good_path).unwrap(),
            bytes_before,
            "stored config.json must be byte-identical after a refused save",
        );
        let (reloaded, err) = AppConfig::load(&good_path);
        assert!(err.is_none(), "unexpected error reloading: {:?}", err);
        assert_eq!(reloaded.gw2_api_key.as_deref(), Some("stored-gw2-key"));
        assert_eq!(
            reloaded.gemini_api_key.as_deref(),
            Some("stored-gemini-key")
        );
        assert!(
            reloaded.is_setup_complete(),
            "the player's setup must survive a run that could not read the config",
        );

        // 4. The reported failure itself: another process holds config.json
        //    with no sharing flags while the addon starts.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0) // deny all sharing, like an AV scanner mid-scan
                .open(&good_path)
                .expect("exclusive open of the good config");

            let (locked, err) = AppConfig::load(&good_path);
            assert!(err.is_some(), "a locked config.json must be reported");
            assert_eq!(locked.save_policy, SavePolicy::RefuseUnreadFileOnDisk);
            assert!(
                !sibling_bak(&good_path).exists(),
                "a sharing-locked file must not be copied to .bak"
            );
            assert!(
                locked.gw2_api_key.is_none(),
                "nothing was read, so nothing is known"
            );
            assert!(
                locked.save(&good_path).is_err(),
                "the locked file must not be overwritten with defaults",
            );

            drop(lock);
            assert_eq!(
                std::fs::read_to_string(&good_path).unwrap(),
                bytes_before,
                "stored config.json must survive the lock window intact",
            );
        }

        // 5. A file that was read but not parsed is a different case: saving
        //    must stay enabled so the user can repair it from the UI. The
        //    unreadable-to-serde bytes are copied to a sibling .bak first, so
        //    a later save does not destroy the only copy.
        let corrupt_path = dir.join("corrupt.json");
        let corrupt_bytes = b"{ not valid json";
        std::fs::write(&corrupt_path, corrupt_bytes).unwrap();
        let (after_parse_error, err) = AppConfig::load(&corrupt_path);
        let parse_msg = err.unwrap();
        assert!(parse_msg.contains("could not be parsed"));
        assert!(
            parse_msg.contains("config.json.bak"),
            "successful backup must be mentioned, got: {}",
            parse_msg,
        );
        assert_eq!(after_parse_error.save_policy, SavePolicy::Writable);
        let bak_path = sibling_bak(&corrupt_path);
        assert_eq!(
            std::fs::read(&bak_path).unwrap(),
            corrupt_bytes,
            "parse failure must copy the original bytes to a sibling .bak",
        );
        after_parse_error
            .save(&corrupt_path)
            .expect("a corrupt config must still be replaceable");
        assert_ne!(
            std::fs::read(&corrupt_path).unwrap(),
            corrupt_bytes.as_slice(),
            "save must replace the corrupt config.json",
        );
        assert_eq!(
            std::fs::read(&bak_path).unwrap(),
            corrupt_bytes,
            "save must leave the .bak intact",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_invalid_utf8_backs_up_then_stays_writable() {
        // Notepad UTF-16 (or any InvalidData from read_to_string) never
        // produced a Rust String, but the keys are still in the bytes.
        let dir = env::temp_dir().join(format!("gw2_config_utf16_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        // UTF-16 LE BOM + "{}" — read_to_string fails with InvalidData.
        let bytes = [0xFFu8, 0xFE, 0x7B, 0x00, 0x7D, 0x00];
        std::fs::write(&path, bytes).unwrap();

        let (loaded, err) = AppConfig::load(&path);
        let msg = err.expect("InvalidData must be reported");
        assert!(
            msg.contains("could not be parsed"),
            "InvalidData shares the parse-failure banner, got: {}",
            msg,
        );
        assert!(
            msg.contains("config.json.bak"),
            "successful backup must be mentioned, got: {}",
            msg,
        );
        assert_eq!(loaded.save_policy, SavePolicy::Writable);
        assert_eq!(std::fs::read(sibling_bak(&path)).unwrap(), bytes);

        loaded.save(&path).expect("must stay replaceable");
        assert_ne!(std::fs::read(&path).unwrap(), bytes.as_slice());
        assert_eq!(
            std::fs::read(sibling_bak(&path)).unwrap(),
            bytes,
            "save must leave the .bak intact",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_active_routing_matches_provider() {
        // active_api_key / active_model_id must route to the field matching
        // active_provider, not to whichever key happens to be set. Populate
        // all three provider slots with distinct values and flip the active
        // provider across the enum.
        let mut config = AppConfig {
            gemini_api_key: Some("gemini-key".into()),
            openai_api_key: Some("openai-key".into()),
            anthropic_api_key: Some("anthropic-key".into()),
            gemini_model: Some("gemini-custom".into()),
            openai_model: Some("openai-custom".into()),
            anthropic_model: Some("anthropic-custom".into()),
            ..Default::default()
        };

        config.active_provider = LlmProvider::Gemini;
        assert_eq!(config.active_api_key(), Some("gemini-key"));
        assert_eq!(config.active_model_id(), "gemini-custom");

        config.active_provider = LlmProvider::OpenAI;
        assert_eq!(config.active_api_key(), Some("openai-key"));
        assert_eq!(config.active_model_id(), "openai-custom");

        config.set_active_model_id("openai-switched".into());
        assert_eq!(config.active_model_id(), "openai-switched");
        assert_eq!(config.openai_model.as_deref(), Some("openai-switched"));
        assert_eq!(config.gemini_model.as_deref(), Some("gemini-custom"));

        config.active_provider = LlmProvider::Anthropic;
        assert_eq!(config.active_api_key(), Some("anthropic-key"));
        assert_eq!(config.active_model_id(), "anthropic-custom");

        // Empty slot → None key, default model id for the provider.
        let empty = AppConfig {
            active_provider: LlmProvider::Anthropic,
            ..Default::default()
        };
        assert_eq!(empty.active_api_key(), None);
        assert_eq!(empty.active_model_id(), DEFAULT_ANTHROPIC_MODEL);
    }

    #[test]
    fn theme_defaults_to_glacial_ward_on_old_configs() {
        // Pre-theme config.json: no `theme` key at all.
        let json = r#"{
            "gw2_api_key": "old-key",
            "gemini_api_key": "old-gemini-key",
            "cache_build_number": 12345
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.theme, ThemeConfig::default());
        assert_eq!(config.theme.preset, "glacial-ward");
        assert!(config.theme.custom.name.is_empty());
        // Custom defaults are the Tyrian Gold bases.
        assert_eq!(config.theme.custom.bg, [0.07, 0.06, 0.045]);
        assert_eq!(config.theme.custom.accent, [1.0, 0.84, 0.38]);

        // A theme object missing `preset` still lands on the default.
        let partial: ThemeConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(partial.preset, "glacial-ward");
        assert_eq!(partial.custom, CustomTheme::default());
    }

    #[test]
    fn theme_round_trips_through_serde() {
        let mut config = AppConfig::default();
        config.theme.preset = "custom".into();
        config.theme.custom = CustomTheme {
            name: "My Night".into(),
            bg: [0.01, 0.02, 0.03],
            panel: [0.04, 0.05, 0.06],
            accent: [0.9, 0.1, 0.2],
            text: [0.91, 0.92, 0.93],
            muted: [0.5, 0.51, 0.52],
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme, config.theme);

        let preset_only = AppConfig {
            theme: ThemeConfig {
                preset: "glacial-ward".into(),
                custom: CustomTheme::default(),
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&preset_only).unwrap();
        let back: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme.preset, "glacial-ward");
        assert_eq!(back.theme.custom, CustomTheme::default());
    }

    #[test]
    fn news_prefs_opt_in_and_old_config_stays_off() {
        let mut p = NewsPreferences::default();
        assert!(!p.any_enabled());
        p.set(NewsSource::Youtube, true);
        assert_eq!(p.enabled_sources(), vec![NewsSource::Youtube]);
        let json = r#"{"gw2_api_key":"k","gemini_api_key":"g","cache_build_number":1}"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(!config.news.any_enabled());
        assert_eq!(config.news.layout, NewsLayout::Desk);
        assert!(config.news.show_images);
        let old_layout: NewsPreferences =
            serde_json::from_str(r#"{"youtube":true,"layout":"timeline"}"#).unwrap();
        assert_eq!(old_layout.layout, NewsLayout::Desk);
        assert!(old_layout.youtube);
        assert!(old_layout.show_images);
        assert_eq!(NewsSource::Youtube.kind(), NewsKind::Video);
        assert_eq!(NewsSource::Official.kind(), NewsKind::Articles);
    }

    /// Naming a theme is what keeps it, and a second theme must be able to
    /// follow the first. Before this, the one custom slot took the new name
    /// and the "Custom" row in the picker became that name, so there was no
    /// unnamed slot left to start another from.
    #[test]
    fn naming_a_theme_keeps_it_and_leaves_room_for_the_next() {
        let mut theme = ThemeConfig::default();
        let named = |name: &str| CustomTheme {
            name: name.into(),
            ..CustomTheme::default()
        };

        // The scratch slot is not a theme until it has a name.
        theme.remember(&CustomTheme::default());
        theme.remember(&named("   "));
        assert!(theme.saved.is_empty(), "an unnamed theme is not kept");

        theme.remember(&named("Rob"));
        theme.remember(&named("Dusk"));
        assert_eq!(
            theme
                .saved
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["Rob", "Dusk"],
            "each named theme joins the list in turn"
        );

        // Editing one updates it in place rather than adding a twin: two
        // rows reading "Rob" would be indistinguishable in the picker.
        let mut recoloured = named("rob");
        recoloured.accent = [0.1, 0.2, 0.3];
        theme.remember(&recoloured);
        assert_eq!(theme.saved.len(), 2, "same name replaces, ignoring case");
        assert_eq!(theme.saved[0].accent, [0.1, 0.2, 0.3]);
    }

    /// A config written before saved themes existed must still load, and must
    /// report that it has none rather than failing to parse.
    #[test]
    fn a_theme_config_written_before_saved_themes_still_loads() {
        let legacy = r#"{"preset":"custom","custom":{"name":"Rob",
            "bg":[0.0,0.0,0.0],"panel":[0.1,0.1,0.1],"accent":[1.0,0.8,0.3],
            "text":[1.0,1.0,1.0],"muted":[0.5,0.5,0.5]}}"#;
        let theme: ThemeConfig = serde_json::from_str(legacy).expect("legacy theme loads");
        assert_eq!(theme.custom.name, "Rob");
        assert!(theme.saved.is_empty());
    }
}
