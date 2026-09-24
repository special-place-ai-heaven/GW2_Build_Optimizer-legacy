# Changelog

All notable changes to GW2 Build Optimizer are documented here.

## 1.14.47

- Mini radio anchor: pin the strip in place so it can no longer be moved or resized and clicks and drags over its plate, equalizer, title and Choya go to the game ("Anchor in place" in the mini radio settings, or the unbound Nexus keybind `GW2_BUILD_OPT_MINI_RADIO_ANCHOR`). The controls still fade in on mouse-over, in their own small window over the strip's top row, so station, play, volume and the gear keep working; only that row takes the mouse. A faint padlock marks an anchored strip (hidden under the controls) and flashes on each flip; Choya, quips, equalizer and title keep animating; unanchor from the strip's gear, the settings gear next to Mini radio in Choya Tunes, or the keybind.
- Mini radio settings: the strip's gear and the Choya Tunes gear open the same, wider popup (460 px at font scale 1). The Bars and Gap number fields fit three digits at any font scale. "Anchor in place" sits in the last group above Reset appearance and Reset position; anchoring no longer closes the popup, so it can be undone right there. The separate Anchor checkbox next to Mini radio is gone.
- Mini radio position and size survive a game restart. The strip was clamped against whatever display the first frames reported (a 1x1 display clamps it to the top-left corner at minimum width), and any frame whose rect differed from the saved one, clamping included, wrote that rect back to config.json, so the bad spot became the saved one. Now the strip waits for a real display, then snaps to the saved rect, and only a drag or resize the player started on the strip is saved (on release). Display changes, clamping and reset never write.
- Mini radio defaults follow the reference setup: on and anchored, centred at 70% of the screen width near the bottom, about a fifth of the screen wide, transparent background, 20 bars, warm bar and title colours, Choya 1.05x, favourites cycling.

## 1.14.46

Choya reads the message before plating; a mini radio strip for when the overlay is closed. SCHEMA CHANGE = N.

- Choya reasons about the message before any optimizer work: quick-prompt chips, build words, pasted chat codes, and greetings/thanks/questions about Choya without game words are caught by rules; anything else goes to one small model call (~200 input tokens, 120 output tokens) that classifies it as build request, plate question, chat-code question, general GW2 question, or small talk. Only build requests enter the plating pipeline. Small talk gets a one-line reply with no reference build, gear ranking, or plate. Questions are answered via the game-data lookup tools (lookup rounds shown in the run feed), no plate. A question that needs a build says "I will compose a plate for that" and switches to build mode. The run feed shows "Reading your message · kind: …"; reply-only runs are recorded in Generations with no build. 14 new strings in all 12 locales. Fixes "how are you doing" running a full Necromancer build.
- Mini radio (Choya Tunes): a floating, title-bar-less strip shown only while the main overlay is closed (toggle in Choya Tunes, Nexus keybind `GW2_BUILD_OPT_MINI_RADIO_TOGGLE`); default at the bottom edge centred above the skill bar, draggable, width-resizable with height following. Controls previous / play-pause / stop / next / mute / volume, open Choya Tunes, gear popup, hide. Previous/next cycle the selected genre's station list in displayed order with wrap-around, or favourites only via a persisted toggle with a heart marker (disabled with a reason when empty). Equalizer bars, scrolling station·title, dancing Choya with quips (bubble flips below near the top edge); appearance controls for background opacity (0.35 default) separate from content opacity (1.0), per-element visibility (bars, title, Choya, quips, controls with auto-hide until hover), colours for bars/bar peaks/title/background tint with theme fallback and reset, bar count 8-48, gap 0-6, height scale. Nothing has to be playing (idle strip shows the last station and Play); position and size persist and are clamped to the screen; config under `radio.mini_radio` with defaults for old configs. 41 new strings in all 12 locales.
- Review fixes rolled in: width re-clamped on resolution change, pre-mute volume persisted, a player shut-down flag so no stream starts during unload, one state lock per frame while hidden, panic-safe alpha scope.
- The Choya Tunes tab's own Choya, title scroller, and heart animation are wall-clock timed like the rest of the overlay.

## 1.14.45

Legacy cleanup: settings that did nothing now work, text that described removed features is gone. SCHEMA CHANGE = N.

- Settings > Cache & Data "Auto-refresh on startup" now works. It was saved but never read. When it is on, setup is complete, and the live `/v2/build` id is newer than the cached data, the addon starts the same refresh as the stale-cache banner's Refresh button, once per session, with the usual live progress. It never starts while a load or refresh is running. The checkbox has a tooltip that says this.
- Setup wizard: the GW2 API key step lists only the permissions the addon uses (account, characters, builds). It no longer recommends inventories and unlocks, because no endpoint needs them. The list comes from one constant, `gw2_api::client::REQUIRED_SCOPES`. The unused `Gw2Client::validate_api_key` and `ApiError::MissingScopes` are removed.
- News: the kind filter tabs (All, Articles, Notes, Video, Guides) show their hover hints. The hint texts already existed but nothing displayed them.
- Removed 99 locale keys from all 12 languages because no code reads them: an old build-display layout, old per-provider setup help, weight preset names that are never offered in the UI, the left-rail Refresh Data button, the setup Complete screen, and old news layouts.
- Item and stat prefix names follow the game language in more places. The gear sheet prefix, rune and sigil lines, the option tab label, the benchmark reference, Saves rows, Choya's reference cards and the Generations cards now use `GameDb::loc_prefix` and `GameDb::loc_item`. Before, a prefix written without "'s" or a rune written without "Superior" stayed English. Chat codes, LLM prompts and records keep English names.
- Removed dead code: `SetupStep::Complete` (never entered), `MainState::benchmark_errors` and `locks_panel_expanded` (never read), `BuildLocks::has_any_locks`, `i18n::current_choya_name`, and the unused Gemini context builder (`optimizer::context`, `context_combo`, `weapon_hands::choya_label`). Also removed eight optimizer helpers with no callers: `traits_granting_buff`, `skills_granting_buff`, `flags_compatible`, `describe_error`, `with_combat_kind`, `get_for_attr_count`, `prefix_flavour` and `damage_flavour`, plus the `Flavour` enum.

## 1.14.44

Generation records, live run feed, Generations tab. SCHEMA CHANGE = N.

