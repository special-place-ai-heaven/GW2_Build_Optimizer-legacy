//! "Additional suggestions you might like" — the closest published build
//! from each community site, shown beside whatever this addon proposed.
//!
//! Lives here rather than in either tab because both of them show
//! suggestions: New Build renders them inside its scroll child, Improve
//! below its own panes. Improve went a long time without calling any of
//! this at all, so the cards were invisible to anyone whose results land
//! there - which is everyone who asks to improve their own build.

use nexus::imgui::Ui;

use gw2_core::i18n::{t, tf};

use crate::state::AddonState;
use crate::ui::theme;

/// Match the current proposal against the synced community builds.
///
/// Cheap on every frame but the first for a given proposal: matching reads
/// every benchmark row on disk, so it runs when the proposal changes and not
/// otherwise.
pub(in crate::ui::main_view) fn refresh_provider_picks(state: &mut AddonState) {
    // The cards belong to the plate, not to whichever tab is selected:
    // opening a published build or asking a question must not re-key them
    // (specs/006 US3, in-game 2026-09-08).
    let Some(suggestion) = plate_suggestion(&state.main.comparison.suggestions) else {
        return;
    };
    // The plate's own profession, read from its specializations, so a
    // build made with no character selected still gets its cards. Taking
    // it from the selected character left the profession empty and the
    // cards absent for exactly the player who had nothing else to look at
    // (a Ritualist plate, three published Ritualist builds, no card,
    // 2026-09-07).
    let profession = state
        .main
        .game_db
        .as_deref()
        .and_then(|db| {
            gw2_optimizer::validation::infer_profession_from_spec_names(
                db,
                suggestion.specializations.iter().map(|(n, _)| n.as_str()),
            )
        })
        .or_else(|| {
            state
                .main
                .current_build
                .as_ref()
                .map(|b| b.profession.clone())
        })
        .unwrap_or_default();
    let mode = state.main.game_mode.label().to_string();
    // English, never `t()`: the key must not move when the overlay language
    // does, and nothing downstream reads these words for meaning any more.
    let role = state
        .main
        .selected_role
        .map(|r| r.label().to_string())
        .unwrap_or_default();
    let specs: Vec<String> = suggestion
        .specializations
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    // PvP has no scale to pick - conquest is always five a side and the
    // chips are hidden, so `combat_tier` is forced to Solo there and reading
    // it would wrongly favour solo-labelled references.
    let want_scale = (!matches!(state.main.game_mode, gw2_core::types::GameMode::PvP)).then(|| {
        gw2_optimizer::picks::StatedScale::from_tier(
            crate::ui::main_view::optimize_flow::combat_tier_for(
                &state.main.game_mode,
                state.main.combat_tier,
            ),
        )
    });
    // The plate's own kit, for the content tie-break. It cannot put a card
    // on the panel or keep one off it: it only orders builds the
    // measurement called equal. The archetype tie-break needs no wiring
    // here - `picks::rank` reads it off the role chip.
    let kit = gw2_optimizer::picks::Kit {
        specs: specs.clone(),
        traits: suggestion
            .specializations
            .iter()
            .flat_map(|(_, traits)| traits.iter().cloned())
            .collect(),
        weapons: suggestion.weapons.clone(),
        rune: suggestion.rune.clone(),
        relic: suggestion.relic.clone(),
        sigils: suggestion.sigils.clone(),
        stat_prefix: suggestion.stat_prefix.clone(),
    };
    let key = picks_key(&profession, &mode, &role, want_scale, &specs);
    if key == state.main.provider_picks_key {
        return;
    }
    state.main.provider_picks_key = key.clone();
    // No game data means no specialization or item names to compare, so
    // there is nothing to match on and nothing worth showing.
    let Some(db) = state.main.game_db.clone() else {
        return;
    };
    // The cards and the published tabs are NOT cleared here.
    //
    // They used to be, and the worker put new ones back a moment later.
    // Any input that flickers - the profession, which is empty until the
    // GameDb finishes loading and then is not - re-keys, and a re-key
    // emptied the panel, dropped whichever published tab the player had
    // open and threw the selection back to the plate. From the player's
    // side the cards simply vanished on a click. Nothing is thrown away
    // now until there is something to put in its place: the worker
    // replaces the list wholesale when it returns, `adopt_pick_tab`
    // replaces a tab in place by `source_url`, and stale tabs are dropped
    // there rather than here.

    let addon_dir = state.addon_dir.clone();
    let weights = state.main.weights.clone();
    let ctx = gw2_optimizer::balance::BalanceContext::new(state.main.game_mode.clone());
    let tier = crate::ui::main_view::optimize_flow::combat_tier_for(
        &state.main.game_mode,
        state.main.combat_tier,
    );
    let role = state.main.selected_role;
    let key_snapshot = key;

    // Only when there is nothing to show meanwhile. A refresh that already
    // has cards keeps them on screen rather than replacing them with a
    // progress line.
    state.main.picks_matching = state.main.provider_picks.is_empty();
    // Ranking runs the referee over every candidate - the same referee that
    // ranks the optimizer's own result - so it cannot happen during a frame.
    // Not the run token either: a Stop ends the player's request, not the
    // measuring of published builds.
    let started = state.spawn_worker_on("pick-rank", state.pick_cancel_token.clone(), {
        let token_url = key_snapshot.clone();
        move |token| {
            let builds = gw2_optimizer::scraper::load_benchmarks(&addon_dir);
            let ranked = gw2_optimizer::picks::rank(
                &builds,
                &db,
                &ctx,
                &profession,
                tier,
                role,
                &weights,
                &kit,
                &|| token.is_cancelled(),
            );
            if token.is_cancelled() {
                crate::state::with_state(|s| {
                    s.main.picks_matching = false;
                    // No cards and no reason to try again would leave the
                    // panel empty forever. Dropping the key re-keys on the
                    // next frame, which re-runs this.
                    s.main.provider_picks_key.clear();
                });
                return;
            }
            // One card per source, decided by the library so the panel, the
            // example and the corpus test cannot disagree: each site's best
            // candidate, and only if it measures near enough to be worth
            // offering at all. A build that measures like a damage build is
            // not a worse answer to a support request, it is the wrong
            // answer, and a site with nothing close says so instead.
            let (chosen, silent) = ranked.cards();
            // The scenario `picks::rank` measured in, for the tab's rotation.
            let scenario =
                crate::ui::main_view::optimize_flow::scenario_for_run(&ctx, tier, role, &weights);
            let winners: Vec<(gw2_optimizer::benchmark::BenchmarkBuild, PickNote)> = chosen
                .into_iter()
                .map(|(build, pick)| {
                    (
                        build.clone(),
                        PickNote {
                            similarity: pick.alignment.unwrap_or_default(),
                            scale: pick.stated_scale.map(|s| s.label()),
                            viable: pick.viable,
                            report: pick.report.clone(),
                            // The plate the referee ranked, flown on this
                            // worker so the tab never simulates under the
                            // state lock.
                            rotation: gw2_optimizer::benchmark::plate_from(build, &db).and_then(
                                |plate| {
                                    let v = gw2_optimizer::validation::validate_gemini_build(
                                        &plate,
                                        &db,
                                        &build.profession,
                                    );
                                    crate::ui::main_view::optimization::flow_rotation(
                                        &v,
                                        &db,
                                        &build.profession,
                                        &weights,
                                        &ctx,
                                        &scenario,
                                    )
                                },
                            ),
                        },
                    )
                })
                .collect();
            let unparsed = ranked.unparsed.clone();
            crate::state::with_state(|s| {
                s.main.picks_matching = false;
                // A newer plate re-keyed the picks while this ran.
                if s.main.provider_picks_key != token_url {
                    return;
                }
                s.main.pick_no_match = silent;
                s.main.pick_unparsed = unparsed.into_iter().collect();
                s.main.provider_picks = winners.iter().map(|(b, _)| b.clone()).collect();
                s.main.pick_notes = winners.iter().map(|(_, n)| n.describe()).collect();
                // Published tabs that are no longer picks go now, with the
                // replacements already in hand. A tab the player has open
                // and one of the new picks are the same tab: `adopt_pick_tab`
                // matches on `source_url` and overwrites in place, so the
                // strip does not shuffle under the selection.
                let keep: Vec<String> = winners.iter().map(|(b, _)| b.source_url.clone()).collect();
                let selected_url = s
                    .main
                    .comparison
                    .suggestions
                    .get(s.main.comparison.selected_suggestion)
                    .map(|sg| sg.source_url.clone());
                s.main
                    .comparison
                    .suggestions
                    .retain(|sg| sg.source_url.is_empty() || keep.contains(&sg.source_url));
                for (i, (_, note)) in winners.iter().enumerate() {
                    adopt_pick_tab(s, i, note.report.as_ref(), note.rotation.clone());
                }
                // Follow the tab the player was on, wherever it landed.
                if let Some(url) = selected_url {
                    if let Some(at) = s
                        .main
                        .comparison
                        .suggestions
                        .iter()
                        .position(|sg| sg.source_url == url)
                    {
                        s.main.comparison.selected_suggestion = at;
                    } else {
                        s.main.comparison.selected_suggestion =
                            s.main.comparison.suggestions.len().saturating_sub(1);
                    }
                }
            });
        }
    });
    if !started {
        state.main.picks_matching = false;
        // Nothing ran, so nothing will put cards here. Drop the key so the
        // next frame tries again rather than sitting on a key whose work
        // never happened.
        state.main.provider_picks_key.clear();
    }
}

