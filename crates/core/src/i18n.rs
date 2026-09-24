//! Overlay chrome translations. Game-data names use an official `/v2?lang=` overlay
//! (`de`/`es`/`fr`/`zh`); the English GameDb stays the optimizer source of truth.
//!
//! Add a language: write `locales/xx.json` with the same keys as `en.json`,
//! then append an entry to [`LANGUAGES`] and [`SOURCES`].

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One shipped UI language.
pub struct Language {
    pub code: &'static str,
    /// Name in that language, for the Settings combo.
    pub native_name: &'static str,
    /// English name passed to Choya ("French", "German").
    pub choya_name: &'static str,
}

/// `auto` is config-only (detect OS). Real catalogs start at English.
pub const LANGUAGES: &[Language] = &[
    Language {
        code: "en",
        native_name: "English",
        choya_name: "English",
    },
    Language {
        code: "fr",
        native_name: "Français",
        choya_name: "French",
    },
    Language {
        code: "de",
        native_name: "Deutsch",
        choya_name: "German",
    },
    Language {
        code: "es",
        native_name: "Español",
        choya_name: "Spanish",
    },
    Language {
        code: "it",
        native_name: "Italiano",
        choya_name: "Italian",
    },
    Language {
        code: "pt",
        native_name: "Português",
        choya_name: "Portuguese",
    },
    Language {
        code: "nl",
        native_name: "Nederlands",
        choya_name: "Dutch",
    },
    Language {
        code: "pl",
        native_name: "Polski",
        choya_name: "Polish",
    },
    Language {
        code: "ru",
        native_name: "Русский",
        choya_name: "Russian",
    },
    Language {
        code: "zh",
        native_name: "简体中文",
        choya_name: "Simplified Chinese",
    },
    Language {
        code: "ja",
        native_name: "日本語",
        choya_name: "Japanese",
    },
    Language {
        code: "ko",
        native_name: "한국어",
        choya_name: "Korean",
    },
];

const SOURCES: &[(&str, &str)] = &[
    ("en", include_str!("../../../locales/en.json")),
    ("fr", include_str!("../../../locales/fr.json")),
    ("de", include_str!("../../../locales/de.json")),
    ("es", include_str!("../../../locales/es.json")),
    ("it", include_str!("../../../locales/it.json")),
    ("pt", include_str!("../../../locales/pt.json")),
    ("nl", include_str!("../../../locales/nl.json")),
    ("pl", include_str!("../../../locales/pl.json")),
    ("ru", include_str!("../../../locales/ru.json")),
    ("zh", include_str!("../../../locales/zh.json")),
    ("ja", include_str!("../../../locales/ja.json")),
    ("ko", include_str!("../../../locales/ko.json")),
];

static CATALOGS: OnceLock<HashMap<String, HashMap<String, String>>> = OnceLock::new();
static CURRENT: Mutex<String> = Mutex::new(String::new());

fn catalogs() -> &'static HashMap<String, HashMap<String, String>> {
    CATALOGS.get_or_init(|| {
        let mut all = HashMap::new();
        for (code, raw) in SOURCES {
            if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(raw) {
                all.insert((*code).to_string(), map);
            }
        }
        all
    })
}

fn current_code() -> String {
    let g = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_empty() {
        "en".into()
    } else {
        g.clone()
    }
}

/// Resolve `auto` or a language code to a catalog that exists.
pub fn resolve(code: &str) -> &'static str {
    let raw = code.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
        return detect_os_language();
    }
    let lower = raw.to_ascii_lowercase();
    if catalogs().contains_key(&lower) {
        LANGUAGES
            .iter()
            .find(|l| l.code == lower)
            .map(|l| l.code)
            .unwrap_or("en")
    } else {
        "en"
    }
}

/// ArenaNet `/v2` name locales. `None` = keep English GameDb names.
pub fn api_lang(ui_code: &str) -> Option<&'static str> {
    match resolve(ui_code) {
        "de" => Some("de"),
        "es" => Some("es"),
        "fr" => Some("fr"),
        "zh" => Some("zh"),
        _ => None,
    }
}

/// Apply a config value (`auto` or a code). Safe to call every frame; cheap if unchanged.
pub fn set_language(code: &str) {
    let resolved = resolve(code).to_string();
    let mut g = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if *g != resolved {
        *g = resolved;
    }
}

pub fn current() -> String {
    current_code()
}

pub fn language_by_code(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|l| l.code == code)
}

/// English language name for the Choya prompt.
pub fn choya_name_for(code: &str) -> &'static str {
    language_by_code(resolve(code))
        .map(|l| l.choya_name)
        .unwrap_or("English")
}