- New Build, Improve and Choya runs each write a `GenerationRecord` to `generations.jsonl` in the addon folder (append-only, torn-tail tolerant): kind, character, profession/spec, mode·scale·role and weights, LLM provider/model, token usage (prompt/completion/requests) read from Gemini `usageMetadata`, OpenAI/OpenRouter usage chunks and Anthropic `message_start`/`delta`, duration split into LLM wait and compute, tier timings, a cost estimate from a cited pricing table (`data/llm_pricing.json`, list prices, 2026-09-24) or "n/a", the produced build as a `SavedBuild` payload plus a card summary, status ok/cancelled/failed, and the run's step feed.
- Live run feed: every long-running action shows its steps as they happen (tier starts and fallbacks with reason, seeds, beam generations and evaluations, every LLM request with tokens, quota waits counting down, validation outcomes, the winners' 60 s simulation, record written), shown live in the New Build / Improve progress area and Choya's thinking bubble, kept afterward as a collapsible Run log. Every number carries a hover explanation of what it counts; the game-data line reads "Guardian: 227 skills · 108 traits (db 2311 profession skills / 999 traits)" instead of raw database totals.
- Header widget: a pill at the right end of the results tab strip shows duration · model · tokens · ≈ cost, with a tooltip for start time, tier, LLM wait vs compute, per-tier times, prompt/completion split, request count and the pricing row.
- Settings: cost estimates display in USD or EUR (ECB reference rate 2026-09-24, 1 USD = 0.8797 EUR, stored with source and date; records themselves keep USD).
- About gains a third view, Generations: a table of every run (date, type, character, mode·scale·role, LLM, duration, tokens, cost, mini build card), page size derived from window height with the pager always visible below it, filters (type, character, mode, LLM, date preset, free text), sortable columns, and an expandable per-run timeline; clicking a build card restores character, mode, scale, role and weights, measures the build through the same path Saves uses, attaches the original run's numbers, and lands on the matching tab. History loads off-thread on its own cancellation token.
- One evaluation path kept: the Generations tab and Saves both measure through `measure_validated`.
- Review fixes (in progress): move the currency lookup out of the render path (a `with_state` call inside render would deadlock the game, since render already holds the state mutex), add the euro sign to the font ranges, record failed/cancelled/reply-only Choya runs too, clear the opening-card state on cancel or panic, store step explanations as i18n keys instead of frozen text, cap on-disk steps and rotate the log at 20 MB, treat a partial provider-reported cost as "n/a" instead of an undercount, and cache filtered/sorted rows per change.
- ~150 new locale keys, translated in all 12 languages.

## 1.14.43

Multi-hit skills land their full coefficient; alignment stops outranking power on sustain. SCHEMA CHANGE = N.

- Fix: both simulators divided a multi-hit skill's API coefficient by its hit count before landing each strike, a regression since `2090739` (2026-09-07); the API value is per strike (wiki Soul Spiral 12 x 0.7 = 8.4, Whirling Wrath 7 x 0.35; the player's log lands each Perforate strike at 1784). Fixed at `simulator.rs` and `wvw_timeline.rs` landing; the scheduler already ranked with the full value. Three tests written failing-first plus a wiki-pinned Soul Spiral test; one existing fixture re-pinned (power axis 0.0266 -> 0.0530, its shroud bar strikes harder now that every hit lands full value). Measured on the player's own logs: Reaper golem 5581 -> 6903 (log 11707), Willbender golem 3403 -> 6104 (log 11343), Willbender solo duel 2786 -> 5157 (log 6582), Druid 1884 -> 1950. `calibrate` WvW every-gate 50 -> 54 of 140, ProtectedExecution 57 -> 61; PvE identical; corpus 3/3; no constants changed. Open: Unload's duplicate Damage fact lands 16 strikes; Whirling Wrath projectiles undercounted (API `hit_count` 1 vs 7 impacts).
- Fix: `c764ddf` (1.14.39) made intent alignment the second rank key above gates, burst and the radar weights, and sustain is uncapped, so with power scores squeezed by the multi-hit bug a Hearty/Sentinel Luminary tank won a WvW Roam Damage request at Power 100 % (Power 2560 -> 1108, Ferocity 0, "144 % of reference"). Alignment is now capped at `INTENT_ALIGNMENT_FLOOR` in `search_rank` so it only decides role fit; test `an_on_intent_tank_does_not_outrank_a_power_roamer_on_alignment`; new example `optimize_tank_repro` reproduces the case from the cached character. After the fix: Radiance/Zeal/Willbender, Marauder x14 + Dragon's x1, Power 2667, Ferocity 880. Follow-ups: the "vs meta" meter still compares uncapped direction scores (`benchmark.rs:681`); a "Locked: Willbender" label sat on a Luminary result; the player's current build fails the CleanseRate gate (1.0 vs 2.0 per 20 s).
- Fact-parser fixes: traited skill facts now apply for equipped traits (override indices collected first); alternative Buff facts pick one per status instead of summing (larger stacks x seconds in PvE, smaller in WvW/PvP, three-way alternatives abstain by name on the gap line); bare "Damage Increase" facts that are condition-scoped by text become per-condition modifiers (17 facts across 12 traits: Putrid Defense, Potent Poison, Amplified Wrath, Poison Master, Deadly Ambush, Acolyte of Torment, Hidden Barbs, Fell Beacon, Heartpiercer, Demonic Lore, Bloodsong, Strength of Shadows; wiki-cited PvE/WvW overrides where split, Hidden Barbs 20 % PvE / 33 % WvW-PvP) and the flow sim gains per-condition multipliers (`condition_type_mults`); seven skill-scoped "Damage Increase" traits (Power for Power 200 % PvE / 100 % WvW-PvP wiki-cited, Burst Mastery, Infinite Forge, One in the Chamber, Deadly Aim, Necromantic Corruption, Empowered Illusions) now apply only to their skills instead of the global strike multiplier (the Willbender's earlier near-match was this bug: 8741 -> 3403 before the multi-hit fix); Glyph of Alignment resolves to its out-of-form variant via new `data/form_variants.json`. Measured, Druid golem: 1656 -> 1884, condition share 0.726 -> 0.816, Poison 5.5 stacks (was double-counted 8.1), Bleeding 6.5 (3.9), Burning now appears. `corpus_matching`: the frozen Mesmer PvE Buffer/Support case re-frozen to guildjen Fractal Support 0.207 with the measured reason.
- One evaluation path for every tab: new `engine::simulate_validated_flow` builds the engine stat sheet, then runs `prepare_validated_rotation` + `simulate_flow`, exactly as the referee; addon wrappers `flow_rotation` and `measure_validated` in `ui/main_view/optimization.rs`. Every tab now measures a validated build through it: New Build / Improve tiers 1-2 (`synergy_result_to_suggestion`; the Rotation block previously showed the referee's 5 s gate window, which is why every skill read "x1" and Simulated DPS 1081), the legacy Improve tier (`optimize_flow.rs`), Choya plating (`chat_flow.rs`, which also had a hand-copied scenario literal, now `scenario_for_run`), the reference-build tabs (`provider_picks` worker), Saves (`saveload.rs`; a save whose names no longer resolve keeps its stored numbers and lists the errors in quality reasons instead of being re-priced as a partial build), and the LLM `simulate_rotation` tool (`gemini_tools.rs`; `rotation_sim_params` and its hand-built `SimParams` deleted, `duration_seconds` parameter removed, always the 60 s window, utilities capped at 3). Damage modifiers for `gemini_tools` `simulate_combat`, the synergy report, `resolution.rs` and Saves now come from the engine stat sheet (`reconstruct_damage_modifiers` deleted). Stunbreak/stability/cleanse lines come from the gate run (same run as the viability verdict); DPS and skill usage from the 60 s flow. `compute_3tier_combat` uses the referee's per-profession buff profiles for the Current column too (it already did for Optimized); tier.solo label now "Solo (gear, traits, own boons)" in all 12 locales. Boon and condition uptime lists are sorted (the top eight no longer change run to run). Tests: `tests/flow_display_parity.rs` (35 fixture builds: same DPS, skill usage and realized axes as `log_compare` and the referee), `every_tab_measures_a_build_the_same` and a CI-runnable `choya_and_optimizer_tabs_measure_alike_on_a_hand_built_db` in the addon crate. Willbender from the cache, WvW Roam: gate-window display 7489 (5 s) and hand-built path 3035 (30 s, listed skills from weapons the build does not hold) both replaced by 4052 (60 s) before the multi-hit fix.
- Improve Build results pane shows one scrollbar (two nested scrolling `ChildWindow`s removed in `tabs/improve.rs`).
- Fix: the "vs meta" meter (`benchmark.rs` `compute_benchmark_delta`) no longer divides uncapped direction scores. New `referee::meter_score` built from `search_rank`'s own keys: the capped radar score with its neglected-axis penalty, times the share of pass/fail checks passed (gates, completed sequence, landed burst), plus alignment clamped at `INTENT_ALIGNMENT_FLOOR`. The served tank read 136 % of the guildjen Roaming DPS reference before and 28 % after; the 1.14.42 ranking's tank winner 120 % -> 23 %; power builds unchanged (1.14.37 roamer 143 %, current unlocked winner 151 %, Willbender-locked winner 134 %). Test `a_tank_does_not_read_above_a_power_reference_under_power_weights`; `optimize_tank_repro` now prints the meter. Closes E25.
- Locks: the engine honours an elite-spec lock in every tier (new `tests/locks_every_tier.rs` runs `optimize_v2` beam including community seeds and seed repair, the deterministic tier and all five legacy candidates under a Willbender lock). The "Locked: Willbender" pill was stale: it reads the live `build_locks` while the run used its start snapshot, and `auto_populate_locks` refills locks after a run; fix in progress (draw the pill from the run's own snapshot). After this work the current unlocked WvW Roam Damage winner for the player's character is Zeal/Radiance/Dragonhunter Marauder; with a Willbender lock, Radiance/Valor/Willbender.
- `builder.rs` `profession_skills_for_build`: a skill that is the `flip_skill` of another skill in the same slot and specialization under a different name is no longer that slot's press (Willbender Flames 62618/62528 were winning the F1/F2 slots over Rushing Justice / Flowing Resolve on the lower-id sort). Corpus replacements: Guardian 58 (Flames -> Rushing Justice 21, -> Flowing Resolve 21, Exit -> Engage Radiant Forge 16), Engineer 17 (Deactivate -> Engage Photon Forge), Revenant 15 (Alliance Tactics -> Energy Meld). Same-name flips and core-to-elite flips unchanged.
- Damage-fact alternatives: rows sharing a label and hit count are one strike; identical rows count once (Unload lands 8 x 0.42, not 16 strikes); two values pick by mode (PvE larger, WvW/PvP smaller); three or more abstain by name; "Minimum ..." rows dropped as floors. Effulgent Stance lands 4.0 PvE / 2.1 WvW-PvP instead of 6.6 per cast. Corpus skill/mode rows changed: Ele 31, Engi 11, Guard 25, Mes 45, Necro 8, Ranger 26, Rev 25, Thief 11, War 34. Four skills with three-way rows now abstain (deal 0 pending wiki overrides): Sword of Justice, Impossible Odds, Phantom's Onslaught, Splinter Weapon (new gap E27, override fix in progress). Eviscerate "Level 1/2/3" rows still summed (different labels). Closes E24.
- Whirling Wrath: the API "Number of Impacts: 7" counts impacts across the area while the log shows ~1.75 hits per cast on one target; kept as one hit and named on the gap line.
- Rushing Justice: its "(Hit)" impact (1.5) now lands because the virtue is on the bar; the flames field is 0.22 x 5 impacts over 5 s (wiki), added as PvE `ProcEffect` records `skill:62668/62603/62648:0` value 1.1, which the flow sim cannot play yet and names on the gap line (schema lacks impacts/interval; `effect_coverage` counts them executable, an instrument overstatement, new gap E28: `effect_coverage` should count executability per engine).
- Guardian PvE records added with wiki citations: Lethal Tempo 2189 (+2 % strike and condition per stack, 6 s, 5 stacks, refresh-all, on virtue use), Virtue of Resolution 604 (Resolution 3 s), Righteous Sprint 2222 (Swiftness 5 s), Inspiring Virtue 603 (+10 % strike 6 s). Abstained by name: Tyrant's Momentum 2201 (a record cannot alter another record's value/duration; likely most of the log's +20.9 %), Righteous Instincts 1683 (crit-while-Resolution has no flow path), Justice is Blind 572 and Inspired Virtue 621 (need per-slot virtue scope), Permeating Wrath 622 (virtue passive triggers), Restorative Virtues 2197 (cooldown reduction). `effect_coverage` Guardian major executable 12 -> 13, coverage 43 -> 42. Measured (before the multi-hit fix was in that tree): Willbender golem 3403 -> 4615 (error -0.700 -> -0.593), Willbender Flames casts 26 -> 0, greatsword autos 0 -> 0.22 share; `skill_share` TVD 0.387 -> 0.410 only because the log names the impact "Rushing Justice (Hit)" while the sim credits "Rushing Justice" (0.354 name-matched; comparator alias pending). `calibrate` WvW every-gate 50 -> 49 (Power Virtuoso Roaming Assassin now fails ProtectedExecution on WvW coefficients; Evoker Roaming Bruiser and D/D Thief Havoc Assassin now pass); PvE identical.
- Wiki-cited per-mode overrides (`damage_coefficient:above_50` mechanism, `balance_overrides` 2026-07-15 files) for three of the four skills abstaining on E27: Impossible Odds 27107 (PvE 0.65 / WvW 0.55 / PvP 0.45), Phantom's Onslaught 62895 (1.6 / 1.33 / 1.18), Splinter Weapon 76975 (0.4 / 0.25 / 0.5). Sword of Justice 9168 (wiki 0.8 / 0.45 / 0.72 per hit x 4 hits) still abstains because the override format has no hit-count field; test `sword_of_justice_still_abstains_hit_count_not_expressible`. Override entity count 39 -> 48. E27: 3 of 4 closed, Sword of Justice needs a hit-count field in the override format.
- Fix: the Improve results lock pill now shows the lock the run actually used (`ComparisonState.run_locked_spec`, set from the run's start snapshot, cleared on every list reset and on Choya pushes) instead of the live locks, so a result produced without a lock is never labelled "Locked". Test `unlocked_run_shows_no_pill_even_after_live_lock_is_set`. Closes E26.
- Engine gap E18 (weapon choice) still open. New gaps: E23 multi-hit landing fixed and Unload's duplicate fixed, but Whirling Wrath's impact count remains (`builder.rs`).

## 1.14.42

Auto-attack chains play out. SCHEMA CHANGE = N.

- Engine gap E11 closed: auto-attack chains cycle in the flow sim and the WvW timeline. `engine::add_weapon_skill_ids` follows each weapon skill's `next_chain` and adds the later steps; `rotation::AutoChain` derives each step's place from the API `next_chain` data; both simulators play the chain's next step after an auto and reset to step 1 after any non-auto cast, an interrupted cast, or a gap longer than `CHAIN_CONTINUE_SLACK_MS` (reaction delay 180 ms + one 100 ms tick; the wiki Chain page gives no time figure, only the interrupt/other-skill reset rule, cited in the constant's doc). A chain whose next step is missing from the db lands on the gap line as "<name> auto chain (step N missing)"; none do today (118 chained skills all resolve). Four new simulator tests; no re-pins.
- Measured: Reaper golem `skill_share` TVD 0.477 -> 0.355 (Life Slash 0.144 and Life Reap 0.105 now cast; log 0.064 / 0.056), `dps_engaged` error -0.556 -> -0.529, `burst_peak_5s` unchanged at 8989 (each non-auto between autos resets the chain, as in the log). Druid and Willbender rows unchanged (no chained autos cast). Fixture Reaper `skill_share` 0.549 -> 0.557: the sim over-uses greatsword autos that the log never casts, a weapon-choice gap, not a chain gap. `calibrate` WvW/PvE gate counts unchanged; `effect_coverage` identical.
- Data: WvW record `trait:892:0` (Fear of Death) gains its wiki 5s internal cooldown (the cooldown applies to the life-force gain in every mode). Closes the first half of gap E17; Dhuumfire per-spec values still need an elite-spec gate.
- `log_compare` `codes.json` entries may be an object `{"code": "[&...]", "gear": {armor, weapons, trinkets, sigils: {"Greatsword": ["Hydromancy","Rage"], ...}, rune, relic, food, utility}}`, every key optional, unknown keys are a parse error. New `Provenance::Stated` for gear; precedence Account (addon cache) > Stated > Corpus; mixed groups print like `Stated(armor,weapons,sigils)+Corpus(trinkets,rune,relic)`. Prefixes resolve through `db.itemstat_by_name`; sigils/rune/relic by name with or without the "Superior Sigil of" / "Superior Rune of the" / "Relic of the" part; unresolvable names abstain by name in the stat flags. The stated prefix also steers the corpus neighbour choice. Fixtures README documents the format.
- Measured with the player's stated Willbender kit (Dragon's armor and weapons, Marauder trinkets, Hydromancy+Rage greatsword, Bloodlust pistol, Fire focus, Scholar runes, Relic of the Brawler): golem 20260923-204811 `dps_engaged` sim 8306 -> 7863 (log 11343; error -0.27 -> -0.31, the corpus guess had been Berserker gear), `burst_peak_5s` 16146 -> 14921 (log 16769), `burst_overlap_share` 0.02 (log 0.31), Quickness self 0.10 (log 0.38); WvW 221546/221837 `dps_engaged` 2416 -> 3364 (log 4149/4150), `burst_peak_5s` 3861 -> 5462 (log 10053/8699), overlap 0.06 (log 0.67/0.80). Real traits plus real gear leave a pure engine gap: Quickness upkeep and trigger records for a build without a form (E15), and consumables.
- New engine gap E19: the item download filters out type Consumable, so stated food/utility (Superior Sharpening Stone, +100 Power/+70 Ferocity food) cannot resolve and abstain, though the stat sheet already has a food/utility path (`consumables.rs`).
- Item download keeps Consumable items of detail type Food or Utility at level 80 (340 items on this machine: 263 Food, 77 Utility; `items.json` 17296 -> 17636 rows, build stamp unchanged). New `ItemDetails.description` carries the tooltip. Refresh runs a one-off consumable backfill when `items.json` predates this and has no Food/Utility row (`download.rs` `download_steps` after the items step; ~285 bulk requests, 30-90 s, cancellable between request groups, progress shown as "Items (food and utility)"); it never repeats once rows exist. Example `backfill_consumables` for a manual run. Closes gap E19.
- `consumables.rs` parses tooltip lines: "+N Stat" flat bonuses; "Gain X equal to P% of your Y" becomes a `StatConversion` applied on the stat sheet (Superior Sharpening Stone: Power +3% of Precision and +6% of Ferocity); experience/magic find/karma/gold lines ignored; triggered or unrecognised lines named in a flag "stated food line '...' not modeled". Known shortcut: conversions read the stat sheet after trait conversions. The optimizer's automatic food/utility pick does not score conversions yet (new gap E20: optimizer food/utility selection ignores `StatConversion` consumables, and conversions apply after trait conversions rather than before).
- `kit.rs`: stated food/utility resolve by exact name, then unique case-insensitive substring; ambiguous text ("Sharpening Stone" alone) is refused with a named flag.
- Measured, Hammerhand golem 20260923-204811: `dps_engaged` 7863 -> 8741 (log 11343, error -0.31 -> -0.23), `burst_peak_5s` 14921 -> 16647 (log 16769, within 1%). "Not in game data" flags across all own logs 12 -> 0. `calibrate` gate counts unchanged; fixture run identical.
- The trait-record loader moved to `engine.rs` (`equipped_trait_records`, `flow_record`, `trait_procs_for_build`); it reads record fields only. `form_for_build` takes only form-owned records (enter, exit, in-shroud periodic, while_in). `SimParams` gains `triggered`, `strike_add`, `condition_add`, `folded`. Triggered records fire for every build, with or without a form; the in-shroud requirement is an `in_form` gate only forms meet. Hosted triggers: on skill use (scoped), on condition applied, on hit (landed strikes), periodic. `FormSpec::triggered` is gone. Closes gaps E15 and E16.
- Two proc kinds: Condition (through the skill condition path, duration and stack cap, fires on-condition records, nesting capped at 2) and Modifier (timed stack with expiry, `max_stacks`, refresh-all per record). Modifiers are additive or multiplicative per `modifier_buckets.json`; crit damage uses the formula ferocity-per-point. Always-on shares the fact parser already folded (Soul Barbs +10%, Death Perception crit damage) are tracked by `TraitStanding` and removed once in the flow sim so nothing counts twice. WvW timeline untouched.
- Abstains by name on the gap line as "<name> <trigger> (flow sim: reason)": on-crit (the flow sim averages crits), on-dodge, on-attunement-swap, on-boon-applied, on-disable-foe; records with gates, scale, foe/attunement prerequisite, proc chance, trait-skill cast or non-player actor; heal/cleanse/strip payloads; modifiers without duration; life-force or shroud records on a build with no form. PvE has no gap line yet (residue: add one).
- Measured: Reaper golem `dps_engaged` 5519 -> 5572, `burst_peak_5s` 8989 -> 9381 (Dread +20% after fear); WvW Willbender 221837 Resolution 0.075 -> 0.218 (log 0.627), Swiftness 0 -> 0.24 (log 0.366). Willbender PvE rows unchanged: `pve.json` has no Guardian records. The player's Willbender traits (decoded with `build_template::decode`): Justice is Blind, Inspiring Virtue, Virtue of Resolution, Inspired Virtue, Permeating Wrath, Righteous Instincts, Lethal Tempo, Restorative Virtues, Tyrant's Momentum, Righteous Sprint; none grants Quickness or Fury by its API facts, so the Willbender's self Quickness 0.38 / Fury 0.34 on the golem come from the skill side (under investigation).
- `corpus_matching`: the frozen Revenant WvW Buffer case re-frozen to hardstuck "Zerg Boon DPS" with the measured reason (boon axis 0 -> 0.171, alignment -0.012 -> +0.088 once its trait boon records fire; "Zerg Support" measures 0 on every axis except sustain before and after). Workspace tests 2191 passed; `calibrate` gate counts identical; `effect_coverage` identical.
- Records: `skill:29965:0` "Feel My Wrath!" self Quickness +3s on cast (the API lists 3s; the skill doubles it for the guardian; the player's 23 WvW casts and the golem log all show 6s x boon duration, no mode split on the wiki) and `sigil:24561:0` Superior Sigil of Rage (on crit, 20s cooldown, self Quickness 3s, gate `SelfBoonAbsent{Quickness}`), in pve/wvw/pvp files with wiki citations. New gate `SelfBoonAbsent` in the record schema (documented in `optimizer-data-schemas.md`).
- Flow sim: boons marked duration-stacking in `boons.json` extend the running instance up to their cap (before, a second Quickness added nothing); new `OwnCast` and `Crit` triggers (crit accumulates per-hit crit chance and fires at one proc; on-crit records play only with a cooldown of 5s or more, shorter ones abstain by name); bar-skill and socketed-sigil records load (`sigil_seats` helper; a sigil fires only while its set is held); self-boon gates play.
- Measured: Willbender golem self Quickness 0.100 -> 0.350 (log 0.376), `burst_overlap_share` 0.021 -> 0.247 (log 0.308), `dps_engaged` 8741 -> 9840 (log 11343), `burst_peak_5s` 16647 -> 21348 (log 16769, now +27% over); WvW Willbender 221837 Quickness 0.10 -> 0.30, overlap 0.045 -> 0.230 (log 0.796). Reaper golem 5572 -> 5668. `effect_coverage` sigils 7 -> 8, elite skills 0 -> 1. `corpus_matching`: Necromancer WvW Disabler re-frozen to hardstuck "Zerg Power DPS" (control 0.320 -> 0.363 with Quickness duration stacking; Zerg Support unchanged 0.348). New gaps E21 (WvW timeline runs boons side by side, duration-stacking records add nothing there) and E22 (WvW Reaper rows over-produce Quickness, Lucian Lord 0.785 -> 0.997 vs logs 0.2-0.6).

## 1.14.41 - 2026-09-23

Radio without skips; the comparator reads your own gear. SCHEMA CHANGE = N.

- Choya Tunes: decoding moved off the audio output callback onto a decode thread that keeps about 2.5 s of PCM queued; the callback only reads memory. Root cause of the jerking/skipping: rodio pulled the decoder, and through it the network reader, ICY parser and equalizer tap, inside the output callback, so any blocked read was an audible underrun. Stall now means the decoder ran dry; one quiet reconnect, pause/resume and the unload budgets unchanged. A `radio: audio queue low-water` log line reports dips at most once a minute.
- Fidelity comparator: kit reconstruction takes gear, rune, sigils, relic, traits and skills from the player's own cached character tabs (provenance `Account`) before falling back to a published build; neighbour choice ranks by stat nearness; new flags for chat-code vs build-tab disagreement.
- Flow simulation enters forms as data-driven timed states: Necromancer shroud (`data/formulas/shroud.json` rows now tagged by elite spec; a Reaper previously got Death Shroud's drain) and Druid Celestial Avatar (new `data/formulas/forms.json` from the wiki Astral force page: 0.75% per strike, 50% kept on early exit, entry at the API cost of 100, drains over the API 15s duration, recharge PvE/WvW 10s, PvP 15s). `SimParams.form: Option<FormSpec>`; the form bar replaces the weapon bar, utilities stay usable, weapon swap is blocked. Enter when off recharge, at or above the entry floor, and the pool is full or the form's best skill beats the weapon bar; exit when the pool empties or when every form non-auto is recharging and a ready weapon skill beats the form. Pools that persist out of combat start full.
- Trait records with `OnShroudEnter`/`OnShroudExit` and in-shroud `Periodic` triggers fire in the flow sim for the equipped traits (boons and life force only). A form entry with a bar but no pool row lands on the gap line by name.
- `builder.rs`: `shroud_bar_for_build` became `form_bar_for_build`; drops underwater twins per bar slot (a slot holding a `NoUnderwater` skill drops its other skills), drops off-chain flips, maps `Downed_N` slots to weapon slots; F-slot selection skips a form's own exit skill. A control effect is added when a `status_duration_ms` override exists and the API supplied no fact.
- Balance overrides from the wiki: Executioner's Scythe (coefficients 4/6/8 in PvE, 1.25s activation, 30s recharge; the fact parser was summing all three threshold rows), Devouring Cut (coefficient PvE 1.0, WvW/PvP 0.85, 1s activation, recharge 8s, PvP 10s), Voracious Arc (coefficient PvE 1.4, WvW/PvP 1.0, 0.75s activation, recharge 18s WvW else 10s, 0.5s daze).
- Harbinger row in `shroud.json` names its unmodelled mechanics (blight, Corrupted Talent entry without life force) and they reach the gap line.
- `setup_priority` no longer gives self-cover (Stability, Blind, Aegis, stealth) opening priority in the flow sim, because the flow dummy never attacks; the CONN-01-05 2s window has DPS > 0 again.
- `corpus_matching`: Ranger refusal budget tightened (PvP 2 -> 0, WvW 4 -> 2); the frozen `similar_cards_only` case for the Necromancer WvW Disabler now records the measured reason (Harbinger weapon bar stowed in shroud drops control 0.288 -> 0.194, alignment 0.353 -> 0.286; Scourge Zerg Support scores 0.348).
- Measured on the player's own golem logs (main tree, forms and the Account-provenance comparator together): Reaper `dps_engaged` error -0.716 -> -0.589, `skill_share` TVD 0.647 -> 0.478; Druid -0.863 -> -0.774, `skill_share` TVD 0.716 -> 0.205 (Account gear provenance and Celestial Avatar landing together). Reaper self Fury/Quickness unchanged at 0.38 because the PvE record file has no shroud-trigger records. `calibrate` WvW viable 116 -> 118, StabilityAccess 136 -> 138, nothing fell.
- `log_compare` gained five observables per player: `burst_peak_5s` and `burst_peak_10s` (peak damage in any 5s / 10s window as DPS; log side from EI `damage1S` cumulative totals, sim side a new per-second capture in the flow simulation), `burst_overlap_share` (share of damage dealt in seconds where Quickness and Fury are both present; log side samples `buffUptimes[].states` at each second's midpoint), `condition_share` (`dpsAll actorCondiDamage / actorDamage`, minions excluded; the older `condi_fraction` counted pet damage and stays as is), `condition_ramp_s` (seconds from first damage until condition damage per second first reaches 80% of its median; abstains when `condition_share` < 0.2). Each side abstains by name when it lacks the data; a missing log value prints NaN, never 0.
- `SimulationResult` carries per-second strike/condition damage and per-second boon presence; scheduling never reads them; a test checks per-second sums equal the totals.
- Measured on the player's Reaper golem log: `burst_peak_5s` 18856 log vs 8989 sim (-0.523), `burst_peak_10s` 15392 vs 7131 (-0.537), `burst_overlap_share` 0.888 vs 0.496 (-0.391); the log's best five seconds run 1.6x its engaged average. Druid golem: `condition_share` 0.921 vs 0.726, `condition_ramp_s` 9 vs 7, overlap 0 on both sides (the log has no Quickness). WvW Druid 210742: `burst_peak_5s` 4652 vs 2424, `condition_share` 0.891 vs 0.755, ramp 6 vs 10.
- The three committed fixtures were trimmed before these fields existed, so overlap, `condition_share` and ramp abstain on them; a fixture refresh is in progress.
- Necromancer PvE shroud and fear trigger records: 30 records added to `data/normalized_effects/2026-01-13/pve.json`, each citing its wiki line (shroud enter x11, shroud exit x5, in-shroud x6, shroud skill 1 x3, on-fear x4, on-condition-removed x1). Finding: on the player's own Reaper, Fury and Quickness come from Dread on fear and the "Chilled to the Bone!" shout, not the shroud traits. `engine::form_for_build` also loads scoped `OnSkillUse`/`OnConditionApplied` records (boons and life force only) into `FormSpec::triggered`; `fire_triggered` runs on cast, on condition applied, and on Fear/Taunt landed; `skill_scope_admits` is shared with `wvw_timeline.rs`. Measured, Reaper golem: `dps_engaged` 4814 -> 5197 (error -0.589 -> -0.556), Fury 0.383 -> 0.705 (log 0.993), Quickness 0.383 -> 0.515 (log 0.830), Might 6.6 -> 8.0 (log 19.0), `burst_overlap_share` 0.496 -> 0.622, `burst_peak_10s` 7131 -> 7300. Side effect: Druid WvW self Fury 0.00 -> 0.30 from the existing WvW Ranger Survival-skill record. `calibrate` WvW ProtectedExecution 55 -> 56, nothing fell.
- Engine gaps named for the next increment: triggered records ride on `FormSpec` so a build with no form (Scourge, core) fires none of them in the flow sim (E15); the flow sim ignores timed damage modifiers and condition-applying procs (E16); the WvW record file is missing Fear of Death's wiki recharge and applies the Scourge Dhuumfire values to every Necromancer for lack of an elite-spec gate (E17). Named on the gap line: Soul Comprehension, Unholy Sanctuary, Soul Eater, Relentless Pursuit, Vital Persistence, Gluttony, Soul Battery, Sinister Shroud, Shroud Knight, Spiteful Spirit, Weakening Shroud, the Scourge/Harbinger Dhuumfire variants, and the Harbinger shroud traits.
- Fidelity fixtures re-downloaded from dps.report and re-trimmed with `log_compare --trim` to carry `damage1S`, `conditionDamage1S`, `buffUptimes[].states` and `dpsAll` actor damage splits (66 -> 71 KB, 160 -> 182 KB, 173 -> 198 KB); the golem test now pins `burst_overlap_share` 1.0, `condition_share` 0.0029, `condition_ramp_s` 11 as facts of the file. Two new WvW squad logs measured (Willbender roaming on a corpus-neighbour kit, no chat code): `dps_engaged` error -0.658 and -0.701, `burst_overlap_share` 0.667/0.171 and 0.796/0.162; melee power players undershoot while Dragonhunter/Berserker overshoot, because every simulated hit lands on a target present 27% of the time and group boon totals aren't yet a scenario input.

## 1.14.40 - 2026-09-23

Sprint 008 Gate 1, increment 2: minor-trait effect records for Elementalist, Engineer, Guardian and Ranger. SCHEMA CHANGE = N.

- 110 new WvW effect records (779 -> 889), every one citing its wiki line with WvW numbers; the four professions have no unrecorded minor trait left (executable 34 -> 95 across all minors; remaining coverage blocks each name the trigger or field the format lacks).
- Fix: a trait's always-on Percent fact and its executed Conditional record were both applied in the WvW timeline (Radiant Power, Symbolic Exposure, Pyromancer's Training, Shaped Charge); the trait's share is now divided out, like rune clauses.
- `effect_coverage` prints per-profession executable / abstaining / coverage / none counts and, given a profession, one line per trait with its abstain reason.
- Sprint plan records the engine gaps the review measured (records counted executable that the timeline never runs) as the next increment.
- Fidelity harness: `fidelity/` reads Elite Insights combat logs, rebuilds each squad player's kit with provenance, runs it through the addon's own path and prints per-observable error bands; `log_compare` example, `fidelity_logs` suite, three trimmed log fixtures; fight profiles (target availability, hit rates, incoming pressure) extracted from real WvW fights. First baseline: the flow simulation never enters shroud for a golem Power Reaper (12.5k vs 42.5k), and WvW damage overshoots because every hit lands; both recorded in the sprint plan as the next engine work.
- Snow Crows scraper captures the published benchmark DPS and the dps.report log link per build.

## 1.14.39 - 2026-09-22

Reliable matching and an honest simulator. SCHEMA CHANGE = Y (objective profiles gain per-scale `intent` rows; benchmark rows gain `benchmarks_synced`; new `data/stunbreak_sources.json`).

- Intent is directional: every objective profile declares focus and avoid axes per scale (solo/party/squad); `scoring::intent_alignment` with one calibrated floor replaces every label- or cosine-based rule.
- Provider cards: shown on the Improve tab; chosen by measured alignment only, one per site, none when nothing is close; persist across tab switches; failing gates named on the card; abstentions shown muted.
- vs-meta meter: both sides refereed under the same weights and simulator; reference chosen by the same alignment rule; viability caveat when either side fails a gate.
- Improve never serves an off-intent or non-viable result over a viable current build; an unavailable baseline is explained, not hidden.
- Meta-seeded search: aligned published builds enter the beam as seeds (locks respected).
- Referee: real measured axes for refused builds on the ranking path; `ranked_direction_score`; the panel renders the referee's viability (no divergent duplicate).
- Ammo skills amortise `Count Recharge / Maximum Count`; healing and boon axes count ally-facing output only; WvW rank keys run at every scale.
- Resource models per wiki: Revenant legend swap and upkeep, Mesmer illusions from API categories, Bladesworn Flow, Warrior adrenaline by bars, Thief Preparedness; unmodelled resources abstain by name.
- Gates: ResourceLegality is a starvation ratio with an unpayable hard fail; CleanseRate lets off-bar rate satisfy the floor; StunbreakCount reads descriptions and a wiki-catalogued table.
- Validator: flat published weapon lists packed by hand; elite weapons legal without the spec (Weaponmaster); PvP sigils/runes filed structurally. Published-build rejects: WvW 57->5, PvE 192->16.
- Corpus acceptance suite: 740 synced builds as fixtures; card outcomes, refusal and unplatable budgets per profession, ratcheting; one shared `ScenarioSpec::for_request` for addon, tests and examples.
- UI: Stop on the Improve banner; reference tabs fully evaluated off the render thread.
- Docs: `docs/doctrine.md`, `docs/sprints/008-data-driven-simulator.md`.

## 1.14.38 - 2026-09-22

Superseded by 1.14.39 the same day; changes folded in above.

## 1.14.37 - 2026-09-22

Full-code-review remediation FCR-20260922T145300Z-ccc85e1: 10 findings closed. SCHEMA CHANGE = N.

- WvW: `OnConditionApplied` fires on every application, including at the stack cap (Vulnerability at 25, cap-1 controls while live).
- Sim: cap-1 conditions (Chilled, Crippled, Weakness, Blinded, Slow, Immobile, Fear, Taunt, Daze) refresh duration on re-apply on both surfaces.
- WvW: endurance dodge is reactive; endurance is spent only when an enemy strike lands inside the evade window, so `avoided_damage` credits dodges on the production profile.
- WvW: Weaver secondary attunement satisfies skill prerequisites (`AttunementState::is_attuned`).
- WvW: `OnThreshold` re-arms once health recovers above 50%; record ICDs still bound re-emits.
- Modifiers: Sigil of Impact's `+7% vs. Stunned or Knocked-Down` half is deferred behind the Disabled gate; only the 3% is additive.
- Modifiers: a second sigil's name fallback (Force, Bursting) applies even when the first sigil parsed something.
- Choya `simulate_rotation`: `skill_ids` capped at 64 and `trait_ids` at 36.
- TriggerBus `drain`/`pending`/`BusEmission` are test-only; no silent empty stubs in production builds.
- Workspace `cargo fmt --check` clean (`packed_traits` reflowed).

## 1.14.36 - 2026-09-20

Full-code-review remediation FCR-20260920T081033Z-3eafccc: 20 findings closed. SCHEMA CHANGE = N.

- Sim: condition tick at shared expiry keeps Vulnerability/deferred modifiers (tick-only inclusive query; strikes stay exclusive).
- WvW: skill/op condition applies respect the stack cap; foe prerequisites match aliases and case; auto-dodge grants Evade against a strike due on the dodge tick.
- Sim: aliased and ApplyBuff-shaped foe conditions now appear in `condition_uptime`.
- Weaver secondary attunement enabled via `SimParams.weaver` (API spec 56); flow sim performs one setup attunement swap per run.
- Choya `simulate_rotation` threads vs-target trait modifiers (`trait_ids`).
- Deferred vs-target percents: PvE/competitive pair collapse, deferred-only upgrade text parsed once, per-sigil dedupe.
- Perf: TriggerBus payloads test-only; trigger_procs skip paths allocation-free with tracing off; interned condition names, single-pass soft-control weight, no per-apply String.
- clippy: `never_loop` in validation.rs fixed; workspace clippy clean.

## 1.14.35 - 2026-09-15

Choya plates always include both weapon sets. SCHEMA CHANGE = N.

- Prompt and kitchen brief: if weapons stay the same, still write set1 and set2 from Character.
- `fill_holes_from_loadout` copies a missing set from the equipped loadout instead of leaving Set 2 empty.

## 1.14.34 - 2026-09-15

Chat-code plates with only Set 1 now get a second land set. SCHEMA CHANGE = N.

- Validator fills a legal two-hander the elite can use (Herald Sword/Axe → Hammer, not Vindicator Greatsword).
- SotO trailer then has both kits; the game cannot reconstruct two sets from Sword+Axe alone.

## 1.14.33 - 2026-09-15

Build-template chat codes: one encoder/decoder, wiki layout. SCHEMA CHANGE = N.

- Palette ids in the `u16` slots (3875 ≠ skill 21750). `encode`/`decode` are inverses of the same bytes.
- Revenant copies land palettes into empty aquatic slots. Same non-zero palette twice in one realm is refused.

## 1.14.32 - 2026-09-15

API status chip shows the live `/v2/build` id. SCHEMA CHANGE = N.

- Header reads `API ready · {build}` from the API instead of "Balance data verified for …".
- Combat-snapshot vs live build no longer paints a yellow header after a successful refresh.

## 1.14.31 - 2026-09-15

Wiki per-hand weapon legality and combo matrix. SCHEMA CHANGE = N.

- Per-hand wiki table (`data/weapon_hands.json`) replaces API one-spec-per-weapon-type for validation, search, and Choya.
- Validator checks Mainhand vs Offhand (Guardian Sword off = Willbender; Ranger Dagger main = Soulbeast / off = core). Herald dual swords stays legal.
- Beam and synergy generate wiki-legal same-type dual wield (Sword/Sword).
- Choya profession dump is per-hand with elite / SotO / JW labels; combo field×finisher matrix and profession finisher gates go in the prompt.

## 1.14.30 - 2026-09-14

NeedsMechanic Engine E4: Mesmer IllusionState (clones) + TriggerBus OnCloneCreated. SCHEMA CHANGE = N.

- NEW `rotation/illusion.rs` `IllusionState { count, cap }` sole clone-count authority. Default count=0, Cap=3. Shared by flow sim and WvW timeline. `spawn` / `consume` only writers; `spawn_clone` emits OnCloneCreated iff count rose. Cap no-op does not emit.
- One new bus event/rule: `OnCloneCreated` on the existing TriggerBus. Optional `EffectCategory::SpawnClone` on normalized effects (catalog class). No OnShatter / OnPhantasm / OnCloneConsumed / blades / mirage / distortion / continuum.
- Executable records: Deceptive Evasion (OnDodge -> spawn_clone), Ego Restoration (OnSkillUse + Slot Heal -> spawn_clone), Compounding Power (OnCloneCreated -> +2% strike / +2% condi damage 8 s WvW, max 5 stacks; Virtuoso blades banked). Sharper Images (710) stays NeedsMechanic: clones.
- ValidatedBuild SCHEMA = N; TriggerBus += OnCloneCreated only. Weapon-skill clone summons stay residual; no `SkillEffect::SpawnClone`.
- Kent causal micro-proof: dodge 0->1 + emit; 4th spawn at cap = no-op/no emit; heal-slot Ego Restoration; 723 buff only on successful spawn; 710 still NeedsMechanic; >=3 Mes clone traits executing.

## 1.14.29

### Fixed
- clippy `redundant_closure` in Stats fill-mode i18n map (SCHEMA=N).
## 1.14.28 - 2026-09-13

UX strings for durable items fill modes. SCHEMA CHANGE = N.

- Setup Data Download and Settings Refresh progress show distinct copy for `items_fill_kind`: FirstFill, Resume, SameBuildSkip.
- `None` (normal refresh / Verify / build mismatch) keeps existing downloading/Refreshing labels.
- Presentation only; download and cache semantics unchanged. No RefreshDelta panel.

## 1.14.27 - 2026-09-13

Durable first-fill resume for the items catalog via `items.partial`. SCHEMA CHANGE = N.

- Install path (`items.json` missing) commits `{ live_ids, fetched_ids, kept, skipped }` to DataCache key `items.partial` after each completed 200-id batch (atomic tmp+rename). `fetched_ids` is every id whose body was already requested (keep and discarded).
- Resume: GET live ids, body-fetch only `live_ids` minus `fetched_ids` (same if build drifted). No partial -> first-fill.
- Warm-complete is `exists("items")` only. Never save `items` / `items.ids` / `items.skipped` until install finishes; then write those three and delete `items.partial`. `refresh_items` unchanged; FOLD3 same-build 0-fetch still holds.
- Hopper query: `items_fill_kind` -> FirstFill | Resume | SameBuildSkip (no RefreshDelta).
- ValidatedBuild SCHEMA = N.

## 1.14.26

### Fixed
- `cargo fmt --all` import order / wrapping after E3 land (SCHEMA=N).
## 1.14.25 - 2026-09-13

NeedsMechanic Engine E3: Elementalist AttunementState + TriggerBus OnAttunementSwap. SCHEMA CHANGE = N.

- NEW `rotation/attunement.rs` `AttunementState` sole writer of current (+ Weaver secondary on this state). Default Fire. Shared by flow sim and WvW timeline. Not an aura engine; no overload / dual-attack bar / jade / familiar.
- One new bus event/rule: `OnAttunementSwap` on the existing TriggerBus. Element filter uses existing `TriggerScope::Status` (Air/Earth/...). No OnAttuneToFire/Water/Air/Earth. While-attuned via `is(current)` and optional `prerequisite.attunement`.
- Profession attune skills mutate state (Weaver: secondary = outgoing primary; non-Weaver: secondary = None) and emit OnAttunementSwap only when primary changes.
- Executable records: One with Air (swap-to-Air -> Superspeed 3 s), Rock Solid (swap-to-Earth -> Stability 3 s), Arcane Prowess (any swap -> Fury 2 s WvW). NeedsMechanic:attunement -> record for those three.
- ValidatedBuild SCHEMA = N; TargetState SCHEMA = N; TriggerBus += OnAttunementSwap only.
- Kent causal micro-proof: Fire->Water changes current; while-Earth off in Fire / on in Earth; swap-to-Air fires One with Air only; no-trait = 0; >=3 attunement traits executing.

## 1.14.24 - 2026-09-13

Settings Refresh Game Data no longer wipes the cache. SCHEMA CHANGE = N.

- Refresh button removed `cache.clear_all()` and no longer clears `cache_build_number` before `RefreshMode::Default`.
- Warm cache + same build can take the FOLD3 skip path again (0 catalog body fetches); Clear Cache still wipes explicitly.


## 1.14.22 - 2026-09-13

NeedsMechanic Engine E2: trait-skill cast scheduler via existing TriggerRule sites. SCHEMA CHANGE = N.

- Optional `cast_skill_id` on NormalizedEffect; shared `resolve_trait_skill` applies lesser SkillEffects on the existing apply path (wvw_timeline + simulator). No OnTraitSkill; OnSkillUse stays the when, never "I cast a lesser".
- Executable trait-skill records: Final Shielding (OnElite → Lesser Arcane Shield), Defy Pain (OnElite → Lesser Endure Pain), Protector's Restoration (OnSkillUse Heal → Lesser Symbol of Protection). NeedsMechanic:trait skill → record.
- Lesser resolve does not emit OnElite/OnDisableFoe unless those events actually land. One TriggerBus.
- Kent causal micro-proof: elite + Final Shielding inactive vs active changes Arcane Shield; ICD holds; ≥3 trait-skill traits executing.

## 1.14.21 - 2026-09-13

NeedsMechanic Engine E1: TriggerBus OnDisableFoe fires on landed foe disable. SCHEMA CHANGE = N.

- Shared `land_foe_disable` reads `TargetState.disabled_until_ms` (Stability blocks; overlap that does not extend does not emit). Flow sim and WvW CrowdControl use the same emit; one TriggerBus (E0), no second disable engine.
- Executable OnDisableFoe records for Dazzling, Delayed Reactions, and Dulled Senses (NeedsMechanic:disable trigger -> record). No trait-skill casts; no profession cores.
- Kent causal disable micro-proof: disable inactive vs active changes Dazzling Vulnerability.

## 1.14.20 - 2026-09-13

Ada FOLD3: idempotent Refresh Game Data for KEPT catalogs. SCHEMA CHANGE = N.

- `RefreshMode::Default` (addon Refresh button) skips per-key body fetches and id-list probes when `CacheEntry.build` matches live `/v2/build`; `Verify` refetches KEPT only and compare-before-writes.
- Build-mismatch items Refresh body-fetches `|new ∪ cached keep|` via persisted `items.ids` snapshot — never the discarded ~95k commons when `items.json` exists. Equal rows reuse the cached row; all-equal stamps build without rewriting the data payload; vanished keep-ids are dropped.
- KEPT set unchanged: itemstats, specializations, traits, skills, professions, legends, pets, pvp_amulets, items (existing type+rarity filter), locale name packs.

## 1.14.19 - 2026-09-13

Phase 4 ComboEngine: shared field x finisher resolution from `data/formulas/combos.json`. SCHEMA CHANGE = N.

- NEW `data/combos.rs` loads the wiki combo table (`include_str!` + `OnceLock`, boons.json pattern).
- NEW `rotation/combo.rs` `ComboEngine` owns live fields (`ComboSite`, expiry, max 5 unique combatants) and emits `ComboOutcome`; one engine for flow sim and WvW timeline.
- `simulator.rs` and `wvw_timeline.rs` call the engine; hardcoded `resolve_combo` field x finisher arms deleted. Foe conditions go through `TargetState::apply_condition` (Phase 3).
- Projectile finishers honor API `percent` as deterministic EV (no RNG). Water heal / Dark whirl leech keep existing WvW coefficients; unread cells `note_unmodeled`.
- Kent: Fire blast Might, Water heal scales with Healing Power, Burning via TargetState, 20% vs 100% EV, 5th combatant ok / 6th refused; reaper dark-whirl / expired-field retargeted to engine.

## 1.14.18 - 2026-09-13

First-run setup nav/complete/news parity with Dieter DESIGN LOCK. SCHEMA CHANGE = N.

- Setup wizard nav is one centered Back|Next row (width 120, gap 8); Language is Next-only; never stacks Back under Next after download.
- DataDownload Next goes to Main (skips Complete / Get Started); Complete dropped from progress pills.
- Setup news uses config.news.enabled_sources() + kick/collected (Tyria Dispatch multi-source), with kick_art when show_images.

## 1.14.17 - 2026-09-13

NeedsMechanic Engine E0: shared TriggerBus + Endurance/Dodge family. SCHEMA CHANGE = N.

### TriggerBus + Endurance/Dodge

- Shared `TriggerBus` events are OnDodge, OnDisableFoe, OnElite, and OnThreshold only. WvW timeline wires live Endurance/Dodge -> OnDodge; flow `SimState` holds the same types for a follow-up tick hookup.
- `EndurancePool` + `DodgeAction` are the same dodge family (50 endurance per dodge, 5/s base regen). A successful dodge emits OnDodge so dodge-tagged trait records fire — no one-off trait-skill casts.
- `TriggerRule` gains OnDodge / OnDisableFoe / OnElite / OnThreshold, extending the OnShroudEnter pattern in `normalized_effects.rs`.
- Executable OnDodge records for Expeditious Dodger, Pumping Up, Resilient Roll, and Resolute Evasion (NeedsMechanic:dodge → record). OnDisableFoe may read TargetState disable; no second foe-condition map.
- Kent causal dodge micro-proof: endurance spend → DodgeAction → bus OnDodge → dodge traits execute.

## 1.14.16 - 2026-09-13

Revenant legends use the same icon+name skill-bar slots as Ranger pets. SCHEMA CHANGE = N.

- The current-build skills bar shows LEGENDS | UTILITY SKILLS | ELITE SKILL when a Revenant has legends and no pets, matching PET SKILLS geometry.
- Each legend slot paints the swap-skill icon and a compact human name (Alliance, Dwarf). Raw API ids such as Legend7 are never shown.
- Clicking a legend slot still previews that legend's heal / utilities / elite.
- Addon compile: `SimParams.deferred_target` is filled at the suggestion-rotation call site so the crate builds after 1.14.15.

News Show filters are icon+label tabs (All / Articles / Notes / Videos / Guides). SCHEMA CHANGE = N.

- Kind filters keep exclusive single-select wiring (`filter=None` for All) with the kind glyph left of the visible i18n label.
- Selected/hover uses the gold chip tokens; height follows `theme::control_height`; narrow widths wrap via wrap_chip (no H-scroll, no icon-only collapse).
- Per-tab kind hint tooltips are removed; the Show caption still carries `news.filter.show.hint`. English `news.kind.video` reads Videos.

## 1.14.15 - 2026-09-13

Live foe TargetState ledger and deferred vs-target resolve (Success [5]). SCHEMA CHANGE = N.

### Target-state modifiers

- Shared TargetState / TimedFoeCondition in combat_model seeds from EnemyDummy {protection, stability, hp} and carries live disable + foe conditions for flow sim and WvW timeline.
- Vulnerability (+1%/stack, cap 25) and deferred vs-target / vs-disabled / PerFoeStack percents resolve at skill land against live TargetState — never folded into static DamageModifiers strike/condi buckets or calculate_validated_stats.
- search_rank stays [i64; 9].

## 1.14.14 - 2026-09-13

Revenant elite swaps now carry legends through `retarget_after_elite_swap`. SCHEMA CHANGE = N.

### Revenant elite-swap legends

- After an elite swap, legends that fail `legend_available` are dropped, legal ones are kept, and the list is padded to 2 in the same template-code order `fill_revenant_legends` uses.
- The active legend's heal / utilities / elite package is applied via `apply_legend_package`, shared with plate fill.
- Aquatic legends keep remaining legal entries (padded) or copy terrestrial when none remain; empty aquatic stays empty so the encoder copies terrestrial.
- `swap_elite_spec` is unlocked for Revenant and still honors `locks.specs[2]`. `refill_bar` and the independent Rev heal / utility / elite operators stay no-ops.

## 1.14.13 - 2026-09-13

Revenant trait catalogue: every Revenant trait is classified; the coverage table's Revenant NoRecord column is 0. Track B Success [9] — all nine professions shipped.

### Revenant trait triggers

- The Revenant's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (legend, energy, upkeep, stance, facets, citadel, kalla's fervor, cosmic wisdom, affinity, energy meld, alliance, battle scars, band together, and the rest).
- New wiki records fire on existing sites: on-condition-applied chilled (Abyssal Chill torment), on-boon-applied fury (Incensed Response might), on-crit (Endless Enmity ally fury), heal skill use (Blinding Truths blinded, Ashen Demeanor might and resistance), and Periodic in-combat (Assassin's Presence ally fury). Legend / energy / upkeep / stance / Invoke Torment halves stay NeedsMechanic — no fake legend-state records and no new trigger enum variants.
- Honest NeedsMechanic names cover legend, energy, upkeep, stance, facets, consume, citadel, kalla's fervor, band together, cosmic wisdom, affinity, energy meld, alliance, battle scars, combat, profession skill, aura, condition duration, dodge, endurance, trait skill, incoming strike, ally state, percent heal, boon grant, max health, disable trigger, and the elite-line unlocks.
- `SHIPPED_PROFESSIONS` now includes Revenant; `docs/audit/trait-coverage.md` regenerated with Revenant NoRecord 0. All nine professions NoRecord=0.

## 1.14.12 - 2026-09-13

Mesmer trait catalogue: every Mesmer trait is classified; the coverage table's Mesmer NoRecord column is 0.

### Mesmer trait triggers

- The Mesmer's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (clones, phantasms, shatter, mirage cloak, continuum split, distortion, deceptions, ambush, blades, bladesong, instruments, interrupt, disable trigger, dodge, and the rest).
- New wiki records fire on existing sites: heal skill use (Metaphysical Rejuvenation ally regeneration), Manipulation skill use (Master of Manipulation ally aegis), on-crit (Critical Infusion vigor, Master Fencer ally fury), on-condition-applied blinded (Ineptitude confusion), and Glamour skill use (Temporal Enchanter ally resistance). Superspeed / interrupt-blind halves stay NeedsMechanic — no fake clone, phantasm, shatter, mirage cloak, or continuum-split records and no new trigger enum variants.
- Honest NeedsMechanic names cover clones, phantasms, shatter, mirage cloak, continuum split, distortion, deceptions, ambush, blades, bladesong, instruments, notes, alacrity, interrupt, disable trigger, dodge, trait skill, aura, stealth, recharge, weapon-scoped, signets, revive, block, and the elite-line unlocks.
- `SHIPPED_PROFESSIONS` now includes Mesmer; `docs/audit/trait-coverage.md` regenerated with Mesmer NoRecord 0.

## 1.14.11 - 2026-09-13

Elementalist trait catalogue: every Elementalist trait is classified; the coverage table's Elementalist NoRecord column is 0.

### Elementalist trait triggers

- The Elementalist's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (attunement, aura, overload, dual attack, jade sphere, elemental empowerment, familiar, conjure, glyphs, cantrips, meditations, combo, disable trigger, dodge, trait skill, and the rest).
- New wiki records fire on existing sites: on-crit (Renewing Stamina vigor, Burning Precision burning), heal skill use (Earth's Embrace resistance, Soothing Ice regeneration, Gale Song ally protection), cantrip skill use (Soothing Disruption regeneration), shout skill use (Tempestuous Aria ally might), overload skill use (Harmonious Conduit stability and swiftness, Hardy Conduit ally protection), stance skill use (Bolstered Elements protection), on-boon-applied swiftness (Woven Stride regeneration and cleanse), and on-condition-applied burning (Persisting Flames strike damage). Attunement-gated halves stay NeedsMechanic — no fake attunement records and no new trigger enum variants.
- Honest NeedsMechanic names cover attunement, aura, overload, dual attack, jade sphere, elemental empowerment, familiar, conjure, glyphs, cantrips, meditations, combo, disable trigger, dodge, trait skill, endurance, recharge, revive, ally state, signets, and the elite-line unlocks.
- `SHIPPED_PROFESSIONS` now includes Elementalist; `docs/audit/trait-coverage.md` regenerated with Elementalist NoRecord 0.

## 1.14.10 - 2026-09-13

Engineer trait catalogue: every Engineer trait is classified; the coverage table's Engineer NoRecord column is 0.

### Engineer trait triggers

- The Engineer's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (toolbelt, kits, gadgets, turrets, gyros, heat, photon forge, mech, morph, evolve, explosion, disable trigger, dodge, trait skill, and the rest).
- New wiki records fire on existing sites: on-boon-applied swiftness (Invigorating Speed vigor) and might (Boiling Point fury), on-condition-removed (Comeback Cure regeneration), elixir skill use (HGH ally might), heal skill use (Reconstruction Enclosure ally protection, Cleansing Synergy ally cleanse and regeneration), on-condition-applied bleeding (Sanguine Array might), on-crit (Incendiary Powder burning, Serrated Steel bleeding), tool-belt skill use (Optimized Activation vigor, Mechanized Deployment cleanse), stance skill use (Stainless Steel condition convert), and conditional foe-health gates (Heavy Metal crit chance and crit damage). Trigger kinds reused from prior professions; no new trigger enum variants.
- Honest NeedsMechanic names cover toolbelt, kits, gadgets, turrets, gyros, heat / photon forge, mech, morph / evolve, explosion, disable trigger, dodge, trait skill, barrier, endurance, incoming healing, percent heal, weapon-scoped shield, boon grant, movement skill / superspeed, and the elite-line unlocks.
- `SHIPPED_PROFESSIONS` now includes Engineer; `docs/audit/trait-coverage.md` regenerated with Engineer NoRecord 0.

## 1.14.9 - 2026-09-13

Guardian trait catalogue: every Guardian trait is classified; the coverage table's Guardian NoRecord column is 0.

### Guardian trait triggers

- The Guardian's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (virtues, aegis, block, tomes, pages, mantras, ashes, radiant forge, light aura, consecrations, symbols, signets, traps, physical, disable trigger, dodge, trait skill, and the rest).
- New wiki records fire on existing sites: on-crit (Empowering Might might), heal skill use (Healer's Resolution resolution, Liberator's Vow ally quickness), on-hit with burning gate (Inner Fire fury), trap skill use (Hunter's Premonition aegis), periodic cleanse (Strength of the Fallen), stance skill use (Shimmering Stances protection and blind), spirit-weapon on-hit (Eternal Armory burning), virtue skill use (Virtue of Resolution resolution, Righteous Sprint swiftness), and on-condition-applied immobilize/slow (Stoic Demeanor ally resistance and might).
- Honest NeedsMechanic names cover virtues, aegis block-end, tomes/pages/mantras/ashes, radiant forge, light aura, consecrations, symbols, signets, weapon-scoped axe/focus, incoming strike, boon grant, vitality scaling, and the elite-line unlocks.
- `SHIPPED_PROFESSIONS` now includes Guardian; `docs/audit/trait-coverage.md` regenerated with Guardian NoRecord 0.

## 1.14.8 - 2026-09-13


Warrior trait catalogue: every Warrior trait is classified; the coverage table's Warrior NoRecord column is 0.

### Warrior trait triggers

- The Warrior's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (adrenaline, burst, berserk, banners, soldiers focus, block, fire aura, dragon trigger, ammunition, chants, refrain, motivation, full counter, disable trigger, weapon swap, dodge and the rest).
- New wiki records fire on existing sites: on-condition-applied immobilize (Opportunist fury), burst on-hit / on-crit (Sundering Burst vulnerability, Cleansing Ire cleanse, Heat the Soul ally boons), on-crit (Bloodlust bleeding, Hardened Armor resolution), heal skill use (Thick Skin protection, Restorative Strength might and resistance), banner skill use (Doubled Standards resolution), periodic combat might (Empower Allies), on-hit foe-health gate (Heightened Focus quickness), rage skill use (Last Blaze burning), on-boon-stripped (Enchantment Collapse), and a passive outgoing-healing percent (Stalwart Focus). Trigger kinds reused from prior professions; no new trigger enum variants.
- Honest `NeedsMechanic` entries name the fidelity still missing (adrenaline, burst, berserk, banners, Soldier's Focus, block, fire aura, Dragon Trigger / flow / ammunition, chants / refrains / motivation, Full Counter, and related). They are not claimed as simulated.
- `docs/audit/trait-coverage.md` regenerated; Warrior `NoRecord=0`. Necromancer, Ranger and Thief coverage unchanged.

## 1.14.7 - 2026-09-13

Thief trait catalogue: every Thief trait is classified; the coverage table's Thief NoRecord column is 0.

### Thief trait triggers

- The Thief's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (steal, stealth, shadowstep, initiative, dodge, endurance, weapon swap, interrupt, artifact, malice, deadeye mark, shadow shroud, siphon, flanking, stolen skill, unique condition count and the rest).
- New wiki records fire on existing sites: on-boon-applied fury (Assassin's Fury might), on-crit (Unrelenting Strikes fury), on-condition-applied poison (Lotus Poison) and immobilize (Panic Strike poison), Stealth Attack on-hit (Sundering Shade, Even the Odds, Hidden Thief, Rending Shade), Trick skill use (Trickster cleanse), on-boon-applied swiftness (Don't Stop cleanse), and a passive outgoing-healing percent (Dark Sentry). Trigger kinds reused from prior professions; no new trigger enum variants.
- Honest `NeedsMechanic` entries name the fidelity still missing (steal, stealth, shadowstep, initiative, artifacts / Skritt Swipe, malice / Deadeye's Mark, Shadow Shroud / Siphon, dodge replacements, and related). They are not claimed as simulated.
- `docs/audit/trait-coverage.md` regenerated; Thief `NoRecord=0`. Ranger and Necromancer coverage unchanged.

## 1.14.6 - 2026-09-13

Ranger trait catalogue: every Ranger trait is classified; the coverage table's Ranger NoRecord column is 0.

### Ranger trait triggers

- The Ranger's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (pet, pet swap, weapon swap, spirits, celestial avatar, transform, unleash, wind force, arrows, disable trigger, dodge, traps, endurance, opening strike and the rest).
- New wiki records fire on existing sites: Survival skill use (Wilderness Knowledge fury and cleanse), Signet skill use (Stoneform might and fury), on-hit with a foe-health gate (Hunter's Gaze), Celestial Avatar skill use (Grace of the Land ally might), and a passive outgoing-healing percent (Natural Mender). Trigger kinds added for this profession are those skill-use categories and the on-hit health gate; no new trigger enum variants.
- Honest `NeedsMechanic` entries name the fidelity still missing (pets, beastmode merge, unleash, celestial avatar, Wind Force / Cyclone Bow, weapon swap, and related). They are not claimed as simulated.
- `docs/audit/trait-coverage.md` regenerated; Ranger `NoRecord=0`. Necromancer coverage unchanged.

## 1.14.5 - 2026-09-09

- Armor and trinket rows of an optimized build show an icon again. A plate names its gear by stat prefix and slot with no item id, so those rows had nothing to look up; they now take the icon of a cached exotic (else ascended) piece of that slot, armour weight and prefix, the way weapon rows already fell back to the weapon type.

## 1.14.4 - 2026-09-08

Sprint 3 convergence: Death's Carapace, the Scourge's shroud skills and a clean wiki check.

- Death's Carapace is simulated: a stacking toughness effect (20 per stack in WvW, 30 stacks at most, 10 s) that shrinks incoming strikes by armor / (armor + toughness). Armored Shroud, Putrid Defense, Shrouded Removal, Dark Defense and Corrupter's Fervor feed it from their wiki records, and a cleanse that removed a condition is now a trigger (Shrouded Removal). The cached Reaper build's "Not simulated" line falls from three traits to two, each naming the half still unmodeled.
- A Scourge's shade skills count as its shroud skills for traits that say "shroud skill N", as the wiki states; a build with a shroud bar is unchanged.
- The wiki-number check is clean: a derived value names the page number it comes from, the game's own API facts count beside the page, heuristic records are skipped, and Superior Sigil of Bursting carries the page's +5% (the record said 6).

## 1.14.3 - 2026-09-08

Trait triggers are the build: the WvW simulation now fires the Necromancer's traits.

### Trait triggers in the WvW simulation

- Trait records fire at the moments the game fires them: entering and leaving shroud, landing a critical hit on a chilled foe, using a shout, an elixir, a signet or a numbered shroud skill, putting a condition on a foe, gaining a boon, stripping or corrupting one, and every few seconds. A record with a prerequisite (the foe is chilled, the foe is below half health, you are in shroud) waits until it holds and says so in the trace.
- New payloads: life force gains, heals with a healing-power coefficient, a scaled gain per condition consumed, timed strike bonuses, critical-damage and critical-chance bonuses that hold in shroud, against a foe with a condition, or per stack of it.
- The Necromancer's 108 traits are catalogued: every trait is executed from its facts, executed from a dated wiki record, or classified with the mechanic it still needs (carapace, blight, shades, spirits, minions, life siphon and the rest). The trait coverage table in `docs/audit/trait-coverage.md` shows the state of every trait of every profession; the other eight professions follow one increment each.
- The "Not simulated" line names only what was skipped, each with its reason: `no record`, `needs: carapace`, `unresolved value`, `no firing site`. Weapon skills the builder produced effects for and traits whose facts the parser or the stat sheet consumed no longer appear. An empty line leaves the build Verified.
- Fights count their population: in a Havoc or Cloud scenario, strikes and conditions reach the secondary foes in range and boons, heals and cleanses reach the allies, within each effect's target cap. Support, Commander and Staller builds rank on the boon time they give allies. Roam fights are unchanged.
- The improviser enters an affordable shroud instead of leaving a Reaper on its weapons for the whole fight.
- Non-damaging conditions a skill applies (Chilled, Crippled, Weakness, Vulnerability) land on the foe in the WvW simulation; they used to be counted as buffs on the player. Fear and Taunt count as conditions as well as disables.

### What the simulator simulates in WvW

- Harbinger Shroud and Ritualist's Shroud have their numbers now (wiki, read 2026-09-08): Harbinger drains 5 % a second, reduces nothing, leaves health exposed and lets healing land; Ritualist's drains 3 % in PvE and 5 % in WvW and PvP behind the same 33 % / 50 % reduction as Death Shroud. Blight is not modeled. Scourge has no shroud and never did in the simulation: Manifest Sand Shade is the F1, and shade skills run at all times; their life force costs are not in the API facts and are not modeled.

## 1.14.2 - 2026-09-08

Choya can be read, and the builds it puts beside its own can be told apart.

### Reading Choya

- A reply is shown whole. It used to be cut at 600 characters and end in three dots, with the rest thrown away before it reached the transcript.
- Replies are laid out, not dumped: bullets on their own lines, names the game knows in the accent colour, bold names, italic asides, warning lines, rotations as `A → B → C`, web addresses underlined and clickable. Choya is asked for that shape and for facts over prose.
- The comparison tabs say whose build each is: a blue Current tab for what is equipped, a green Optimized tab for what Choya or the optimizer made, and a tab in the site's colour named after the site for a published build. Opening a published build or loading a saved one adds a tab; nothing is replaced.
- The published-build cards stay while you ask follow-up questions; only a new build replaces them.
- When the model does not answer, the fallback answers what was asked: a scoring question gets the referee's verdict on the plated build, a build request gets the optimizer's build for the specialisation named, anything else gets a plain line with what to do next. A question about a Reaper no longer comes back as an optimizer run for another profession.
- While Choya works, the bubble names the step and counts the seconds, says "nothing for N s" when nothing is arriving, and expands on click to the model's live reasoning, or its answer as it streams, or the tools it has called. There is no time limit on the wait any more; Stop ends it and keeps what arrived, Retry asks the model to continue from there. The addon log has one line per step with its duration and outcome.

### Builds side by side

- The Improve and New Build panes share one strip of tabs: Current (blue), Choya's pick (green), and one tab in the site's own colour for every published build the chat offered, present as soon as the cards are, not only after a click. The selected tab is solid with a bright rim and a bar beneath it; the header row keeps one order so switching tabs moves nothing.
- Site colours are the sites' own: GuildJen pink, Hardstuck red, Snowcrows ice cyan, each adapted in brightness to the active theme.
- Loading a saved build adds a tab instead of replacing the strip.

### Fixes

- The build and equipment tabs you picked for a character no longer snap back to the in-game active tab when the API refresh lands or the character list is opened.
- Settings has default Scale and Role beside the default Game Mode; they apply at startup.
- Rotation arrows and list marks are drawn as shapes, so they render in every font instead of as a question mark.

## 1.14.1 - 2026-09-07

The evening's question was why no free model produced a build any more. The answer is written down, with the numbers, in `docs/llm-requirements.md`, and this release fixes the parts that were ours.

### Choya

- Google's free tier allows five requests per minute on Gemini 3.8 Flash, and a Choya run had grown to six or more. The sixth was refused with "retry in 39 s" and the addon treated that as final. Choya now learns each model's stated limit from that reply, keeps it, paces itself to it, and waits the seconds Google asks for instead of giving up. A daily quota is still final; waiting a minute does nothing for that.
- The profession reference handed to the model - every specialization, trait and slot skill, about 26 KB - was being cut to its first 2,000 characters on the way into the prompt, while the prompt promised the model it was complete. Every run started by fetching through tools what it should have been reading. The reference now arrives whole.
- A model whose handshake got no answer at all (a 404 from an account setting, a 429 from a busy pool) was recorded as "no tools, answers in prose" for a week. A handshake that never happened is no longer kept.
- The chat's own two-minute stopwatch fired while the model was still legitimately writing the build, threw the result away, and showed "timed out" with no build at all. The worker owns the deadlines and always ends in a build; the stopwatch is a backstop again.
- A rate limit now says whose it is: "the provider is throttling everyone right now" reads differently from "your daily quota is used up" or "this tier allows a few requests a minute", and the addon had been showing one line for all three.
- Free models on OpenRouter think at low effort. A free reasoning model given medium spent its whole closing budget thinking and returned nothing.
- Settings says how to get more free models on OpenRouter: some are hidden unless the account allows prompt sharing with their providers, which is a switch at openrouter.ai/settings/privacy, not in the addon.
- The Free switch in Settings now also filters the model list in the Choya row.
- A run on a free model, or on a Gemini key whose quota Google has not stated as generous, is now two lookup rounds and the plate: about five requests including the handshake, where it was six to ten. Measured on the same free model, four runs before took 46 to 199 seconds; three runs after took 30 to 86, all with a valid build. The instructions no longer tell the model to confirm with a tool what the profession reference in front of it already says, and no longer force a tool call on the first turn.
- A free endpoint that takes minutes on one lookup no longer takes the whole run with it: a lookup on a free model is abandoned after 90 seconds and Choya plates from what it has. One in-game run had sat 179 seconds on a single lookup and then timed out writing the build.
- Gemini's closing request now carries an output cap, so a model that reasons at length cannot spend minutes on it.
- `cargo run -p gw2-optimizer --example choya_live -- <provider> <model>` runs the real contract against the configured keys and prints PASS or FAIL with the request count. This is what "the model works" means from now on.

### What the simulator now simulates in WvW

- On-crit sigils fire. Ranking counts them at the build's critical chance with the cooldown applied to the expected rate; the diagnostic trace also rolls eight fixed seeds and reports how far the expected value sits from a proc that either fires or does not.
- Swapping weapons swaps sigils: the sigils on the stowed set load, fire only while their set is held, and keep one cooldown across the swap.
- Health-threshold and stacking bonuses apply per strike, only while true: Rune of the Scholar above 90 % health, Relic of the Thief up to five stacks for six seconds from weapon skills with a recharge. Nothing is counted twice, and PvE and PvP results are unchanged.
- Dark field combos resolve: whirl finishers leech, leap and blast grant Dark Aura.
- Life force and shroud: every Necromancer specialisation shares one shroud shape (10 % to enter, drain per second, damage to the pool at the mode's reduction, no healing inside, out at zero); the shroud bar is built from the game data, and a build that cannot enter shroud says why.
- The records for Sigil of Fire, Rune of the Scholar and Relic of the Thief are rewritten from the wiki and dated; the resource-model check is derived from the skills instead of a list of professions.

### What the simulator did not simulate

- A build's result now says what it did not simulate, by name, instead of counting it: the quality marker on a comparison carries "Not simulated: Superior Sigil of Fire (on-crit), … and N others" beside it, the same line is in Choya's evidence and appended to a plate's concerns, and a plate served by Choya now shows the referee's Provisional / Verified marker instead of Verified regardless. Nothing is scored differently; the line qualifies, it does not penalise.
- Choya can ask for the app's own verdict on a whole build: `score_build` takes the complete plate and returns viable, the gate results, the score, the six realized axes, the data quality and the coverage line, computed by the same validated-build and referee path the app uses. The old prefix-only form still works and says so. Three such evaluations per chat request. `simulate_rotation` now says it estimates a skill list on an open dummy and is not a full-build verdict.
- `docs/simulator-connection-audit.md`: the first slice (Necromancer Reaper, WvW) traced end to end, with the findings, the 8 × 8 matrix and seven kinds of causal experiment behind the line above.

## 1.14.0 - 2026-09-07

Every number in this release was checked against a wiki page, and the page is named at the constant it justifies. Where the wiki has no dev statement, the code says "community-tested" rather than pretending.

### The build it recommends

- Skills recharge when the cast finishes, not when it starts. Every skill had been getting its whole cast time back for free, which flattered long casts most. The wiki's rule - "once the activation is complete a skill will enter a recharge time" - is what runs now. A channelled skill really starts recharging a little earlier, at the start of its active phase; there is no public data on where that phase begins, so channels now err late by their own length, where before everything erred early.
- Chill is 60% recharge, not 34%. Yesterday's release read the tooltip's "cooldown increased by 66%" as a 34% rate. The wiki puts it plainly: for every 1.66 seconds chilled, one second of cooldown expires. Supports under Chill get their heals back sooner than 1.13.0 said, and later than 1.12.0 said.
- A boon strip takes what you applied last. It used to take whatever was about to expire anyway, which is the one thing a strip never does. Last in, first out - the reason every WvW guide says "stability out first, then cover it" - is what the enemy does now, and extending a boon no longer moves it to the front of the queue. Cleanses already worked that way.
- Damage modifiers stack the way the game stacks them. The wiki is explicit that some sigils, traits and utility effects add together before the rest multiply, and it gives no rule for which is which - it is a tested fact per effect. Seventy-nine effects are now named in the data as additive, unnamed ones multiply as before, so nothing that was right can have gone wrong. Warrior's Peak Performance, Berserker's Power, Warrior's Sprint and Fierce as Fire are all additive, and Warrior is exactly where the old model over-ranked stacked bonuses.
- A multi-hit skill lands its hits across the cast. Both simulations used to drop the whole thing as one lump, which undercut the premise this addon is built on: a hit that lands inside a burst window is worth more than the same hit outside it, and a channel that starts inside a window and runs out of it is worth something in between. Hits are now spaced by measured timing where anyone has measured it - Guardian and Ranger, from gw2combat's log-audited files - and evenly across the cast everywhere else. An interrupt drops the hits that had not landed.
- The referee plays the rotation the page wrote. Every build site writes its rotation as `Rifle 3 > Shred > Rifle 5 > 2 > Demolish` and nothing read it, so a published build was judged on an opener the simulation invented. Seventy-four of the 124 synced WvW builds are now judged on their own rotation line, and the timeline improvises only after it runs out.
- Boons cap where the wiki caps them: 30 seconds for most, 60 for Swiftness, none for Might, Aegis and Regeneration - the last three had been capped at 30 in the data. Stack caps come from the same data instead of a number typed into two places.
- An interrupted cast goes on a 4-second recharge. The wiki disagrees with itself (two pages say 4, two say 5); the newest page wins and the disagreement is written at the constant.

### Choya

- When Choya simulates a rotation it now keeps the two weapon sets apart. The tool it calls was handed a flat list and put both sets in hand at once, so a rotation could weave Greatsword and Longbow skills with no swap between them.

## 1.13.0 - 2026-09-07

### The build it recommends

- The optimizer can tell your builds apart again. A build that failed any viability check was scored -1.0 on every axis and skipped the rotation simulation entirely - and measured against the 124 synced community builds, **not one of them passed every check**. The score that carries what you asked for was the same number for every build a real player has ever published, so it could not order anything. Only the checks that the published meta actually clears may refuse a build now; the rest are written on it as caveats. 67 of 124 pass.
- WvW support builds stopped being judged on a rule none of them can satisfy. The sustain check demanded that a healer end the modelled fight able to repeat it, which no published support build does, so every one of them was refused and every one of them tied on the ranking key meant to separate them. It now asks whether you survived, which about half of them do.
- The enemy no longer attacks at a constant rate. It used to repeat one four-second burst forever with six-tenths of a second between the last hit and the next opener, which is not a fight - it is a drip, and against a drip nothing matters except raw mitigation per second, because a heal on a twenty-five second cooldown can never catch up. Support builds are watched for twenty seconds where a DPS is watched for five, so they ate four uninterrupted bursts and died. Damage now ramps to a peak worth spending an evade on, and then lets go long enough to heal. The published support builds went from 14% to 79% on the sustain check, and the DPS builds barely moved.
- Conditions have to be cleansed. They used to expire during the lull between bursts, so waiting was a complete answer and a build carrying no cleanse at all measured exactly the same as one built around cleansing. Condition damage is not strike damage: armour does not reduce it, protection does not reduce it, and an evade cannot dodge what is already ticking.
- Chill costs you your skills. Alacrity made cooldowns come back faster and nothing anywhere made them come back slower, so the enemy could not touch your skill availability - which leaves out the way a support actually dies. Chill does not have to out-damage your healing. It only has to keep your heal on cooldown until the next burst lands.

### Choya

- Choya answers instead of running out the clock. One message could cost eighteen sequential requests: up to eight tool rounds plus a closing request, twice over, because a refused build was always composed again. On a free model that is minutes of silence ending in "request timed out". Looking things up is now bounded, a round that runs out its own deadline answers from what it gathered rather than throwing it away, and a second attempt only starts if the first left time.
- Choya is handed the profession instead of fetching it. It used to spend its first rounds asking what specializations exist, then what traits each one has, then guessing skill names one at a time to find out which were real - twenty-one seconds of a thirty-one second run, and six round trips, for data already in memory. It is all in the first message now. The skills in particular were a guessing game: there is a list of every rune, sigil and relic, but there was never one of a profession's skills, so a wrong name was found the hard way.
- Google models answer instead of giving up. When one used all its tool rounds it returned "tool loop exceeded" and nothing else. It now writes the build from whatever it gathered, the same as the other providers.

### Reading it

- Dashes, quotes and arrows draw. English drew the whole addon in the game's own typeface - a set of glyphs we do not control - so an em dash reached you as a question mark mid-sentence, including in text the model wrote. Every Latin language now uses the face whose glyph coverage we do control, and it covers punctuation, arrows and mathematical symbols, because no test can hold a language model's prose to ASCII. The game typeface is still there under Settings for anyone who prefers it.

## 1.12.0 - 2026-09-06

### Choya

- Choya answers with a build when no character is selected. It used to ask which profession, and ask again when pressed, and serve nothing either time - the prompt named the profession as `unknown` and never said what to do about that. It now picks the profession that suits what was asked for and says in one clause which it chose and why. The chat also says so above the reply, in warning colour, rather than leaving "no character" in grey among three other facts where it went unnoticed.
- A refused build is no longer reported as "I kept your build" when there is no build to keep. The viability gate exists to stop a worse build replacing one already being worn; with nothing worn there is nothing to protect, so the plate is served with the concern written on it. Told what is weak, you can decide. Told that something was kept, you were given neither a build nor the truth.
- Requests stopped timing out. Three faults, all ours: a completion budget of 65,536 tokens cannot be delivered inside a 420-second deadline at any realistic speed, so any model that used it ran out the clock; where a model did not accept our reasoning effort the fallback took the first entry of its list, which on `z-ai/glm-5.3` means `max`, so a model that must think was told to think as hard as it can on every message; and a timeout was retried on an identical budget, which cannot end differently and only doubles the silence.

### Community builds

- Under every proposal, the closest published build from each site, and clicking one opens it here as a suggestion of its own with the site's own link beside it. 740 community builds were already on disk and the only thing reading them was a percentage.
- Closest means closest to the build, not to the profession. Specializations weigh most, then the job, then the weapons that decide five skills each, then rune, relic and prefix - two builds sharing all three specializations are the same build with different gear.
- A card must be the job, the damage flavour and the scale that was asked for. A healer and a DPS are opposite jobs; assassin and duelist are their own. Power and condition are different builds, decided by the label where it is explicit and by the gear where it is not - one site publishes a Marauder Reaper as plain "Roaming DPS", which is a power build whatever the label omits. Scale disqualifies asymmetrically: a roaming build can walk into a zerg because it carries its own sustain, but a zerg build cannot go roaming, where it leans on twenty people's boons and dies alone. Hybrids are exempt, being self-sufficient by construction.
- Where nothing qualifies, nothing is shown. Nobody publishes a roaming Necromancer healer, and saying so by offering no card is better than answering with a build that would die in the fight it was asked about.

### Providers and models

- The model list shows only models that can serve this addon, best first. It was every id the provider returned in alphabetical order - on OpenRouter that is 431 rows sorted so the one worth picking is two hundred lines down. Models without tool support, without text output, batch jobs that answer within 24 hours and models carrying a retirement date are gone; what remains is ordered by published agentic score, with the score beside the name.
- A Free filter, on by default, greyed for providers that have no free models. Free is read from each provider's own data - OpenRouter states a price per model, Google publishes a free tier per model - never from a list we maintain. Turning it on brings out a Choya in sunglasses.
- Every request is now built for the model it is sent to. Its completion budget is clamped to what that model will produce and its reasoning effort chosen from the list that model publishes: 153 of OpenRouter's 431 models cannot serve the budget we used to send unconditionally, and 34 reject the effort - including the highest-scoring free model there is, which accepts only `xhigh` and `high`.

### Viability

- The viability gates were measured against the 740 community builds rather than set by taste. Every published PvE build passed; not one published WvW build did. `cargo run -p gw2-optimizer --example calibrate_viability` prints the pass rate per gate, so a miscalibrated one is a number rather than an argument.
- A protected sequence now completes on support output. It used to require damage, control or a condition, so a healer's protected window - healing and cleansing - was discarded, and a WvW healer failed the gate by doing its job.
- Gates the published meta itself fails no longer refuse a build; they report. `HarasserStrip` rejects 89% of published roamers and `ProtectedExecution` 73% of what it judges - a rule contradicted by the whole body of evidence it describes has no authority to reject anything. Nothing is silently forgiven: a gate that fails travels with the build as a caveat.

### Benchmarks

- The scrape keeps each page's prose, which is where every site writes its rotation and what the role is actually for. 188 of the synced builds carry an explicit `Weapon Swap` in an ordered chain. The parts list was being kept and the assembly order thrown away.
- Weapon types are read from Snowcrows rather than the hand they sit in, so `Shortbow 5` in a published rotation can be resolved at all.
- Hardstuck's own role and game type are read from the page instead of inferred, and an em dash in a build description no longer ends a sync - a byte-indexed slice through a multi-byte character killed a 328-page run.

### Settings

- The benchmark table sizes itself to its contents instead of sharing out the whole panel between three columns of three digits, and Failed is a column with a heading rather than a number hanging off the right edge. It stays on screen at any window width, and stays visible while a sync runs and before the first one.
- The Optimize button follows the font scale like every other control instead of being fixed at 28 pixels.
- Theme, provider and preference rows are paired two to a row, and a named theme joins a list rather than replacing the last one.


### Choya

- Choya no longer composes a build from nothing. The chat advertised a `get_optimizer_results` tool and then handed it an empty list, so every call answered "No optimizer results available" and Choya reasoned from the player's message alone - while the deterministic optimizer could answer the same scenario, respecting the same locks, in about 30 milliseconds, with a build that already passes every viability check. Measured 2026-09-05 against a real character: WvW Roam Support with Scourge locked, the deterministic answer sits at 68% health and repeatable; the plate Choya composed blind that evening sat at 44% and could not repeat, so the referee refused it and the second attempt timed out. That worked answer is now in Choya's Context as a floor: match its survivability at least, then beat it on what the player actually asked for, and be able to say why if you depart from it.

### Benchmarks

- Sync Benchmarks downloads every build the sources list, class by class. It used to take the top 15 rows of each GuildJen category and the first 45 from Snowcrows and Hardstuck - and because the tables are grouped by profession, those 15 bought Elementalist and part of Necromancer while the other seven classes got no reference at all. Measured 2026-09-05: the store held Guardian, Mesmer, Revenant, Thief and Warrior, and a Necromancer looking at WvW was told "No benchmark data available" while the sync was working exactly as written. The WvW page alone lists 99 builds across 8 classes; all of them are fetched now, and progress names the class it is on, so cancelling leaves whole classes finished rather than a slice of each.
- The sync paces itself. One page at a time with a gap of roughly one to two seconds, jittered rather than fixed - several hundred pages fetched back to back is unmistakably a script, and GuildJen already answers traffic it dislikes with a block page. A full run is now minutes rather than seconds; it reports progress throughout and Cancel stops it promptly, including mid-wait.
- Benchmark data reads as a grid: providers down the side, PvE / PvP / WvW across. The sources do not cover the same modes - Snowcrows is PvE, GuildJen is WvW and PvP - so a single total each could not tell you whether the mode you actually play is covered. Pages a run listed but could not read are counted in red at the end of that provider's row, with a Retry beside them; a retry costs almost nothing, because a re-run the same day skips every page already read and fetches exactly those. Before this, a source that listed 157 builds and returned 148 reported "done 148" and the nine simply vanished.
- While a sync runs, GuildJen names the category it is on rather than just the mode. Raid, fractal and open world are all PvE and each index restarts the count with its own total, so three of them in a row read as one list whose total kept shrinking.
- The default game mode radio buttons sit in one row instead of a stack of three.

- Running the sync twice in a day only downloads what is new or missing. Every build already read today is taken from disk and its page is not requested again - no fetch, and none of the wait that goes with one. So a run that was cancelled, or interrupted by closing the game, or that lost one source to an error, can simply be run again: it picks up where it left off in seconds rather than starting the whole several-hundred-page walk over. Same day only, deliberately - tomorrow every build refreshes, because a sync that quietly did nothing would be worse than one that takes its time.

- Benchmark files from the old format are cleared automatically at the start of every sync. Until 1.11.30 the GuildJen scraper read the profession out of a URL path segment the site no longer has, so it filed builds under whatever it found there - a real install had collected `guildjen_1.0_pvp.json`, `guildjen_comments_wvw.json`, `guildjen_fonts_pvp.json`, `guildjen_pages_pvp.json`, `guildjen_https__wvw.json` and `guildjen_guildjen.com_pvp.json`, fourteen files in all. Nothing generates those names any more, so nothing would ever have overwritten them: they would have sat there looking like benchmark data for the life of the install, on every machine that synced before the fix. Anything not named for a real source, profession and game mode is now removed before a run starts, and the sync says how many went.

- A rate limit no longer costs you builds. When a source answers "slow down" - a 429, a 403 from a bot filter, or a challenge page served with a normal status - the sync treats it as a wait rather than a refusal: it honours the site's own Retry-After, eases the gap between every following request, and gives that page up to eight attempts instead of three. The slowdown decays as pages start coming through again, so a rough patch costs minutes rather than the rest of the run. Settings says so while it happens, in yellow, instead of showing a bar that looks stuck.
- A failed page is retried instead of silently dropped. Timeouts, dropped connections and the statuses that mean "slower" or "later" (408, 425, 429, 5xx) get up to three attempts with growing backoff; a 403 or 404 is taken at its word. Requests also carry the Accept and Accept-Language headers any reader sends, which they did not before. A sync cancelled before it starts now makes no network requests at all.

### Overlay

- The arrow in "Go to Settings > Sync Benchmarks", "Settings > Cache > Refresh Game Data" and the stale-data notice is no longer a question mark. Those strings used a typographic arrow, which is outside the glyph range the Latin overlay font is built with, so it reached the player as "?". Latin languages only - the CJK fonts carry it.

## 1.11.31 - 2026-09-05

### Choya

- A chat plate now uses the validated per-slot kit for its stats, not one prefix painted over the whole set. Mixed Sentinel/Dragon (or any mixed prefixes) show the toughness and vitality those slots actually give, PvP plates use the amulet, and the validator's slot corrections show up on the plate and in the bubble.

### Benchmarks

- Sync Benchmarks reads GuildJen's category list from the site's sitemap, and now finds every build category it publishes. Discovery was reading the build hub, whose category grid renders inside a consent-gated embed: fetched without marketing cookies that page carries prose, social links and one off-site card, and on 2026-09-05 it had 41 links with not one category among them. So discovery found nothing on every run and the sync quietly fell back to a hardcoded list of three - which was itself short two categories, dropping raid and fractal builds entirely. The sitemap needs no consent, is what the site publishes for machines, and listed five. Checked against the live WvW page the same day: 99 builds listed, all 99 filed under the right profession and specialization, newest elite specs included.

## 1.11.30 - 2026-09-05

### Choya

- A proposed build now shows every slot and marks the ones that moved. It is a build you are meant to equip, so it lists the whole loadout whether a slot changed or not - and everything Choya changed carries a green halo: skill slots, specialization hexagons, trait circles, armour, trinkets, weapons and the relic. Same shape as the gold lock ring, on purpose: gold means you pinned it, green means Choya moved it, and an unmarked slot honestly means it is what you are already wearing. A slot that changed is marked even when the combat numbers did not move, because "identical to yours" and "different, no measured gain" are not the same answer. The same three utility skills in a different order is not a change and is not marked.
- Choya changes anything you did not pin. The rule is now stated as you would say it: if you ask it to keep your weapons, your runes or your gear, it keeps exactly that and changes the rest - and everything you did not name is its to improve whenever it can argue the change is better. It is also told to fill both weapon sets, all four sigils and the relic on every build, because saying nothing about a slot is not "keep what they had", it reaches you as an empty slot.
- A build from the chat keeps the weapons you are holding. The prompt tells Choya to copy your weapons only if you asked it to, so a plate that changes nothing about them says nothing about them - and nothing put them back. Seen in-game 2026-09-05 on 1.11.29: a complete heal Scourge, specs, traits, skills and rune all present, arrived on the Optimized tab with an empty WEAPONS column. Your equipped weapon sets, their sigils and your relic are now carried onto any plate that does not name its own, the same way your heal, elite, utilities and stat prefix already were.

## 1.11.29 - 2026-09-05

### Choya

- When Choya's build is sent back for a second try, it is now told what to change, not just what failed. The refusal used to be the referee's own note - "SustainRecovery (survived=true, health=44%, margin=-566/s, repeatable=false)" - which is true and tells a model nothing about which half of the build to touch, on the one retry it gets. Seen in-game 2026-09-05 asking for a heal build that stops dying first: the first plate was refused for exactly that, and the second never landed. Each gate now carries its remedy - raise sustain, add cleanse, add a stunbreak, add a disengage, raise effective health, strip boons first, raise damage, add an interrupt, cover the chain, or stop overspending the profession resource.
- A failed chat request now records what the provider actually said. The error shown in the bubble is a category ("Request timed out. Try a larger/faster model."), and the provider's own message was thrown away before anything logged it, so an in-game timeout left no trace of which request, how long, or why. The raw error is written to the Nexus log first.

### Overlay

- Lock All no longer crashes the overlay. It wrote into a three-slot array using a counter taken straight from the character's build tab, and nothing on that path - neither the API response nor the on-disk cache - is clamped to three, so a tab resolving four or more specializations panicked out of bounds inside the render callback.

### Radio

- The station guard now catches every form of a local address. It read the URL host straight into an IP parse, so an IPv6 literal arrived with its brackets ("[::1]"), failed to parse, and fell through to a resolver-dependent lookup of that bracketed text - the exact trap the station-logo screen documented and defended against, in the one copy that never got the fix. Both now use one guard, shared with news images.
- That guard also missed IPv4-mapped IPv6: `::ffff:127.0.0.1` is loopback, but answers no when asked directly, so it walked through both the stream and favicon screens. It is now unwrapped before the check.

## 1.11.28 - 2026-09-05

### Choya

- Google models keep their train of thought across a tool loop. A tool loop is one continuous thought interrupted by lookups, and the reasoning blocks a model produces have to come back to it on the next turn, in the order it produced them; the addon dropped them. Gemini 3 refuses to continue a loop whose blocks are missing, and 1.11.27 made that the normal case by always sending tools and raising the round budget from three to eight.
- Choya stops thinking for minutes at a time. The thinking budget was sent as a token cap, which Gemini 3 does not take - those models want a thinking level, and a raw token budget is remapped to whatever level Google picks. It is now sent as a level. On models that already worked the setting is unchanged: it is the same half of the completion budget the old cap spelled out by hand.
- A model that cannot produce a usable tool call is now told to stop calling tools before being asked again. It was only having the tool list withheld, while the prompt in the same breath went on ordering it to call `get_spec_traits` - the exact contradiction that already had to be countermanded when a loop runs out of rounds.
- The prompt no longer asks for strict JSON and a tool call in the same turn, which Google documents as a cause of malformed function calls. A turn is now either tool calls or the finished build.

### Benchmarks

- Sync Benchmarks finds builds again. GuildJen moved: the profession was read out of a URL path segment the site no longer has, and the list of category pages was frozen while GuildJen keeps adding and retiring builds. Categories are now discovered from the builds hub, links are read only from each page's build table so a sidebar entry cannot file itself under the wrong game mode, and the profession comes from the elite specialization or core name in the address.

### Feedback

- A message you send the developer is no longer lost when the history file cannot be read. A failed load was treated as "no history yet", and the next write published that empty state over the real file. A failed load now refuses to publish for the rest of the session and says so, and every write goes through the same atomic replace the rest of the addon uses.

## 1.11.27 - 2026-09-05

### Choya

- Choya's build now has to beat the one you are already wearing before you are shown it. The chat path never ran the viability gates or the always-better check that the Improve button has always run, so any structurally complete answer was served as "Choya's pick". Seen in-game 2026-09-05 (Guardian, WvW Roam, Bruiser): the plate lost 207 Power, 280 Ferocity, 311 Condition Damage and 157 Healing Power to gain 81 Vitality, and carried no condition cleanse at all in a mode whose own gate demands it. A plate that fails is now sent back to Choya once with the exact check it failed; if the second one also loses, Choya says so and your build stands.
- Choya no longer answers with its own half-finished notes. When a model asked for tools and ran out of turns, the chat showed whatever it had said mid-thought, which is how a raw `{"explanation":...,"specializations":[]}` blob reached the bubble. The tool loop now makes one final request with the tools withheld, so the model answers from what it gathered.
- Choya talks to Google models again. When a character was loaded, the chat asked the model for a build while sending it no tools at all, and the prompt in the same breath told it "an equipped loadout is your STARTING POINT, not a licence to skip the tools - you must still call get_spec_traits". A model that obeyed had nothing to call. Every Google model tried on 2026-09-05 failed on this: Gemini 3.8 Flash and Gemini Flash Latest answered MALFORMED_FUNCTION_CALL, Gemini 3.7 Flash returned a call and no text. The contradiction was introduced earlier the same day in 1.11.10-1.11.24, which removed the old "do not call tools" instruction from the prompt without changing the code that withholds them. The tools are now sent on both paths.
- A model that genuinely cannot use the tools no longer kills the conversation: that case retries once with the tools dropped instead of failing.
- Provider errors name their cause. OpenRouter reports the upstream reason in a field the addon discarded, so a failed Gemini call read only "Empty response (finish_reason: error)". It now reads "error/MALFORMED_FUNCTION_CALL".
- Choya sounds like a choya. The persona follows the wiki: grumbling, needled, famously aggressive, keeps the village peaceful by kicking troublemakers off the mesa, likes shiny things, dancing and coconuts. One flourish per reply, never in the middle of the reasoning. The radio DJ voice gained the same lore and a "needled" mood.

## 1.11.26 - 2026-09-05

### Optimize

- A WvW build is no longer judged non-viable because its heal or elite is still on cooldown five seconds after the fight. The Sustain gate's "repeatable" check demanded that every skill in the best protected window be ready again within 5s of a 20s fight, which every heal (20-30s) and every elite (60-180s) in the game fails, so for Support, Condi, Commander and Troll roles the search spent its whole budget looking for a viable build that could not exist. Seen in-game 2026-09-05 on 1.11.25: a Roam/Support Scourge finished the exchange at 87% health and was reported NON-VIABLE with "repeatable=false". Repeatable now means the player leaves the exchange alive, with resources back, and either the target down, a positive sustain margin, or half the bar left. Cooldowns are still enforced inside the fight. The same run replayed: the seed is viable at once, the search runs 53 rounds in 15s instead of stalling, and the result is viable.

## 1.11.25 - 2026-09-05

### Optimize

- The optimizer now knows every condition cleanse in the game from a table, not from a text pattern. `data/cleanse_sources.json` lists 385 sources: 280 skills (heal, utility, elite, weapon, profession-mechanic, toolbelt and kit skills), 77 traits and 28 sigils and relics, one list per profession and specialization, catalogued from the game data by one reader per profession, cross-checked against the wiki, and re-derived by a second independent reader. 354 look-alikes (boon corruption, Resistance, "heal when you remove a condition", condition-damage text) are recorded as judged non-cleanses so the old text pattern can never fire on them again. Before this, in WvW, a Reaper running "Suffer!", with Consume Conditions, Plague Signet, Well of Power and Spectral Walk all one swap away, was judged to have no cleanse at all and the search served a non-viable build (seen in-game 2026-09-05 on 1.11.24): Necromancer transfers, sends, consumes and converts its conditions, and the pattern only knew remove, cleanse and cure.
- Cleanses that exist only through a trait (Cleansing Ire on Warrior bursts, Restorative Illusions on Mesmer shatters, Blurred Inscriptions on Signet of Midnight, 99 in all) count only when the build runs that trait.
- The two short LLM calls at the end of Optimize (the advisor's three swap suggestions and the build explanation) are capped at 2048 tokens with no separate thinking budget. Under the 64k/32k Choya ceilings a thinking model routed through OpenRouter took minutes for each, and Optimize looked hung.
- A kit with no cleanse reports a rate of 0.0 instead of -0.0 in the viability report.
- A one-handed weapon only contributes the skills of the hand it is in, and a weapon skill carried by both weapon sets is one skill with one cooldown, usable on either set. A dagger in each hand of a Necromancer with a dagger on the second set was simulated as three separate Deathly Swarms, each with its own cooldown, which tripled that skill's damage and cleanse credit (found by the new cleanse trace in the probe, 2026-09-05).

## 1.11.24 - 2026-09-05

### Optimize

- Builds are now ranked by what they actually do, not by their stat sheet. Every candidate is played for 60 seconds on a dummy in a simulation that casts skills the way your radar asks (heals for a healer, stuns for control, damage for damage), and the score comes from what came out: strike and condition damage per second, healing per second, boons kept up, control landed, and effective health with the Protection the rotation really maintained. Before this, in PvE, the score never looked at a single skill: a bar with three empty utility slots scored exactly the same as a full one.
- Measured on the same Necromancer PvE Roamer run: the score rose from 0.58 to 0.82 of the ceiling, and emptying the utility bar now costs a third of the score instead of nothing.
- Exact ties in produced output are still broken by the stat direction of the radar, so two gear sets that do the same thing are ordered as before.
- Racial skills (Battle Roar, Shrapnel Mine, Reaper of Grenth, Healing Seed and the rest) are no longer offered as build skills. The optimizer does not know your character's race, no published build carries one, and the healer seeds that had picked Healing Seed healed for zero.
- The simulator runs about five times faster per evaluation, so the wider objective still finishes a search by patience well inside the time limit (Necromancer PvE: 42 rounds in 18 seconds).
- Fixed from an adversarial review of the new objective before it shipped: a utility could be proposed twice on one bar and its output counted twice; a weapon skill the radar made worthless (a bleed on a healer) pinned the simulation to that weapon set so the other set's heals were never cast; chill, cripple, weakness and slow on the enemy were lengthened by your boon duration instead of your condition duration; five stacks of Stability read as five times the uptime; every boon counted the same, so Swiftness plus Vigor beat Quickness; the scheduler spent stuns into a target with Stability. Builds that fail a viability check no longer pay for the flow simulation at all.
- Revenant elite-specialization swaps are no longer attempted by the search: a legend package cannot be rebuilt by the skill operators, so the candidate would have shipped with an empty bar. The seed's elite stands for Revenant until the search can carry legends.

## 1.11.23 - 2026-09-04

### Optimize

- Optimized PvE builds no longer come back with empty utility and elite slots. When the search moved a build to a different elite specialization (Reaper to Ritualist, for example), every skill the old specialization owned was removed and nothing put replacements in; in PvE the score does not look at skills at all, so the holes cost nothing and shipped. The swap now refills every slot it empties with the best eligible skill by the same scoring the initial build uses.
- Verified against the live game data: the exact in-game run (Necromancer, PvE, Roamer) that produced three empty utilities and no elite now returns Consume Conditions, Signet of Spite, Blood Is Power, Well of Power and Lich Form, at the same score.

## 1.11.22 - 2026-09-04

### Optimize

- The WvW/PvP cleanse check now counts cleanses from sigils, runes, relics and traits, not only from the skills on the bar. A Sigil of Cleansing was worth zero to it; a build that leaned on gear for cleansing was being sent back for repair over a shortfall it did not have.
- Self-applied Resistance now lowers the cleanse requirement in proportion to its uptime (capped at 75%), since Resistance ignores the non-damaging conditions that the requirement is mostly about.
- Repairing a build that fails the cleanse check can now change sigils and the heal skill, not just utilities.
- The search receipt in the log shows how far the best build is from passing a check it still fails, so a failing check that is getting closer is visible instead of silent.

## 1.11.21 - 2026-09-04

### Optimize

- Small choices were being starved. Elite-specialization swaps (three options) got roughly one look every six rounds while gear prefixes (hundreds of options) got dozens, because attention was handed out in proportion to how many options a category had. Each category now gets its own share every round, so the swaps that move a build the most are always considered.
- The "stop when it flattens" rule now waits at least one full rotation through every option before giving up, capped so it cannot stall for ten seconds on a large build. The build repair that runs before the search now respects the time limit and the Cancel button.
- The log now records how many candidates were generated, admitted and scored, how many beat the starting build, and which rounds improved it, so a search that finds nothing can be told apart from one that never looked.

## 1.11.20 - 2026-09-04

### Optimize

- The search now keeps going while it is still finding better builds and stops once it flattens, instead of quitting after a fixed number of tries. Measured in-game: it used to stop after three rounds; it now climbs for twenty or more when there is something to find.
- Every option is now actually considered. The old search scored only the first handful of choices from each category (the first few runes, the first few traits, the first few gear prefixes) and never looked further, no matter how much time it had. It now spreads its attention across all of them and rotates each round.
- Builds that fail a WvW/PvP viability check (too little cleanse, cannot survive the return fight) are repaired before the search begins, and both the repair and the search can now climb a failing check gradually instead of only noticing the moment it passes.

## 1.11.11 - 2026-09-04

### Choya Assist

- Choya was being told to guess. When you had a build equipped, the instructions said "do not call tools, edit that loadout" — so it named traits from memory rather than from the live game data, and a name that no longer exists gets the whole build thrown away. It now has to look up the real trait columns for every specialization it touches, whether you have a build equipped or not.
- The two-tool-call budget is gone. Checking three specializations' trait columns never fit in it. A few extra lookups cost seconds; a wrong name costs you the entire build.
- New standing rules: no name may come from memory, only from a tool result. Every specialization gets exactly one trait per column. And reasoning must be shown with mechanism and numbers — cooldown against uptime, internal cooldowns on sigil procs, whether a trait's trigger condition can actually be met, which skill applies the condition your rune boosts.
- The write-up now has to name the synergy chain concretely: what triggers what, on what cooldown, and the resulting uptime — plus the weakness the build accepts.
- Token ceilings raised: 16k to 64k per reply, and the thinking budget from 8k to 32k. A thinking model could previously spend half the budget deliberating and have too little left to answer with.

## 1.11.10 - 2026-09-04

### Choya Assist

- Choya gives you the build again. If it named one trait wrong out of the three in a specialization, the whole build was thrown away and you got only the write-up — the "expected 3 traits, got 2" note at the end of every reply. Its correct picks are now kept and the one column it fumbled is filled from game data, so the build lands. A specialization where nothing it named exists is still refused, so a wrong guess cannot be dressed up as a real build.
- When a build is refused, the log now names the trait or skill that failed instead of only reporting that a count came up short.

## 1.11.9 - 2026-09-04

### News

- Large images are downscaled instead of skipped. Stills wider or taller than 1024px are now shrunk before they are cached, so the overlay is handed an image it can actually use. The official blog's announcement art went from 1920x1080 and 1.9 MB to 1024x576 and 0.7 MB, and its texture from 8.3 MB of video memory to 2.4 MB. Images already small enough — YouTube thumbnails, for one — are left untouched rather than re-encoded.
- The download limit that was silently rejecting those images has been raised. It fails closed, so the pictures that most needed shrinking were never fetched in the first place.
- Truncated images are no longer cached. A download cut off mid-transfer used to be stored as a permanent half-grey thumbnail.

## 1.11.8 - 2026-09-04

### News

- Images are back. Article art, YouTube thumbnails and guide images had all stopped loading: the still-image allowlist only listed the hosts the feeds are *fetched from*, not the CDNs those feeds actually serve pictures from. YouTube round-robins thumbnails over i1-i4.ytimg.com, GuildJen serves through Jetpack Photon, and the official blog serves from a CloudFront distribution — none of them were admitted, so every image was discarded before it was ever downloaded.
- Official-blog images written as `//host/path` now resolve instead of being dropped.
- YouTube stills arrive as 16:9 instead of the letterboxed 4:3 crop, from one host instead of four.
- The News source picker moved into the left column of Settings. It used to stretch five checkboxes across the full window in four columns, three of them nearly empty.

### Themes

- Glacial Ward is the new default theme. An existing theme choice is untouched.
- The custom theme editor is rebuilt around one always-visible colour picker, with the five base colours beside it as a 2x3 grid of swatches — click a swatch to point the picker at it, then click the next. The three R/G/B number boxes per colour are gone, and the picker itself is about a third of its old area.
- Every base colour now says what it paints ("Headers, buttons, highlights, and the selected row"), translated into all 12 languages.
- Choosing Custom starts from the theme you were just looking at rather than a fixed palette. A custom theme you have already edited or named is never overwritten.
- The theme dropdown and the theme name share one line, and the name box only appears for a custom theme.

### Fixed

- Overlay text scale: several sections reset the font scale to 100% instead of the value on your Settings slider, silently shrinking everything drawn after them for anyone not on the default. Radio and the theme editor are corrected.

## 1.11.2 - 2026-09-01

### Radio

- Quip bubbles draw on top of everything else in the overlay, stay for 12 seconds, fade over 2, and float with a cartoon bob.

## 1.11.1 - 2026-09-01

### Themes

- Theme sweep: 59 remaining hardcoded colours across 14 files now follow the palette — the API status strip, the INFO sidebar, section bars, the whole spec-and-trait lock panel, settings headers and legends, progress banners, spinner dots, the chat background and the choya speech bubble. Meaning colours (errors, warnings, comparison green and blue, profession identity, heal and elite rims) deliberately stay fixed so they read the same in every theme.

## 1.11.0 - 2026-09-01

### Themes

- Five themes: Tyrian Gold, Glacial Ward, Verdant Wilds, Molten Ember and Void Orchid, each contrast-checked so text stays readable.
- Custom theme: name it and set five base colours; every other shade is derived from them. Live preview, and the choice persists.
- Settings has a Theme section, localized in all 12 languages.

### Radio

- The World genre loads on the tab's first open, and your last genre is remembered between sessions.
- Playing a favourite loads its genre into the station list and pins the live station into the results, so the controls row is always reachable.
- The volume slider is twice as long, and dragging it no longer re-tunes the station.

## 1.10.7 - 2026-09-01

### Radio

- A star or a music note anywhere in a song title no longer forces the whole ticker onto the blurry fallback font, which also turned accented characters into "?".
- Quip bubbles always sit above the choya's head. The low one used to land inside the now-playing ticker.
- Stations that fail on ports commonly blocked by antivirus or router firewalls now say so, naming the port and both likely culprits, in all 12 languages.

## 1.10.6 - 2026-09-01

### Interface

- Tabs renamed to Choya Assist and Choya Tunes, translated natively in all 12 languages.

## 1.10.5 - 2026-09-01

### Radio

- AI quips, opt-in: the choya asks your configured AI for short lines about the song playing, capped at 30 a day with 90 seconds between them, and only while the player bar is on screen. Canned genre-flavoured lines cover it when the feature is off or unavailable.
- Quips appear in a speech bubble with a tail and a pop-in, with mood emotes and a rare ON-AIR jackpot, roughly every 30 seconds to 2 minutes plus a greeting when you tune in.
- The now-playing ticker flows endlessly with a small dancing choya between repeats, and renders crisp at 42px.
- Playback controls move onto the active station's row while it plays; the slim player bar keeps LIVE, ticker, equalizer and DJ.
- The choya dances to a followable groove instead of twitching.
- Buffering status, doubled prefetch, and a retry on the station's raw URL when the resolved one fails.

## 1.9.2 - 2026-09-01

### Radio

- Sort combo no longer draws on top of the STATIONS header - it sits right-aligned inside the header bar.
- Dashes the game font cannot draw were showing as `?` in radio messages ("No favorites yet ? ...") - replaced with `-` in all 12 languages.

## 1.9.1 - 2026-08-31

### Radio

- Station rows breathe: logos twice as big (56px), taller rows, more space between them.
- Sort the results: Popular (directory votes), Name, Bitrate, or Country — combo next to the STATIONS header.
- Bitrate cap for poor connections: Any / 64 / 128 / 192 kbps on the filter row, applied server-side to every search.
- Choya DJ doubled in size and pops out of the player bar — feet on the bar, head over the station list — moved clear of the hearts column.

## 1.9.0 - 2026-08-31

### Radio

- Station logos: real favicons in the results and favorites lists (downloaded safely, cached on disk, capped at 200 per session so long sessions never bloat VRAM; letter plates remain the fallback).
- Pause with memory: short pauses resume instantly from the buffer; if the station dropped you while paused, resuming re-tunes seamlessly. The keybind now toggles pause/resume.
- Lower volume in combat: optional checkbox next to the volume slider — radio ducks to 30% with a smooth ramp when Mumble says you are in combat, and comes back after.
- AAC+ stations verified: they play their AAC core at correct pitch (missing only the highest frequencies); dropping them entirely would be worse than slightly-reduced fidelity.
- Long-session memory audit: every streaming buffer proven bounded (~1-2 MB flat regardless of hours played).

## 1.8.3 - 2026-08-31

### Radio
- Choya DJ moved into the player bar (right end, clipped to it) — no more covering the station hearts; the sprite EQ strip retired in favor of the real bars.

- Real equalizer bars behind the player bar: 24 log-spaced frequency bands measured from the decoded audio itself (not a fake animation), drawn in translucent gold so the status and now-playing text stay readable. Bars rise with the music and sink gracefully when playback stops. Their height tracks the stream, not the volume slider — turning the game-side volume down does not flatten the show.

## 1.8.2 - 2026-08-31

### Radio

- Language and Country filters on the search row, applied to every search (name and genre alike). Language defaults to Auto — it follows your overlay language, so an English UI surfaces English stations and a French one surfaces French. Both persist in config.
- Station rows use `-` separators instead of the bullet the game font cannot draw (no more `?`).

## 1.8.1 - 2026-08-31

### Radio

- Clicking a station no longer crashes: the tune-in built tokio timers on the audio thread outside any runtime context ("there is no reactor running"). The audio thread now enters the runtime for the whole connect sequence, pinned by a regression test.

## 1.8.0 - 2026-08-31

### Radio (new tab)

- Internet radio while you game: search 30,000+ stations (radio-browser.info) by name, genre, or country and play them in the background.
- Choya DJ mans the decks in the corner — sleeps when idle, dances while tuning, bobs on air, with an ON AIR badge and EQ bars.
- Now-playing song titles from the stream (ICY metadata), favorites saved to your config, volume with a proper log taper, optional keybind toggle (assign it in Nexus).
- Streams that stall reconnect once quietly; a lost audio device tells you instead of dying silently.
- HLS-only and undecodable-codec stations (OGG/FLAC/Opus) are filtered out rather than offered and failing.
- If a station will not connect, your antivirus or firewall may be blocking it — the error says so.

## 1.7.26 - 2026-08-30

### Combat

- WvW viability no longer treats a missing dummy HP as 0 (any spike used to pass the 30% gate).
- Prefix search builds the itemstat pool once per neighbour generation instead of three times.

### Overlay

- Fetching the official news feed no longer starts the 30-minute cache clock for YouTube, GuildJen, or the other sources.
- Settings language list uses English names for Chinese/Japanese/Korean until a CJK font is active (no more `????`).
- Font combo lists every Windows face we can find (Segoe, YaHei, Yu Gothic, Malgun), not only ones already loaded this session.
- Data-quality notes use `--` instead of an em-dash the game font cannot draw.

## 1.7.25 - 2026-08-30

### Combat

- WvW barrier now expires after 5 seconds and caps at 25% of max health.
- Interrupted skills use a 5 second cooldown instead of their full recharge.
- A killed dummy no longer keeps punching you for the rest of the window.
- Condition leftover fractions tick (1.5s pays 1.5 ticks, not 1 or 2).
- Large-scale WvW profiles with no kill target no longer invent 18k/24k/35k HP.
- Rotation scheduler values strike crit the same way the evaluator does.
- Corrupt converts the stripped boon; Steal grants it to you.

### Overlay

- News feed text no longer eats words after a bare `&`.
- A failed feed fetch keeps the last good items and retries in 45 seconds, not 30 minutes of empty.
- Lock/news counts use the active language's plural rule (English "21 locks", not "21 lock").
- Localized weapon labels accept Harpoon / Harpoon Gun like English does.

### Other

- Game-data refresh no longer reports success when the icon folder cannot be created.
- API key requests reject lookalike loopback hosts (`127.0.0.1.evil.com`).
- GuildJen scrape treats an empty or blocked index as down, not "done 0".
- Scraper redirects stay on the same host; Luminary is on the shared spec list.

## 1.7.24 - 2026-08-30

### Fixed

- Duplicate specialization selections are now rejected (two of the same spec no longer validate as a build).
- LLM advisor gear swaps can no longer write a prefix onto a slot the build does not wear (e.g. an off-hand prefix on a Greatsword build).
- `get_skill_info` tool no longer fuzzy-matches needles shorter than 5 characters — exact names still resolve.

### Removed

- Dead suggestion-card rendering code and an unused API model.

## 1.7.23 - 2026-08-30

### Overlay

- News, What's new, and other prose wrap to the pane instead of clipping at the right edge.
- First run and Reset layout open at ~80% of the monitor. Ultrawide uses a 1920-wide box so the overlay does not span the desk.

## 1.7.22 - 2026-08-30

### Changed

- Optimizer: Torment tick now blends stationary/moving by the Generic rotation profile's `movement_fraction` (PvE f=0.2, PvP f=0.5, WvW f=0.6). Condi scores on moving-target modes drop accordingly — PvE Torment @1000 condi: 121.8 → 113.84. No build ranking logic changed.

### Tests

- Referee: new pin `unset_viability_gates_pin_hardcoded_ehp_floors` locks the six hardcoded EHP floors (PvE 11000, PvP 8000, WvW Roam 15000, Havoc 13000, Zerg 10000, Staller 15000) for profiles with no `viability_gates` key; extended override test asserts gate notes show profile floors.
- Combat: Torment blend endpoint pins (f=0 stationary, f=1 moving) and updated PvE/PvP mode-dispatch expectations.

## 1.7.21 - 2026-08-30

### Overlay

- The ImGui draw hook stays registered on load, same as 1.7.20. Registering it from a PostRender callback mutates Nexus's render list while that list is being walked and takes the game down (heap 0xC0000374).
- Quick-access icon textures wait for a PostRender after a 2s settle. English/`auto` still does not load CJK faces.

## 1.7.20 - 2026-08-30

### Overlay

- Load panic no longer takes the game down. Unload keeps the DLL mapped until workers finish.
- Settings LLM keys stay masked on paste. Show/Hide still works.
- Copy says Copied only after Windows has the text.
- Ranch Load runs off the draw pass and does not persist notes on the click frame.
- Weapon lock on Current keeps the equipped prefix. Improve with a weapon lock keeps that prefix.
- Ungated Improve does not stamp Improved.
- Empty leftover kits stay Blocked, not Verified.
- No game data: lock panel says (Load game data first). No character: (Select a character first).
- Missing combat metrics read (not computed), not 0/0/0. Viability gates use readable names.
- Condi/boon duration on the panel includes trait duration, not only Expertise/Concentration.
- Optimized spec panel majors match English trait names when the overlay is de/fr.
- Spanish leftover chrome is translated. Dutch titles: ARMOR to RUSTING, STATS to STATISTIEKEN, BOONS to ZEGENINGEN.

### Improve, Choya, plates

- Alacrity is +25% recharge (10s to 8s), not the old 33%.
- Confusion 1s pulse is over-time; on-skill-use fires on activation.
- Live Might raises dummy condition ticks (player Might; dummy stays unbooned).
- Trait duration percent sits outside the Expertise/Concentration cap; skill-specific trait duration stays inside the Expertise cap.
- Dummy prot+stab cover is WvW only.
- Intensity stack cap is 1500 in every mode (no old PvP 100 bleed clamp).
- Set-2 sigils are copied from set 1. Land bars drop the aquatic palette. Land Spear stays a terrestrial two-hander.
- Giver's prefers three-stat itemstat 628, not 627.
- Ranger pets stay on chat plates. Revenant legends parse and win on the plate.
- Infer-from-heal warns on overwrite; rest-pad does not warn inferred-from-heal.
- Spec/item/prefix apply is exact or fuzzy only if the needle is 5+ characters. Skill resolve is full-name equal, not substring. Garbage trait names do not autofill.
- Gemini 403/429 with billing language is a billing issue, not a bad key. Gemini RPM persists across client create. Anthropic key check uses GET /v1/models and should not spend Messages quota. Cancel during a Gemini stream actually stops.
- Cancel a scrape: last-good benchmarks stay. Failed stills retry on Refresh. Dire is a word, not a substring of directly. Immobilized aliases to Immobile.
- Improve stamps the role profile id for viability gates, but JSON still has no ehp_floor numbers — live rank does not jump from EHP this build.

### News, About, mail

- Article bodies are capped. Stills only load from the allowlisted host.
- If an art worker dies mid-flight, leftover Pending URLs are released.
- Slavic languages get proper one/few/many story counts.
- Mailbag titles and POST bodies strip leftover encode tokens.
- Rate-limit row and wizard copy talk in remaining minutes.
- About wrap uses window-local X.
- Status poll bodies are capped at 1 MiB.

### Saves and data

- Save-build write errors surface. Windows overwrite uses ReplaceFileW.
- Unknown GearSlots keys survive a save/load round-trip.
- Hollow game-data (empty skills/items/traits) fails closed.
- Bulk GW2 5xx skips the rotten id. Oversize JSON bodies fail closed.

### Already on the feedback site

Admin GET list is read-only; marking read is a POST. Empty admin password or session secret fails closed.

### Not in this build

GW2 API key first-8/last-4 hint was reverted. Scraper heal/Luminary patches were reverted. Dummy Torment moving vs stationary and movement_fraction are not wired.

## 1.7.19 - 2026-08-29

Choya's mailbag is one box: type, Enter for a new line, select text and press B or click Bold/a colour. The flipped B icon is a real B now, and the row of `?` faces is gone — overlay fonts could not draw them. Formatting still shows in Looks like; the box itself is plain ImGui text.

## 1.7.18 - 2026-08-28

Mailbag has a row of faces (the BMP symbols Segoe UI can actually draw — not colour emoji; Nexus owns the font atlas, so the ImGui FreeType colour-glyph flags do not apply). A reply from the developer opens Messages and expands that row; the About tab still pulses if you were elsewhere.

## 1.7.17 - 2026-08-28

Choya's mailbag is a notepad bar: normal, bold, five text colours, bullets, numbers, left/center/right. You type plain sentences; Enter starts the next paragraph. Hover an icon for its name. Overlay fonts still have no real bold face — the preview doubles the ink by a pixel.

## 1.7.16 - 2026-08-28

Detail news stills default to 3× the old 240px cap, with a zoom slider and Reset. Article bodies (patch notes included) keep headings, nested bullets, and numbered lists instead of one cream brick. Choya's mailbag has H1/H2/H3, list, and number buttons plus a preview — overlay fonts still cannot bold.

## 1.7.15 - 2026-08-28

News filters are square icon buttons (all / article / notes / video / book). Hover shows the name and what it includes. Layout is Compact, Card, or Detail — radios, not a second capsule bar.

## 1.7.14 - 2026-08-28

Settings News sits in one full-width band: Desk / Magazine / Reader and Show stills on a single row, then four source columns. Cache and Benchmarks sit side by side under that, so the tab no longer needs a long scroll. The News desk list takes most of the pane so titles fit; stills keep their aspect (letterboxed, not stretched). Benchmark sync stamps a date even when a site returns nothing, restores counts from disk on load, and shows live progress instead of a stuck Syncing… / Never synced pair.

## 1.7.13 - 2026-08-28

News is a reading desk, not a stack of teaser cards. Settings groups sources by type (articles, patch notes, video, guides) and keeps stills on or off. The News tab filters by type, searches, and switches Desk (list + reader), Magazine (cells), or Reader (full article). Previous / Next, copy link, and open in browser sit on the article. YouTube is a thumbnail plus description — the overlay cannot play video.

## 1.7.12 - 2026-08-28

Clicking a news card opens the full article text from the feed (lists, headings, paragraphs), not the two-line "Read More" teaser. Compact cards still show the short blurb.

## 1.7.11 - 2026-08-28

Settings News also lists official forum announcements. Cards use the short RSS description (not the HTML dump), and tracking junk is stripped from links.

## 1.7.10 - 2026-08-28

Setup game-data now fills the wait with official Guild Wars 2 RSS cards. Click a card to read it; the others shrink. Settings has a News checklist (official, patch notes, ArenaNet YouTube, GuildJen). Tick any source and a News tab appears so you can sort a timeline or group by source.

## 1.7.9 - 2026-08-28

What's new on the About tab uses the same text size as the rest of the overlay, gold version headings, and cream body text. The full changelog scrolls instead of the last five notes.

## 1.7.8 - 2026-08-28

Gear locks live on the ARMOR / TRINKETS / WEAPONS sheet. Click a piece to pin it (bright blue name, gold ring — same language as a selected trait) or click again to release (dim grey). The old checkbox list under Locks is gone. Lock All / Unlock All still covers gear.

## 1.7.7 - 2026-08-28

Pet portraits sit in a padded 256px canvas, so they looked half-size next to skill icons. They now crop-zoom to fill the same box as utilities.

## 1.7.6 - 2026-08-28

Pet skills take less leftover width (~30% narrower); that space goes to the utility skills.

## 1.7.5 - 2026-08-28

The SKILLS card title is gone. That header row is now three areas — PET SKILLS, UTILITY SKILLS, ELITE SKILL — each with its own gold tick. Slots sit directly under those titles.

## 1.7.4 - 2026-08-28

The skill bar is three groups — PET SKILLS, UTILITY SKILLS, ELITE SKILL — with centered headers. Utility and elite squeeze so ranger pets keep a full two-line name (Siege / Turtle) instead of cutting off. Skill names center in the box, or wrap to two lines when they are more than one word.

## 1.7.3 - 2026-08-28

Overlay fonts cannot draw colour emoji, so viability rows showed `?` instead of pass/fail and `?1` instead of `>= 1`. Those are now `OK`/`NO` and `>=`. Ranger pets sit on the skill row with skill-sized icons, utilities and elite squeeze to the right, and the SKILLS header uses the same gold-tick title as the other cards. Choya "some Plaguedoctor" no longer paints every slot (emits `gear_slots` for the mixed pieces).

## 1.7.2 - 2026-08-28

**Ranger pets are part of the build.** Current and Optimized now keep the equipped terrestrial pets, resolve `/v2/pets` names instead of `#66`, and show pet chips (icon + hover) on the skill bar. Refresh game data once so the pet catalog and icons land.

Gear lock "Click to lock" tooltips only appear while the cursor is on that row, so they no longer follow the mouse off the overlay.

## 1.7.0 - 2026-08-26

**Per-slot gear prefixes — full hybrid builds.** Every weapon, armor piece, and trinket now carries its own single-stat prefix, individually chosen by the optimizer, lockable by you, and proposed by Choya. Berserker's weapons with a Cavalier's chest and Cleric's rings is now one optimize click.

- 16-slot gear model (6 armor, back + 2 accessories + amulet + 2 rings, two weapon sets' main/off hands) replaces the build-wide and group prefixes; old saves load unchanged and migrate automatically.
- The optimizer explores per-slot swaps alongside uniform and per-group moves, respecting per-piece gear locks (Locks panel, new Gear section).
- Choya can plate per-slot gear mixes; unknown prefixes fall back to your profile prefix with a warning.
- All four providers (OpenRouter, OpenAI, Anthropic, Gemini) now stream their responses.


## 1.6.4 - 2026-08-26

The foundational transport rework: **every provider now streams.** A shared LLM transport layer replaces four hand-rolled clients, the optimizer surfaces stale locks instead of silently overriding them, and the addon's largest file is split by responsibility. Twelve commits on top of the v1.6.3 hardening sweep, executed bedrock-up with per-layer verification gates.

### All four providers stream

- **OpenAI and OpenRouter** share one chat core (`llm::openai_compat`): identical wire types, one streaming implementation, one retry policy (408/504/529 retryable, `Retry-After` honored, rate-tracker handshake inside). The OpenAI provider picks up streaming and the 900-second budget — its old non-streaming client carried the exact false-timeout class v1.6.1 fixed for OpenRouter. Completion budget aligned to 16,384 tokens for both.
- **Anthropic Messages** streams: `read_anthropic_stream` assembles content blocks from the event sequence — `text_delta` concatenation, `tool_use` `input_json_delta` fragments stitched and parsed to JSON, `message_delta` stop reason, and in-band `error` events mapped to typed errors (`overloaded_error` → 529).
- **Gemini** streams via `streamGenerateContent?alt=sse`: text parts concatenate in arrival order, `functionCall` parts pass through whole, and `error` payloads map to typed errors.
- Callers are unchanged everywhere — each stream still lands as the same response type the flows already consumed.

### Shared transport bedrock

- `llm::sse` — the streaming reader (keep-alive skipping, delta accumulation, fragmented parallel tool-call merging) is shared infrastructure with its own test suite, ready for any future provider.
- `llm::response_cache::ResponseCache` — one TTL + size-cap cache replaces four inlined copies.
- `gw2api::transport::read_body_capped` — response bodies are capped everywhere: scraper 2 MiB, GW2 API 8 MiB, feedback client 1 MiB, so no endpoint can stream unbounded bytes into the game process.

### Optimizer

- A trait lock whose id no longer exists in the spec's trait rows (stale after a game-data refresh) is now **reported** through the data-quality reasons that already render in the comparison panel, instead of being silently replaced by the archetype-best pick. Regression-tested: stale ids warn, valid locks stay silent.
- Determinism sweep came back clean — the v1.6.3 amulet fix was the last map-fed float accumulation.

### Addon

- `optimization.rs` (2,645 lines, three responsibilities) is split: `chat_flow.rs` owns the Choya pipeline, `optimize_flow.rs` owns Optimize/Improve, and the shared suggestion vocabulary stays in `optimization.rs`. Pure moves.
- Chat history is written on a background thread (snapshot under the lock, atomic temp+rename write) — disk latency can no longer stall the frame.
- Clipboard copies retry three times against transient clipboard contention.

### Verification

- CI: `cargo fmt --check`, `cargo clippy -D warnings`, and the full workspace test suite run on every push to main and every PR (windows-latest). The workspace builds **warning-free**.
- 1,397 tests passing, including streaming regression tests for all four providers; the OpenRouter path was additionally verified against the live API (validate, streamed generate, cache, streamed tool loop).

### Install

Download `gw2_build_optimizer.dll` below and drop it into your `Guild Wars 2/addons/` folder (replace the old DLL), then restart the game or reload Nexus. Verify the SHA-256 against `SHA256SUMS.txt` if you like.

## 1.6.3 - 2026-08-26

A hardening sweep: an adversarial multi-agent review of the whole codebase (correctness, security, performance, lock discipline) and every confirmed finding fixed in one pass.

### Security

- The build-site scraper (Snowcrows / Hardstuck / GuildJen benchmark sync) accepted **any TLS certificate**. A hostile network could serve forged pages whose gear prefixes, runes, sigils, relics, and trait lists are parsed into the persistent benchmark cache that feeds optimizer comparisons and LLM prompts. Certificate validation is restored.
- The GuildJen scraper followed **absolute links from the index page verbatim**, so a crafted `href` could point the in-game process at an arbitrary https URL and persist whatever came back as a benchmark. Links are now pinned to `https://guildjen.com/` — only relative paths on the real host are followed.
- Benchmark cache filenames were composed from **scraped URL text** without sanitization; on Windows a crafted path component containing `\` or `..` could write outside the addon's `benchmarks/` folder. Filename components now go through the same whitelist as saved builds.
- Model-generated **build chat codes became clipboard chips on a bare `[&` prefix check**. A code now only becomes a chip if it base64-decodes to a `0x0D` build template with a profession byte and a sane length, so prompt-injected garbage cannot turn into a pasteable in-game link.
- The GW2 API client carried the account key but accepted an **absolute URL as any endpoint**. It now rejects non-loopback absolute endpoints outright (loopback stays allowed for the test suite).
- Scraper responses are capped at 2 MiB instead of slurping unbounded bodies into the game process.

### Correctness

- Four scraper text extractors sliced HTML at fixed byte offsets — a multi-byte UTF-8 character landing on the boundary **panicked the process**. All truncation now respects character boundaries.
- The LLM context-trim budget counted **bytes ÷ 4**; CJK text runs ~1 token per character, so localized (Chinese/Japanese/Korean) conversations could blow past the context window and get requests rejected instead of trimmed. Estimation is now script-aware: ASCII ≈ 4 chars/token, everything else ≈ 1 token/char.
- PvP amulet attributes were accumulated in **HashMap iteration order**, and f64 addition is order-sensitive — scores could diverge at the ULP level between runs, contradicting the optimizer's determinism guarantee. Accumulation is now key-sorted, matching every other accumulation site.
- The buff-profile lookup is now **locally guaranteed to return exactly 3 profiles** (truncate + pad) instead of relying on a distant embedded-data validation, so five hot-path `[0]/[1]/[2]` index sites cannot panic if that validation ever relaxes.
- During a GW2 API outage with an empty cache, the overlay **respawned the character and game-data loader threads every frame** with no cooldown. Retries are now gated to once per 30 s (characters) and 60 s (game data).
- The LLM response caches in all four providers were insert-only: expired entries were never evicted and the maps grew without bound for the life of the process. Inserts now sweep expired entries and cap the map at 64 responses.

### Maintenance

- Removed ~30 dead items (an abandoned gear-diff rendering subsystem and its helpers — about 1,900 lines) that predated the comparison-tab rework. `cargo build` is now **warning-free across the workspace**.

### Known follow-ups (not in this release)

- A trait lock whose id no longer exists in the spec's trait column is still silently replaced by the archetype-best pick; surfacing that warning requires threading a warnings channel through the beam search.
- The Gemini, OpenAI, and Anthropic providers remain non-streaming (the streaming/timeout class of fixes in 1.6.1 was OpenRouter-specific).

## 1.6.2 - 2026-08-26

Choya's replies can no longer stall the frame loop. The v1.6.1 streaming fix made model answers arrive — and much richer ones, since reasoning models finally get to finish thinking — but the moment a reply landed, the background thread ran the whole serving pass (stat attachment, build-code encoding, rotation simulation, chip building) while holding the shared state mutex. ImGui draws only on the render thread, and the render callback shares that mutex, so a slow serving pass read to Windows as the entire game not responding. Every step of the serving pass now runs without the lock: the state mutex is taken once for a microsecond read (live game DB and game mode), released for the heavy work, and taken again only to append the finished reply and suggestion. A Clear pressed while a reply is still being prepared now correctly drops the result instead of resurrecting it.

The Gemini, OpenAI, and Anthropic providers are unchanged in this release.

## 1.6.1 - 2026-08-26

Choya stopped going silent. Every OpenRouter request — chat, Optimize, and Improve — now streams its answer instead of waiting minutes for a single buffered payload, reasoning models get a dedicated thinking budget that cannot starve the actual reply, and transient gateway failures retry with backoff instead of killing the turn. If a model appeared to "stop responding entirely" — the request eventually failing with *Request timed out. Try a larger/faster model.* — that was this bug, and it is fixed.

### Why requests timed out

- The OpenRouter client sent one non-streaming `POST` per request and enforced a hard 180-second wall clock. Nothing arrived until the model finished the entire generation, so the connection sat completely idle while the model worked.
- Reasoning models such as `z-ai/glm-5.3-flash` spend minutes on hidden thinking before their first output byte — measured at 220 seconds on a real request. OpenRouter's own gateway also aborts non-streaming requests whose provider does not answer in time, returning 408/504. Either way the request died as a false timeout on exactly the models that think the most, while quick non-reasoning models kept working — which is why only *some* models broke.

### Streamed replies

- All chat completions now use `stream: true`. OpenRouter interleaves `: OPENROUTER PROCESSING` keep-alive comments so the connection never idles, and the first bytes land in seconds (2.4 s measured, down from 220 s to anything at all).
- The SSE parser follows OpenRouter's streaming contract: keep-alive comments and blank lines are skipped, content and tool-call deltas accumulate (parallel calls merged by index, fragmented JSON arguments stitched back together), `[DONE]` ends the stream, and the usage-bearing final chunk is tolerated.
- Mid-stream failures arrive inside a 200 response, so they are now recognized in-band: a top-level `error` payload or `finish_reason: "error"` maps to the same typed messages as before (rate limited, billing, timeout, overloaded) instead of surfacing as a confusing empty reply.

### Answers no longer starved by thinking

- Reasoning models share one completion budget between hidden thinking and the visible answer. The old flat `max_tokens: 8192` let thinking consume the entire budget and return an empty message with `finish_reason: length` — reproduced live with 0 characters of content.
- Requests now cap hidden reasoning at 8,192 tokens (`reasoning.max_tokens`) inside a 16,384-token completion ceiling, so a long thinking phase leaves room for the build JSON and its explanation. Providers without thinking support ignore the parameter.

### Smarter retries and timeouts

- Gateway timeouts are now retryable: 408, 504, and 529 join the existing 500/502/503 retry list. Previously a single OpenRouter 408 killed the turn immediately with no retry at all.
- `Retry-After` is honored when OpenRouter sends one (capped at 60 seconds) instead of always guessing 5/10-second backoff.
- The connection budget grew from a 180-second whole-request kill to 900 seconds total with a 15-second connect timeout — a stalled request still fails, but a model that legitimately thinks for minutes now finishes.

### Tool-call routing

- Any request that carries function-calling tools now sets `provider.require_parameters: true`, so OpenRouter only routes to endpoints that implement tools natively — never to one that would fake them through a prompt template and return unparseable pseudo tool calls.

### Tests

- Five new unit tests cover the SSE reader: keep-alive skipping, content accumulation, fragmented parallel tool-call merging, mid-stream error mapping, and empty-stream finish-reason reporting; the existing tool-call round-trip test now exercises the streaming path.
- OpenRouter joined the live provider suite (`test_openrouter_validate_and_generate`): key validation, streamed generation, response caching, and a real streamed tool loop run against the production API. Full workspace suite: 785 passing.
- The Gemini, OpenAI, and Anthropic providers are unchanged in this release.

## 1.6.0 - 2026-08-25

New About tab. What's new shows the release notes for the last five versions in game. Message developer is a short guided form for a bug, a wrong build, a suggestion, a question, or a fistbump for Choya; each message shows its status (received, read, answered, closed) and the reply inline, refreshed on tab open and every five minutes, and the About pill pulses when an answer lands. Failed sends are kept locally with Resend, and nothing typed is lost. Ko-fi link in the header and on the first form step. Privacy: messages carry the category, choices, text, addon version, game build, language, mode/scale/role, profession and elite spec, and the AI provider name; a contact line, the account name, and a slim copy of the last optimize result are opt-in; API keys and character names are never sent.

## 1.5.3 - 2026-08-24

The rotation scheduler now values Fury by game mode. It priced every Fury application at the PvE +25% crit bonus, so Fury-granting skills were overvalued by a quarter when ordering PvP and WvW rotations; they now use the +20% those modes actually grant. A dropped  attribute in the i18n suite is restored, so named-placeholder substitution is verified again.

## 1.5.2 - 2026-08-24

A locked specialization now returns the best lock-respecting candidate even when no build passes every viability gate. The result stays on the locked spec and is marked provisional with the failed requirements instead of erroring out or swapping to another elite.

## 1.5.1 - 2026-08-23

Empty `gear_groups` now inherit the build prefix for armor, trinkets, and weapons, and every sheet path counts only the active land weapon set. Legacy saves with blank groups fill from `stat_prefix` on load. Empty inherited Strong and explicit all-Strong both lock at Power 2556 / Precision 2186 for the Ranger axe/axe fixture.

## 1.5.0 - 2026-08-23

This release moves WvW optimization away from isolated attribute totals and toward a mode-aware, two-sided exchange timeline. It also carries forward the v1.4.18 character-sheet corrections: passive attributes remain values a player can reproduce in Guild Wars 2, while temporary effects, modeled output, and incomplete mechanics are shown separately and labeled according to data quality.

### Reproducible character-sheet values

- The primary Stats comparison remains grounded in the level-80 Hero-panel attributes, profession health tier, armor-weight defense, equipped gear, active land weapon set, runes, and permanent sourced adjustments.
- Tooltip coefficients such as barrier amounts, direct heals, and life-siphon values are rejected by the shared permanent-stat classifier instead of inflating Power or Healing Power.
- Character-sheet calculation, optimizer scoring, synergy extraction, and model-facing build data use the same classification rules so a rejected tooltip amount cannot continue influencing search through another parser.
- PvE and competitive variants of duplicated attribute and percentage facts are resolved by the selected game mode instead of being stacked together.
- Synthetic evaluation values remain separate from visible attributes. The optimizer does not present internal scoring aids as numbers a player should expect to see in the Hero panel.

### Mode-aware balance and timing data

- Added an active 2026-07-15 balance manifest and patch ledger with explicit PvE, PvP, and WvW override files. The earlier 2026-01-13 manifest is retained as the inherited, superseded baseline.
- Rotation construction now receives the selected game mode. Activation time, recharge, resource cost, status duration, and combo-field duration can therefore resolve to different sourced values in PvE, PvP, and WvW.
- The first exact timing slice covers Black Powder, Heartseeker, and Steal, including competitive initiative and coefficient splits where the published values differ.
- Unsupported or ambiguous facts are not silently promoted to exact mechanics. They remain omitted from exact calculations or are surfaced as `Provisional` while source coverage is expanded.

### WvW exchange timeline

- WvW candidates are evaluated on a two-sided timeline with committed casts, incoming actions, interrupts, control, boon removal, defensive layers, recovery, and an exit instead of being ranked primarily by average dummy output.
- Secured sequences may be created by one applicable layer: control ownership, timed Stability, evade, block, invulnerability, or stealth. The evaluator does not require control and defensive cover simultaneously.
- Aegis and Blind are charge-based safeguards. Their mere presence does not create a multi-second protected interval; they preserve only the action or event they actually answer.
- Sequence completion now measures whether meaningful actions execute without interruption during secured slices. A generic mobility label no longer invents timed evade or stealth coverage.
- Enemy actions respect control state, and pending player actions can be interrupted. This makes ordering, short overlaps, and recovery materially affect the report.
- The WvW report exposes sequence completion, protected pressure, target threshold progress, tempo, sustain margin, exit availability, resource legality, and unmodeled-effect counts for ranking and diagnostics.

### Profession mechanics and resource legality

- Profession/F-slot skills can participate in the candidate and timeline instead of limiting evaluation to heal, utility, elite, and weapon actions.
- Added a bounded resource ledger for Initiative, Adrenaline, Energy, Illusions, and Blades. Higher-priority actions are skipped when their known cost cannot be paid, costs are spent at cast start, landed-hit gains respect caps, and over-cap gains are discarded.
- Resource accounting is intentionally a legality guard rather than a complete profession simulator. Attunements, legends, shroud, heat, life force, pets, kits, and other full state machines are outside this release.
- Weapon swaps use the candidate's actual weapon sets and sigils in search identity, while the passive character sheet continues to count only the active land set.

### Search, weighting, and grouped equipment

- WvW search ranking now reads the exchange report instead of leading with legacy dummy DPS.
- Role selection and fine-tune weights are propagated into candidate ranking so Condition, Control, Sustain, support, and direct-pressure preferences can influence which legal build is retained.
- Candidate identity includes weapon sets, sigils, elite specialization, and profession actions, preventing materially different loadouts from being discarded as duplicates.
- Mixed equipment is represented in grouped armor, trinket, and weapon prefixes. Search reachability was corrected so user-weighted alternatives can enter and survive the beam instead of repeatedly collapsing toward one direct-pressure prefix.
- Gate-repair neighbors remain available for candidates missing required defenses or utility, but passing viability does not override the user's selected role and weights.

### Corrected interaction rules

- Stability on the target prevents applicable control until it is removed; ordered removal can therefore change whether the following control action lands.
- Blind and Aegis no longer answer condition application, and their treatment is separated from evade and invulnerability when evaluating incoming actions.
- Resistance and condition application use their own timeline rules rather than borrowing strike-avoidance behavior.
- Incoming condition ticks use the shared condition-damage calculation instead of a max-health percentage stub.
- Might uses the mode-aware boon table rather than a fixed invented attribute increase.
- Combo fields carry explicit durations. Smoke, water, light, and fire finishers use the supported result for their finisher type; unsupported combinations are marked unmodeled instead of receiving a convenient fallback effect.
- Stealth is timed cover for relevant execution windows, not blanket immunity, and a mobility classification alone no longer grants a hardcoded duration.

### UI, reproducibility, and data quality

- Optimized gear display, comparison rows, save/load presentation, and generated build details preserve grouped-prefix and active-set choices so the shown result can be reconstructed instead of appearing as an unexplained aggregate.
- Mode-aware rotation previews use the visible candidate stats and selected mode rather than silently falling back to a generic PvE simulation.
- Build reports distinguish verified source-backed facts from provisional modeled behavior and count unmodeled sources for diagnosis.
- Exact passive attributes, simulated rotation output, boons, conditions, control, cleanses, and defensive utility remain separate concepts in the UI.

### Runtime and cancellation

- Long optimization work now observes cancellation and runtime limits throughout candidate generation and evaluation instead of only between coarse phases.
- Mode-aware evaluation is reused by search and addon previews, reducing disagreement between the candidate that was ranked and the result shown to the player.

### Validation and current boundaries

- Regression coverage includes the v1.4.18 Ranger character-sheet values, tooltip-stat rejection, competitive percentage classification, mode-isolated timing overrides, charge-versus-duration cover, interruption, ordered removal and control, resource paywalls, grouped-gear search identity, and user-weight reachability.
- The active data slice is not a claim that every Guild Wars 2 trait, skill, rune, sigil, relic, or profession mechanic is now exact. Exact competitive coverage is being expanded incrementally from authoritative sources.
- Timings and effects without a supported mode-specific fact remain `Provisional` or unmodeled. The optimizer will prefer an explicit gap over inventing a duration, coefficient, or interaction.
- Rotation results are deterministic model output, not a live combat record. Player positioning, facing, movement, opponent decisions, and profession state machines not listed above still require further modeling.
- Conditional damage thresholds are represented in the sourced balance layer, but complete conditional execution coverage continues to be expanded. A stored source value is not treated as active unless the timeline can justify its condition.

## 1.4.18 - 2026-08-23

This release replaces several optimistic stat assumptions with sourced, mode-aware character-sheet rules. Its first priority is trust: values shown as attributes should be values a player can reproduce in Guild Wars 2, while modeled rotation output is identified separately.

### Player-visible stat presentation

- The Stats pane now leads with the nine level-80 Hero-panel attributes: Power, Precision, Toughness, Vitality, Condition Damage, Expertise, Concentration, Ferocity, and Healing Power.
- Health and Armor remain visible derived Hero-panel values.
- Removed synthetic `Effective Power`, `Effective HP`, and `Healing Index` from the primary comparison. Those internal scoring aids are not character-sheet attributes and should not look like in-game values.
- Removed the synthetic three-scenario damage table from the primary attribute comparison. Rotation output remains labeled as a simulation rather than a live combat record.
- Boons, conditions, cleanses, stability, skill use, and viability remain separated from passive attributes so temporary effects are not presented as permanent gear stats.

### Permanent attribute corrections

- Tooltip coefficients no longer become permanent attributes. This fixes values such as barrier amounts, direct healing amounts, life-siphon damage, and life-siphon healing being added to Power or Healing Power.
- The same permanent-stat rule is shared by character-sheet calculation, synergy extraction, and the optimizer's LLM tool data. A tooltip amount can no longer disappear from the UI while still biasing candidate selection through another parser.
- Conditional named adjustments such as `Additional Power` are excluded from the unbuffed panel. They belong to timed activation modeling.
- Trait `BuffConversion` facts remain supported. For example, Wellspring's Power-to-Healing-Power conversion still changes the visible passive result.

### Game-mode corrections

- Known duplicated `AttributeAdjust` rows are resolved by trait, target attribute, and selected game mode rather than being summed.
- Lingering Magic now contributes 240 Concentration in PvE and 120 in PvP/WvW, instead of adding both API rows.
- Known conditional or pet-only duplicated rows are excluded from the standing player panel.
- Unknown duplicated rows are omitted instead of guessed. Missing data is safer than a confidently inflated attribute.
- Percentage tooltip facts now share one classifier between combat and synergy parsing.
- Tooltip-only 100% critical-chance facts, recharge reductions, incoming-damage reductions, and other defensive percentages no longer become outgoing damage multipliers.
- Two-value percentage pairs within one trait collapse to one mode value instead of stacking simultaneously. Conditional timing remains provisional and is handled separately from the passive panel.

### Equipment corrections

- Only the active land weapon set's two sigils contribute to the standing stat/modifier calculation. The second set is reserved for weapon-swap timeline handling.
- Rune tier bonuses continue to be summed from all six equipped tiers.
- Level-80 base attributes remain 1,000 for Power, Precision, Toughness, and Vitality, and 0 for the five secondary attributes.

### Profession and armor baselines

The optimizer uses separate sourced profession health and ascended-armor defense values:

| Baseline | Value | Professions |
| --- | ---: | --- |
| High base health | 9,212 | Warrior, Necromancer |
| Medium base health | 5,922 | Revenant, Engineer, Ranger, Mesmer |
| Low base health | 1,645 | Guardian, Thief, Elementalist |
| Heavy armor defense | 1,271 | Warrior, Guardian, Revenant |
| Medium armor defense | 1,118 | Engineer, Ranger, Thief |
| Light armor defense | 967 | Elementalist, Mesmer, Necromancer |

Visible Health is `profession base health + 10 × Vitality`. Visible Armor is `armor-set defense + Toughness`. Armor weight does not change the attribute budget of an otherwise identical armor prefix.

Sources: [Attribute](https://wiki.guildwars2.com/wiki/Attribute), [Health](https://wiki.guildwars2.com/wiki/Health), and [Armor](https://wiki.guildwars2.com/wiki/Armor).

### Reproduced failure and corrected result

The original Ranger case reached 7,156 Power and 13,696 Healing Power because effect coefficients were treated as passive stats. The exact offending tooltip amounts were removed from every permanent-stat consumer.

The cached WvW reproduction now resolves the tested optimized candidate to:

| Attribute | Value | Exact source |
| --- | ---: | --- |
| Power | 3,058 | level-80 base + Strong ascended gear + six Rune of Infiltration tiers |
| Precision | 2,544 | level-80 base + Strong ascended gear + six Rune of Infiltration tiers |
| Toughness | 1,000 | level-80 base |
| Vitality | 1,240 | level-80 base + Natural Fortitude |
| Concentration | 120 | Lingering Magic, WvW value |
| Healing Power | 214 | Wellspring: 7% of 3,058 Power, rounded for display |

The Rune of Infiltration contribution is exactly +175 Power and +225 Precision from its six API bonus lines.

### Validation

- All optimizer tests pass: 735 passed, 1 network test ignored.
- All 39 math permutation tests pass.
- All 29 objective-profile integration tests pass.
- Addon compilation passes.
- Clippy completes successfully; existing repository warnings remain non-blocking.

### Known boundaries

- Rotation output is still a model, not a live combat log. It remains labeled as simulated.
- Conditional trait and upgrade effects require timeline activation rules before their contribution can be called exact.
- A shield's separate defense rating is not yet added to the optimized Hero-panel Armor value. The armor-weight baseline itself is exact.
- Food, utility consumables, temporary boons, and triggered upgrade effects are intentionally not folded into the unbuffed attribute table.