/// The plate the cards belong to: the newest build this addon cooked itself
/// (empty `source_url`), whatever tab the player has selected.
pub(crate) fn plate_suggestion(
    suggestions: &[crate::ui::comparison::BuildSuggestion],
) -> Option<&crate::ui::comparison::BuildSuggestion> {
    suggestions.iter().rev().find(|s| s.source_url.is_empty())
}

/// What the cards were matched against; a change here re-runs the match.
pub(crate) fn picks_key(
    profession: &str,
    mode: &str,
    role: &str,
    scale: Option<gw2_optimizer::picks::StatedScale>,
    specs: &[String],
) -> String {
    // The scale is part of the question: changing the chips from Havoc to
    // Cloud asks for different references off the same plate, and without it
    // here the key would not move and the old cards would stay.
    format!("{profession}|{mode}|{role}|{scale:?}|{}", specs.join(","))
}

/// "Additional suggestions you might like" — the closest published build from
/// each site.
///
/// Deliberately below the proposal and visibly separate: these are not what
/// Choya cooked, they are what other people published for the same job. They
/// appear only once the player has synced, because until then there is
/// nothing to compare against.
pub(in crate::ui::main_view) fn render_provider_picks(
    ui: &Ui,
    state: &AddonState,
) -> Option<usize> {
    if state.main.provider_picks.is_empty()
        && state.main.pick_no_match.is_empty()
        && !state.main.picks_matching
    {
        return None;
    }
    // Named in the "nothing close" lines, so they read as a sentence about
    // this request rather than a bare site name.
    let requested_profession = state
        .main
        .current_build
        .as_ref()
        .map(|b| b.profession.clone())
        .unwrap_or_default();
    ui.spacing();
    ui.separator();
    ui.spacing();
    theme::wrapped(ui, theme::pal().gold, &t("cmp.also_like"));
    ui.spacing();
    if state.main.picks_matching {
        // Ranking runs the referee over every candidate, so there is a
        // visible pause. Saying what it is beats an empty panel that looks
        // like a feature with nothing to say.
        theme::wrapped(ui, theme::pal().muted, &t("pick.matching"));
        ui.spacing();
        return None;
    }
    let mut chosen = None;
    for (i, build) in state.main.provider_picks.iter().enumerate() {
        let title = if build.spec_name.is_empty() {
            build.profession.clone()
        } else {
            build.spec_name.clone()
        };
        let mut line = build.role.clone();
        if !build.gear_prefix.is_empty() {
            line.push_str(" \u{00b7} ");
            line.push_str(&build.gear_prefix);
        }
        let weapons = published_weapons(build);
        if !weapons.is_empty() {
            line.push_str(" \u{00b7} ");
            line.push_str(&weapons.join("/"));
        }
        // How near this build measured, and whether it survives its gates
        // here. No category word: the only claim the panel makes about a
        // card is the number it measured.
        let note = state.main.pick_notes.get(i);
        if let Some(note) = note {
            line = format!("{:.2} \u{00b7} {}", note.similarity, line);
            if let Some(scale) = &note.scale {
                line.push_str(" \u{00b7} ");
                line.push_str(scale);
            }
            if note.viable {
                line.push_str(" \u{00b7} ");
                line.push_str(&t("pick.viable"));
            }
        }

        // The whole card is the button: opening the build here is the point
        // of showing it, and a card that only links out sends the player to
        // a website to read what this panel could have shown them.
        let origin = ui.cursor_screen_pos();
        let width = ui.content_region_avail()[0].max(80.0);
        // `true` means muted: a gate we could not run, not one that failed.
        let caveat: Vec<(String, bool)> = match note {
            Some(n) if !n.gate_notes.is_empty() => {
                let mut lines = n.gate_notes.clone();
                if n.more_gates > 0 {
                    lines.push((
                        tf("pick.more_gates", &[("n", &n.more_gates.to_string())]),
                        true,
                    ));
                }
                lines
            }
            // The referee collapsed without naming a gate: the generic
            // sentence is all there is to say.
            Some(n) if !n.viable => vec![(t("pick.not_viable_here"), false)],
            _ => Vec::new(),
        };
        let rows = 2.0 + caveat.len() as f32;
        let height = ui.text_line_height() * rows + 10.0;
        let clicked = ui.invisible_button(format!("##pick_{i}"), [width, height]);
        let hovered = ui.is_item_hovered();
        {
            let dl = ui.get_window_draw_list();
            let p = theme::pal();
            let fill = if hovered {
                p.gold_hover
            } else {
                p.chip_idle_fill
            };
            dl.add_rect(origin, [origin[0] + width, origin[1] + height], fill)
                .filled(true)
                .rounding(4.0)
                .build();
            dl.add_rect(
                origin,
                [origin[0] + width, origin[1] + height],
                p.chip_idle_rim,
            )
            .rounding(4.0)
            .build();
            dl.add_text(
                [origin[0] + 6.0, origin[1] + 4.0],
                crate::ui::color_u32(p.gold),
                &title,
            );
            let after = ui.calc_text_size(&title)[0] + 12.0;
            dl.add_text(
                [origin[0] + after, origin[1] + 4.0],
                crate::ui::color_u32(p.muted),
                format!("\u{00b7} {}", build.source),
            );
            dl.add_text(
                [origin[0] + 6.0, origin[1] + 5.0 + ui.text_line_height()],
                crate::ui::color_u32(p.muted),
                &line,
            );
            // Shown, not hidden - and told exactly what failed. An empty
            // panel teaches the player nothing, and neither does "we cannot
            // confirm this"; the gate's own note usually can.
            for (n, (text, muted)) in caveat.iter().enumerate() {
                let col = if *muted {
                    p.muted
                } else {
                    crate::ui::comparison::DEMOTED_PICK
                };
                dl.add_text(
                    [
                        origin[0] + 6.0,
                        origin[1] + 6.0 + ui.text_line_height() * (2.0 + n as f32),
                    ],
                    crate::ui::color_u32(col),
                    text,
                );
            }
        }
        if clicked {
            chosen = Some(i);
        }

        // No link on the card. Clicking it opens the build in the panel that
        // shows builds, and the link to the site lives there, beside the
        // build it belongs to.
        ui.spacing();
    }
    for site in &state.main.pick_no_match {
        theme::wrapped(
            ui,
            theme::pal().muted,
            &tf(
                "pick.none_similar",
                &[
                    ("site", &title_case_site(site)),
                    ("profession", &requested_profession),
                ],
            ),
        );
    }
    // "We could not read these" is not "the site published nothing", and
    // the panel used to show the same silence for both.
    for (site, n) in &state.main.pick_unparsed {
        theme::wrapped(
            ui,
            theme::pal().muted,
            &tf(
                "pick.unparsed",
                &[("n", &n.to_string()), ("site", &title_case_site(site))],
            ),
        );
    }
    chosen
}