pub fn t(key: &str) -> String {
    let lang = current_code();
    let cats = catalogs();
    cats.get(&lang)
        .and_then(|c| c.get(key))
        .or_else(|| cats.get("en").and_then(|c| c.get(key)))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

/// Weapon-type enums stay English on the wire.
pub fn loc_weapon_type(api_type: &str) -> String {
    let lang = current_code();
    if api_lang(&lang).is_none() {
        return canonical_weapon_type(api_type);
    }
    weapon_label(&lang, api_type)
        .map(str::to_string)
        .unwrap_or_else(|| canonical_weapon_type(api_type))
}

/// Localize a `Dagger / Dagger` weapon-set label.
pub fn loc_weapon_types(joined: &str) -> String {
    joined
        .split(" / ")
        .map(|part| loc_weapon_type(part.trim()))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Letters-only lowercase key: "Short Bow" / "ShortBow" / "Shortbow",
/// "Knight's" / "Knights".
pub fn alnum_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Profession and skill APIs use Shortbow/Longbow/Spear/Speargun.
/// Item `details.type` uses ShortBow/LongBow/Harpoon.
pub fn weapon_type_key(s: &str) -> String {
    match alnum_key(s).as_str() {
        "harpoon" => "spear".to_string(),
        "harpoongun" => "speargun".to_string(),
        other => other.to_string(),
    }
}

/// Profession HashMap key for a weapon type string from any API spelling.
pub fn canonical_weapon_type(name: &str) -> String {
    match weapon_type_key(name).as_str() {
        "axe" => "Axe".into(),
        "dagger" => "Dagger".into(),
        "focus" => "Focus".into(),
        "greatsword" => "Greatsword".into(),
        "hammer" => "Hammer".into(),
        "longbow" => "Longbow".into(),
        "mace" => "Mace".into(),
        "pistol" => "Pistol".into(),
        "rifle" => "Rifle".into(),
        "scepter" => "Scepter".into(),
        "shield" => "Shield".into(),
        "shortbow" => "Shortbow".into(),
        "spear" => "Spear".into(),
        "speargun" => "Speargun".into(),
        "staff" => "Staff".into(),
        "sword" => "Sword".into(),
        "torch" => "Torch".into(),
        "trident" => "Trident".into(),
        "warhorn" => "Warhorn".into(),
        _ => name.trim().to_string(),
    }
}

fn weapon_label(lang: &str, ty: &str) -> Option<&'static str> {
    // de/es/fr/zh only — loc_weapon_type never calls this for other codes.
    let key = weapon_type_key(ty);
    let row: [&str; 4] = match key.as_str() {
        "axe" => ["Hache", "Axt", "Hacha", "斧"],
        "dagger" => ["Dague", "Dolch", "Daga", "匕首"],
        "mace" => ["Masse", "Streitkolben", "Maza", "锤"],
        "pistol" => ["Pistolet", "Pistole", "Pistola", "手枪"],
        "scepter" => ["Sceptre", "Zepter", "Cetro", "节杖"],
        "sword" => ["Épée", "Schwert", "Espada", "剑"],
        "focus" => ["Focus", "Fokus", "Foco", "聚能器"],
        "shield" => ["Bouclier", "Schild", "Escudo", "盾"],
        "torch" => ["Torche", "Fackel", "Antorcha", "火炬"],
        "warhorn" => [
            "Cor de guerre",
            "Kriegshorn",
            "Cuerno de guerra",
            "战争号角",
        ],
        "greatsword" => ["Espadon", "Großschwert", "Mandoble", "巨剑"],
        "hammer" => ["Marteau", "Hammer", "Martillo", "锤子"],
        "longbow" => ["Arc long", "Langbogen", "Arco largo", "长弓"],
        "rifle" => ["Fusil", "Gewehr", "Rifle", "步枪"],
        "shortbow" => ["Arc court", "Kurzbogen", "Arco corto", "短弓"],
        "staff" => ["Bâton", "Stab", "Báculo", "法杖"],
        "speargun" => ["Fusil-harpon", "Harpunenschleuder", "Cañón arpón", "鱼叉枪"],
        "trident" => ["Trident", "Dreizack", "Tridente", "三叉戟"],
        "spear" => ["Lance", "Speer", "Lanza", "长矛"],
        _ => return None,
    };
    Some(match lang {
        "fr" => row[0],
        "de" => row[1],
        "es" => row[2],
        "zh" => row[3],
        _ => return None,
    })
}

/// Replace `{name}` placeholders.
pub fn tf(key: &str, args: &[(&str, &str)]) -> String {
    let mut s = t(key);
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// Catalog bucket (`fmt.*_one` / `_few` / `_many`) selected by the active locale.
///
/// `slavic_plural_form` dispatches on `current()`: Germanic two-form, French
/// 0/1, Russian CLDR, Polish CLDR. ja/zh/ko strings are identical across forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlavicPluralForm {
    One,
    Few,
    Many,
}

/// Classify `n` into the CLDR one/few/many bucket for the **active** locale.
///
/// Callers (`lock_count_key`, `news_count_key`) stay language-blind: this
/// reads `current()` so English is not fed the Russian rule.
pub fn slavic_plural_form(n: u64) -> SlavicPluralForm {
    match current().as_str() {
        "fr" => {
            if n <= 1 {
                SlavicPluralForm::One
            } else {
                SlavicPluralForm::Many
            }
        }
        "ru" => {
            let mod10 = n % 10;
            let mod100 = n % 100;
            if mod10 == 1 && mod100 != 11 {
                SlavicPluralForm::One
            } else if (2..=4).contains(&mod10) && !(12..=14).contains(&mod100) {
                SlavicPluralForm::Few
            } else {
                SlavicPluralForm::Many
            }
        }
        "pl" => {
            if n == 1 {
                SlavicPluralForm::One
            } else {
                let mod10 = n % 10;
                let mod100 = n % 100;
                if (2..=4).contains(&mod10) && !(12..=14).contains(&mod100) {
                    SlavicPluralForm::Few
                } else {
                    SlavicPluralForm::Many
                }
            }
        }
        // ja/zh/ko: catalogs use the same string for every form.
        "ja" | "zh" | "ko" => SlavicPluralForm::Many,
        // en/de/es/it/pt/nl (and unknown): n == 1 ? one : many
        _ => {
            if n == 1 {
                SlavicPluralForm::One
            } else {
                SlavicPluralForm::Many
            }
        }
    }
}

fn detect_os_language() -> &'static str {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        let langid = unsafe { GetUserDefaultUILanguage() };
        match langid & 0x3ff {
            0x0c => "fr",
            0x07 => "de",
            0x0a => "es",
            0x10 => "it",
            0x16 => "pt",
            0x13 => "nl",
            0x15 => "pl",
            0x19 => "ru",
            0x04 => "zh",
            0x11 => "ja",
            0x12 => "ko",
            _ => "en",
        }
    }
    #[cfg(not(windows))]
    {
        "en"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static LANG_LOCK: Mutex<()> = Mutex::new(());

    fn with_lang<R>(code: &str, f: impl FnOnce() -> R) -> R {
        let _g = LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_language(code);
        f()
    }

    #[test]
    fn english_catalog_has_core_chrome() {
        with_lang("en", || {
            assert_eq!(t("tab.settings"), "Settings");
            assert_eq!(t("tab.choya"), "Choya Assist");
            assert_eq!(t("tab.radio"), "Choya Tunes");
            assert_eq!(t("btn.optimize_build"), "Optimize Build");
        });
    }

    /// The on-ramp reaches a first-time user in their own language.
    ///
    /// Written first to assert that German fell back to English, which was
    /// true only while German had no on-ramp. It does now — and a test that
    /// pins a fallback is a test that fails the moment somebody does the
    /// translation, which is the wrong thing to make expensive.
    ///
    /// What is worth holding is that the strings are real in every language
    /// and are not the key names leaking through, so the check is the same
    /// for all of them.
    #[test]
    fn every_language_has_its_own_onboarding() {
        const ON_RAMP: [&str; 4] = [
            "setup.ai_howto",
            "setup.ai_pick",
            "setup.openrouter_steps",
            "setup.gemini_steps",
        ];
        // Overlay fonts we ship do not draw U+2014; it rendered as '?' on the
        // Anthropic howto. Keep the on-ramp in ASCII punctuation.
        const FORBIDDEN: &[char] = &['\u{2013}', '\u{2014}', '\u{2015}'];
        for lang in LANGUAGES.iter().map(|l| l.code) {
            with_lang(lang, || {
                for key in ON_RAMP {
                    let text = t(key);
                    assert_ne!(text, key, "{lang} has no {key}");
                    assert!(text.len() > 20, "{lang} {key} is too short to be prose");
                    for ch in FORBIDDEN {
                        assert!(
                            !text.contains(*ch),
                            "{lang} {key} contains U+{:04X} which the overlay draws as ?",
                            *ch as u32
                        );
                    }
                }
                for provider in crate::config::LlmProvider::ALL {
                    for key in [provider.setup_howto_key(), provider.setup_steps_key()] {
                        let text = t(key);
                        for ch in FORBIDDEN {
                            assert!(
                                !text.contains(*ch),
                                "{lang} {key} contains U+{:04X} which the overlay draws as ?",
                                *ch as u32
                            );
                        }
                    }
                }
            });
        }
        with_lang("en", || {
            assert!(t("setup.ai_howto").contains("do not have to pay"));
        });
        // Not a fallback: German says it in German.
        with_lang("de", || {
            assert!(t("setup.ai_howto").contains("Bezahlen musst du nicht"));
            assert_eq!(t("setup.free_tag"), "(gratis)");
        });
    }

    #[test]
    fn french_translates_settings() {
        with_lang("fr", || {
            assert_eq!(t("tab.settings"), "Paramètres");
            assert_eq!(t("choya.assistant"), "Assistant de build");
            assert_eq!(t("label.build"), "Composition :");
            assert_eq!(t("section.mode"), "MODE DE JEU");
            assert_eq!(t("section.actions"), "COMMANDES");
            assert_eq!(t("pane.build"), "Composition");
            assert_eq!(t("pane.stats"), "Statistiques");
            assert_eq!(t("ranch.title"), "RANCH CHOYA");
            assert_eq!(
                tf(
                    "explain.uses_gear",
                    &[("profession", "Thief"), ("gear", "Valkyrie")],
                ),
                "Ce build Thief utilise l'équipement Valkyrie.",
            );
        });
    }

    #[test]
    fn spanish_and_dutch_translate_leftover_chrome() {
        with_lang("es", || {
            assert_ne!(
                t("note.attributes"),
                "Power and Condition Damage are damage flavors — not DPS."
            );
            assert_ne!(t("btn.sync"), "Sync Benchmarks");
            assert_ne!(t("label.stacks"), "Application (avg stacks)");
            assert_eq!(t("btn.sync"), "Sincronizar benchmarks");
        });
        with_lang("nl", || {
            assert_eq!(t("section.armor"), "RUSTING");
            assert_eq!(t("section.stats"), "STATISTIEKEN");
            assert_eq!(t("section.modifiers"), "MODIFICATOREN");
            assert_eq!(t("section.boons"), "ZEGENINGEN");
        });
    }

    #[test]
    fn missing_key_falls_back_to_english() {
        with_lang("en", || {
            let missing = t("this.key.does.not.exist");
            assert_eq!(missing, "this.key.does.not.exist");
        });
    }

    #[test]
    fn unknown_locale_is_english() {
        assert_eq!(resolve("xx"), "en");
        assert_eq!(resolve("auto").len(), 2);
    }

    #[test]
    fn api_lang_maps_official_locales_only() {
        assert_eq!(api_lang("fr"), Some("fr"));
        assert_eq!(api_lang("de"), Some("de"));
        assert_eq!(api_lang("es"), Some("es"));
        assert_eq!(api_lang("zh"), Some("zh"));
        assert_eq!(api_lang("en"), None);
        assert_eq!(api_lang("it"), None);
        assert_eq!(api_lang("ko"), None);
    }

    #[test]
    fn loc_weapon_type_french_dagger() {
        with_lang("fr", || {
            assert_eq!(loc_weapon_type("Dagger"), "Dague");
            assert_eq!(loc_weapon_types("Dagger / ShortBow"), "Dague / Arc court");
            assert_eq!(loc_weapon_type("Short Bow"), "Arc court");
            assert_eq!(loc_weapon_type("Harpoon"), "Lance");
            assert_eq!(loc_weapon_type("Harpoon Gun"), "Fusil-harpon");
        });
        with_lang("de", || {
            assert_eq!(loc_weapon_type("Harpoon"), "Speer");
            assert_eq!(loc_weapon_type("Harpoon Gun"), "Harpunenschleuder");
            assert_eq!(loc_weapon_type("LongBow"), "Langbogen");
        });
        with_lang("en", || {
            assert_eq!(loc_weapon_type("Dagger"), "Dagger");
            assert_eq!(loc_weapon_type("ShortBow"), "Shortbow");
            assert_eq!(loc_weapon_type("Harpoon"), "Spear");
            assert_eq!(loc_weapon_type("Harpoon Gun"), "Speargun");
        });
    }

    #[test]
    fn weapon_type_keys_collapse_api_spellings() {
        assert_eq!(alnum_key("Short Bow"), "shortbow");
        assert_eq!(weapon_type_key("ShortBow"), "shortbow");
        assert_eq!(weapon_type_key("Harpoon"), "spear");
        assert_eq!(weapon_type_key("Harpoon Gun"), "speargun");
        assert_eq!(canonical_weapon_type("ShortBow"), "Shortbow");
        assert_eq!(canonical_weapon_type("Long Bow"), "Longbow");
        assert_eq!(canonical_weapon_type("Harpoon"), "Spear");
        assert_eq!(alnum_key("Knight's"), alnum_key("Knights"));
    }

    #[test]
    fn tf_replaces_named_placeholders() {
        with_lang("en", || {
            let s = tf("fmt.usage_today", &[("n", "12")]);
            assert!(s.contains("12"));
            assert!(s.to_lowercase().contains("request"));
        });
    }

    #[test]
    fn plural_form_follows_active_locale() {
        with_lang("en", || {
            assert_eq!(slavic_plural_form(1), SlavicPluralForm::One);
            assert_eq!(slavic_plural_form(21), SlavicPluralForm::Many);
            assert_eq!(slavic_plural_form(0), SlavicPluralForm::Many);
            assert_eq!(slavic_plural_form(2), SlavicPluralForm::Many);
        });
        with_lang("de", || {
            assert_eq!(slavic_plural_form(1), SlavicPluralForm::One);
            assert_eq!(slavic_plural_form(21), SlavicPluralForm::Many);
        });
        with_lang("fr", || {
            assert_eq!(slavic_plural_form(0), SlavicPluralForm::One);
            assert_eq!(slavic_plural_form(1), SlavicPluralForm::One);
            assert_eq!(slavic_plural_form(2), SlavicPluralForm::Many);
        });
        with_lang("ru", || {
            assert_eq!(slavic_plural_form(1), SlavicPluralForm::One);
            assert_eq!(slavic_plural_form(21), SlavicPluralForm::One);
            assert_eq!(slavic_plural_form(2), SlavicPluralForm::Few);
            assert_eq!(slavic_plural_form(22), SlavicPluralForm::Few);
            assert_eq!(slavic_plural_form(0), SlavicPluralForm::Many);
            assert_eq!(slavic_plural_form(5), SlavicPluralForm::Many);
            assert_eq!(slavic_plural_form(11), SlavicPluralForm::Many);
            assert_eq!(slavic_plural_form(12), SlavicPluralForm::Many);
            assert_ne!(slavic_plural_form(2), slavic_plural_form(5));
        });
        with_lang("pl", || {
            assert_eq!(slavic_plural_form(1), SlavicPluralForm::One);
            assert_eq!(slavic_plural_form(21), SlavicPluralForm::Many);
            assert_eq!(slavic_plural_form(2), SlavicPluralForm::Few);
            assert_eq!(slavic_plural_form(3), SlavicPluralForm::Few);
            assert_eq!(slavic_plural_form(4), SlavicPluralForm::Few);
            assert_eq!(slavic_plural_form(22), SlavicPluralForm::Few);
            assert_eq!(slavic_plural_form(12), SlavicPluralForm::Many);
            assert_eq!(slavic_plural_form(5), SlavicPluralForm::Many);
        });
    }

    #[test]
    fn every_locale_parses_and_covers_english_keys() {
        let cats = catalogs();
        let en = cats.get("en").expect("english catalog");
        assert!(en.len() > 80, "catalog too small: {}", en.len());
        for lang in LANGUAGES {
            let cat = cats
                .get(lang.code)
                .unwrap_or_else(|| panic!("{}", lang.code));
            for key in en.keys() {
                assert!(cat.contains_key(key), "{} missing key {}", lang.code, key);
            }
        }
    }

    /// No catalog string may use a dash the overlay cannot draw.
    ///
    /// The on-ramp check above guards the `setup.*` keys, which is where the
    /// '?' was first seen. It was never only those: 327 of these dashes were
    /// spread across all twelve catalogs - role hints, the attribute notes,
    /// the cache warning, the ranch. Every one reached a player as a question
    /// mark.
    ///
    /// Whole-catalog rather than a key list, because the next one will be
    /// written by somebody who has not read this comment.
    #[test]
    fn no_catalog_uses_a_dash_the_overlay_cannot_draw() {
        const FORBIDDEN: &[char] = &['\u{2013}', '\u{2014}', '\u{2015}'];
        for (lang, cat) in catalogs() {
            for (key, text) in cat {
                for ch in FORBIDDEN {
                    assert!(
                        !text.contains(*ch),
                        "{lang} {key} contains U+{:04X}; use ASCII '-'",
                        *ch as u32
                    );
                }
            }
        }
    }

    #[test]
    fn choya_name_french() {
        assert_eq!(choya_name_for("fr"), "French");
        assert!(choya_name_for("auto").len() > 2);
    }
}