/// A source id as a page would write it: `guildjen` -> `Guildjen`.
fn title_case_site(site: &str) -> String {
    let mut c = site.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Weapon types named by a published build's gear rows.
///
/// Every site writes the weapon type into the slot label, so a row whose slot
/// is a weapon name is a weapon — see `providers::GearRow::slot`.
fn published_weapons(build: &gw2_optimizer::benchmark::BenchmarkBuild) -> Vec<String> {
    const WEAPONS: [&str; 17] = [
        "axe",
        "dagger",
        "mace",
        "pistol",
        "scepter",
        "sword",
        "focus",
        "shield",
        "torch",
        "warhorn",
        "greatsword",
        "hammer",
        "longbow",
        "rifle",
        "shortbow",
        "staff",
        "spear",
    ];
    let mut seen: Vec<String> = Vec::new();
    for row in &build.published.gear {
        let slot = row.slot.trim();
        if WEAPONS.contains(&slot.to_lowercase().as_str()) && !seen.iter().any(|s| s == slot) {
            seen.push(slot.to_string());
        }
    }
    seen
}

/// Offer the sync, but only to someone who has never done one.
///
/// Two silences look the same from here and must not be treated the same. No
/// benchmark data at all means the feature has never been available and is
/// worth pointing at. Data present but nothing close enough to show means the
/// player has already synced and the honest answer is that their build has no
/// published cousin — nagging them to sync again would be a lie.
///
/// Returns true when the player asked to go and do it.
pub(in crate::ui::main_view) fn take_sync_invite(ui: &Ui, state: &AddonState) -> bool {
    if !state.main.provider_picks.is_empty() || !state.main.benchmark_counts.is_empty() {
        return false;
    }
    ui.spacing();
    ui.separator();
    ui.spacing();
    theme::wrapped(ui, theme::pal().muted, &t("cmp.sync_for_more"));
    theme::gold_button(ui, t("btn.sync_now"))
}

/// Open a published build in this panel, as its own suggestion.
///
/// The row already holds the whole build in API ids — specializations with
/// their chosen traits, gear with per-slot stat prefixes, rune, sigils,
/// relic, skills — so nothing is fetched and nothing is guessed. Names come
/// from the same `GameDb` the rest of the panel reads, and an id the cache
/// does not know is dropped rather than shown as a number.
///
/// It becomes a suggestion beside Choya's own, which is the honest framing:
/// somebody published this for this job, here it is next to what Choya
/// cooked, compare them.
pub(in crate::ui::main_view) fn adopt_provider_pick(state: &mut AddonState, index: usize) {
    let Some(at) = adopt_pick_tab(state, index, None, None) else {
        return;
    };
    state.main.comparison.selected_suggestion = at;
    state.main.comparison.show_optimized = true;
    // Same landing as Choya's own plate: the tab where a build is actually
    // shown. Opening a build and leaving the player on the page they opened
    // it from is a click that appears to do nothing.
    state.main.active_tab =
        crate::ui::main_view::optimization::result_alert_tab(state.main.current_build.is_some());
}

/// How many failing gates a card spells out before it starts counting.
///
/// Two: a card is three lines tall already, and a build that fails five
/// gates is not made clearer by five lines of why.
const MAX_GATE_NOTES: usize = 2;

/// What the measurement concluded about one card, for the line under it.
struct PickNote {
    /// How much of what the role is for this build was measured doing.
    /// See `scoring::intent_alignment`.
    similarity: f64,
    /// The group size the page names, when it names one.
    scale: Option<&'static str>,
    viable: bool,
    report: Option<gw2_optimizer::referee::RefereeReport>,
    /// The build's `optimization::flow_rotation`.
    rotation: Option<gw2_core::types::RotationBreakdown>,
}

impl PickNote {
    /// The card's second line and its caveats.
    fn describe(&self) -> PickCardNote {
        let gates = || self.report.iter().flat_map(|r| r.viability.gates.iter());
        let labelled = |g: &gw2_optimizer::referee::GateResult| {
            format!(
                "{}: {}",
                crate::ui::comparison::viability_gate_label(&g.gate),
                g.note
            )
        };
        // Blocking gates only: the advisory ones do not make a build
        // non-viable, so naming them as the reason would be wrong. A
        // skipped gate reports `passed` so that every "list the failures"
        // caller stays correct, so it is filtered out here and added after,
        // muted - "we did not measure this" is not "this failed".
        let mut notes: Vec<(String, bool)> = gates()
            .filter(|g| !g.passed && !g.skipped && g.gate.blocks() && !g.note.trim().is_empty())
            .map(|g| (labelled(g), false))
            .collect();
        notes.extend(
            gates()
                .filter(|g| g.skipped && !g.note.trim().is_empty())
                .map(|g| (labelled(g), true)),
        );
        let failing = notes;
        PickCardNote {
            similarity: self.similarity,
            scale: self.scale,
            viable: self.viable,
            more_gates: failing.len().saturating_sub(MAX_GATE_NOTES),
            gate_notes: failing.into_iter().take(MAX_GATE_NOTES).collect(),
        }
    }
}

/// The part of a [`PickNote`] the renderer needs. The report stays on the
/// worker and is spent adopting the tab.
#[derive(Debug, Clone, Default)]
pub struct PickCardNote {
    pub similarity: f64,
    pub scale: Option<&'static str>,
    pub viable: bool,
    /// What the referee had to say about this build, as it wrote it.
    ///
    /// Failed blocking gates first as `"<gate>: <note>"`, then gates it
    /// could not run at all. The flag is true for the second kind: a gate
    /// that was skipped is not a verdict on the build, so it is muted
    /// rather than amber. Shown instead of the generic caveat, because "we
    /// cannot confirm this" tells the player nothing they can act on and
    /// the gate's own note usually does.
    pub gate_notes: Vec<(String, bool)>,
    /// Failing gates beyond the two shown.
    pub more_gates: usize,
}

/// Put a published pick on the strip as its own tab without selecting it
/// or leaving the current tab. Every card the chat shows gets a tab this
/// way as soon as it is matched (in-game 2026-09-08: a tab only appeared
/// after its card was clicked). Returns the tab's index.
fn adopt_pick_tab(
    state: &mut AddonState,
    index: usize,
    report: Option<&gw2_optimizer::referee::RefereeReport>,
    rotation: Option<gw2_core::types::RotationBreakdown>,
) -> Option<usize> {
    let build = state.main.provider_picks.get(index).cloned()?;
    let db = state.main.game_db.clone()?;
    let published = &build.published;

    let specializations: Vec<(String, Vec<String>)> = published
        .specs
        .iter()
        .filter_map(|line| {
            let spec = db.specializations.get(&line.id)?;
            let traits = line
                .trait_ids
                .iter()
                .filter_map(|id| db.traits.get(id).map(|t| t.name.clone()))
                .collect();
            Some((spec.name.clone(), traits))
        })
        .collect();

    // The slot bar, positionally: heal, three utilities, elite. Read from the
    // chat code where the site marks up no skills, which is most of them —
    // reading `skill_ids` alone left the heal and elite slots empty on every
    // GuildJen build.
    let slots = published.slot_skills(&db);
    let slot_name = |at: usize| {
        slots
            .get(at)
            .and_then(|id| *id)
            .and_then(|id| db.skills.get(&id))
            .map(|s| s.name.clone())
    };
    let mut skills: Vec<String> = Vec::new();
    if let Some(heal) = slot_name(0) {
        skills.push(format!("Heal: {heal}"));
    }
    let utils: Vec<String> = (1..4).filter_map(slot_name).collect();
    if !utils.is_empty() {
        skills.push(format!("Utils: {}", utils.join(", ")));
    }
    if let Some(elite) = slot_name(4) {
        skills.push(format!("Elite: {elite}"));
    }

    let item_name = |id: Option<u32>| {
        id.and_then(|id| db.items.get(&id))
            .map(|item| item.name.clone())
            .unwrap_or_default()
    };
    let sigils: Vec<String> = published
        .sigil_ids
        .iter()
        .filter_map(|id| db.items.get(id).map(|item| item.name.clone()))
        .collect();

    // The tab prefixes the site's name itself (`comparison::tab_label`).
    let label = if build.spec_name.is_empty() {
        build.profession.clone()
    } else {
        build.spec_name.clone()
    };
    let weapons = published_weapons(&build);
    let mut summary = build.role.clone();
    if !weapons.is_empty() {
        summary.push_str(" \u{00b7} ");
        summary.push_str(&weapons.join(" / "));
    }

    // The same build in the shape validation reads, so the stats below come
    // from the published gear rather than from the summary strings.
    let plate = gw2_optimizer::prompts::GeminiBuildResponse {
        specializations: specializations.clone(),
        weapons: weapons.clone(),
        skills: skills.clone(),
        rune: item_name(published.rune_id),
        sigils: sigils.clone(),
        relic: item_name(published.relic_id),
        stat_prefix: published
            .dominant_stat()
            .unwrap_or_else(|| build.gear_prefix.clone()),
        ..Default::default()
    };
    let mut suggestion = crate::ui::comparison::BuildSuggestion {
        label,
        build_summary: summary,
        stat_prefix: published
            .dominant_stat()
            .unwrap_or_else(|| build.gear_prefix.clone()),
        specializations,
        weapons,
        skills,
        rune: item_name(published.rune_id),
        sigils,
        relic: item_name(published.relic_id),
        // The site's own code, not one we re-encoded: it is what they
        // published and what their page hands out.
        chat_code: build.build_code.clone(),
        explanation: tf(
            "fmt.published_by",
            &[("source", &build.source), ("role", &build.role)],
        ),
        source_url: build.source_url.clone(),
        ..Default::default()
    };
    // Stats, computed the same way a plated build's are. Without this the
    // panel showed a column of zeroes beside the player's real numbers, which
    // reads as a build with no stats rather than a build we did not measure.
    let validated =
        gw2_optimizer::validation::validate_gemini_build(&plate, &db, &build.profession);
    let game_mode = state.main.game_mode.clone();
    crate::ui::main_view::optimization::attach_chat_stats(
        &mut suggestion,
        &db,
        &build.profession,
        &game_mode,
        Some(&validated),
    );
    // The ranking already refereed this build; reuse its answer rather than
    // simulating the same 60 seconds a second time.
    if let Some(report) = report {
        crate::ui::main_view::optimization::apply_referee_report(
            &mut suggestion,
            report,
            &build.profession,
            rotation,
        );
    }

    // The same card twice is one build, not two: replace the tab that
    // already holds it instead of stacking another beside it.
    let strip = &mut state.main.comparison.suggestions;
    let at = match strip
        .iter()
        .position(|s| !s.source_url.is_empty() && s.source_url == suggestion.source_url)
    {
        Some(at) => {
            strip[at] = suggestion;
            at
        }
        None => {
            strip.push(suggestion);
            strip.len() - 1
        }
    };
    Some(at)
}

#[cfg(test)]
mod plate_tests {
    use super::*;
    use crate::ui::comparison::BuildSuggestion;

    fn build(label: &str, url: &str, specs: &[&str]) -> BuildSuggestion {
        BuildSuggestion {
            label: label.into(),
            source_url: url.into(),
            specializations: specs.iter().map(|s| (s.to_string(), vec![])).collect(),
            ..Default::default()
        }
    }

    /// The cards belong to the plate. Opening a published tab, or having
    /// three of them on the strip, must not change what was matched - a
    /// changed key empties the panel, and that is what made the cards
    /// vanish on a click.
    #[test]
    fn the_key_ignores_which_tab_is_selected() {
        let plate = build("Optimized", "", &["Reaper", "Spite", "Soul Reaping"]);
        let specs: Vec<String> = plate
            .specializations
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let key_of = |strip: &[BuildSuggestion]| {
            let p = plate_suggestion(strip).expect("a plate");
            picks_key(
                "Necromancer",
                "WvW",
                "Support",
                None,
                &p.specializations
                    .iter()
                    .map(|(n, _)| n.clone())
                    .collect::<Vec<_>>(),
            )
        };

        let alone = vec![plate.clone()];
        let with_cards = vec![
            plate.clone(),
            build("Guildjen", "https://guildjen.com/a", &["Harbinger"]),
            build("Hardstuck", "https://hardstuck.gg/b", &["Scourge"]),
        ];
        // Same plate, same answer, however many published tabs sit beside
        // it and whichever one is selected.
        assert_eq!(key_of(&alone), key_of(&with_cards));
        assert_eq!(
            key_of(&with_cards),
            picks_key("Necromancer", "WvW", "Support", None, &specs)
        );
    }

    #[test]
    fn picks_key_from_newest_plate() {
        let strip = vec![
            build("A", "", &["Reaper"]),
            build("B", "https://guildjen.com/b", &["Harbinger"]),
        ];
        let plate = plate_suggestion(&strip).unwrap();
        assert_eq!(plate.label, "A", "a published tab is never the plate");
        let key_a = picks_key("Necromancer", "WvW", "Roam", None, &["Reaper".into()]);
        let mut strip = strip;
        strip.push(build("C", "", &["Scourge"]));
        assert_eq!(plate_suggestion(&strip).unwrap().label, "C");
        let key_c = picks_key("Necromancer", "WvW", "Roam", None, &["Scourge".into()]);
        assert_ne!(key_a, key_c);
        assert!(plate_suggestion(&[build("B", "https://x", &[])]).is_none());
        assert!(plate_suggestion(&[]).is_none());
    }
}
