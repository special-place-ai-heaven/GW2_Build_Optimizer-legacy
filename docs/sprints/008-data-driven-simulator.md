# Sprint 008: data-driven simulator, beat meta

Read `docs/doctrine.md` first. Every gate below is a number the CI or an
offline instrument prints. A gate passes when the number is met; nothing is
"done" by report.

## Gate 0: baseline

- 1.14.39 committed, pushed, released (DLL + SHA256SUMS). Sprint starts from
  a tagged tree.

## Gate 1: effect records complete and real

Source of truth: GW2 API facts + wiki pages, written as
`data/normalized_effects/<date>/{wvw,pve,pvp}.json` records.

| Number | Now | Target |
|---|---|---|
| Minor traits with a record / executable (not a placeholder) | 209 / 243, executable 95, abstaining 10 (2026-09-23: Ele 22/1/4, Engi 18/2/7, Guardian 17/3/7, Ranger 14/4/9 exec/abst/cov of 27, none 0) | 243 / 243 executable |
| Major traits with a record / executable | 529 / 729, executable 121 | 729 / 729 executable |
| Rune tier lines / sigils / relics executable | 2 of 642 lines / 8 of 81 / 4 of 128 | all |
| Profession-mechanic skills / elite skills executable | 1 of 383 / 1 of 133 | all with a proc or effect |
| Coverage placeholders still typed `Passive` (schema artifact) | 0 (582 migrated to NotApplicable, 2026-09-22) | 0, guarded by validation rule 15 |
| Minor-trait records that are flat "Passive" but the wiki describes a trigger, interval, combat state or weapon condition | 0 for Ele/Engi/Guardian/Ranger (every remaining coverage block names the trigger or field the format lacks); other four professions not yet read | 0 (each is either genuinely passive, with the wiki line cited, or a real trigger record) |
| Rune, sigil, relic effects modelled as records (not text parse) | 2 / 4 / 1 records | every tier bonus and proc that the wiki lists |
| Records lacking duration, ICD or stacking where the wiki gives one | measure | 0 |

Acceptance: `calibrate_viability` per mode plus a new `effect_coverage`
example that prints the table above; the corpus suite gains a test that the
counts do not regress.

Order: Elementalist, Engineer, Guardian, Ranger (the four that abstain), then
Warrior, Mesmer, Revenant, Thief; Necromancer is the reference (27/27 with a
record; 10/27 executable, so it is a reference for record style, not for the
number).

Increment log:

- 2026-09-23, minors for Ele/Engi/Guardian/Ranger: 779 -> 889 WvW records,
  582 -> 581 coverage blocks (ratchet in `gate1_coverage_only_ratchets_down`).
  `effect_coverage` prints per-profession exec/abst/cov/none and, with a
  profession argument, one line per trait with the abstain reason. Review
  (`review_minor_records`, 183 records) found the WvW numbers right, nine
  data findings (fixed) and one engine bug (fixed: a trait's parsed always-on
  Percent fact and its executed Conditional record both applied; the timeline
  now divides out the trait's share, test
  `wvw_params_divides_out_trait_percent_with_executed_conditional`).

- 2026-09-23, engine increment: forms in the flow sim. Shroud and Celestial
  Avatar are now data-driven timed states (`data/formulas/shroud.json` tagged
  by elite spec, new `data/formulas/forms.json` from the wiki); `SimParams.form`
  replaces the weapon bar on entry, blocks weapon swap, keeps utilities live,
  and enters/exits by pool floor and best-skill comparison rather than a fixed
  schedule. Trait records with `OnShroudEnter`/`OnShroudExit` and in-shroud
  `Periodic` triggers now fire for equipped traits (boons and life force only).
  `builder.rs` renamed `shroud_bar_for_build` to `form_bar_for_build` and fixed
  underwater-twin, off-chain-flip and `Downed_N` slot handling. Balance
  overrides added for Executioner's Scythe, Devouring Cut and Voracious Arc
  from the wiki. Measured on the player's own golem logs: Reaper `dps_engaged`
  error -0.716 -> -0.671, `skill_share` TVD 0.647 -> 0.472; Druid -0.863 ->
  -0.859. Reaper self Fury/Quickness unchanged (PvE record file has no
  shroud-trigger records, gap E12). `calibrate` WvW viable 116 -> 118,
  StabilityAccess 136 -> 138, nothing fell.

- 2026-09-23, Fidelity: burst and condition observables. `log_compare` gained
  five observables per player: `burst_peak_5s` / `burst_peak_10s` (peak
  damage in any 5s / 10s window; log side from EI `damage1S` cumulative
  totals, sim side a new per-second capture in the flow simulation),
  `burst_overlap_share` (share of damage dealt while Quickness and Fury are
  both present; log side samples `buffUptimes[].states` at each second's
  midpoint), `condition_share` (`dpsAll actorCondiDamage / actorDamage`,
  minions excluded; the older `condi_fraction` counted pet damage and stays
  as is), `condition_ramp_s` (seconds until condition damage per second first
  reaches 80% of its median; abstains below `condition_share` 0.2). Each side
  abstains by name when it lacks the data; a missing log value prints NaN,
  never 0. `SimulationResult` carries per-second strike/condition damage and
  per-second boon presence; scheduling never reads them; a test checks
  per-second sums equal the totals. Measured on the player's Reaper golem
  log: `burst_peak_5s` 18856 log vs 8989 sim (-0.523), `burst_peak_10s` 15392
  vs 7131 (-0.537), `burst_overlap_share` 0.888 vs 0.496 (-0.391); the log's
  best five seconds run 1.6x its engaged average. Druid golem:
  `condition_share` 0.921 vs 0.726, `condition_ramp_s` 9 vs 7, overlap 0 on
  both sides (the log has no Quickness). WvW Druid 210742: `burst_peak_5s`
  4652 vs 2424, `condition_share` 0.891 vs 0.755, ramp 6 vs 10. The three
  committed fixtures were trimmed before these fields existed, so overlap,
  `condition_share` and ramp abstain on them; a fixture refresh is in
  progress.

- 2026-09-23, data increment: Necromancer PvE shroud and fear trigger
  records. 30 records added to `data/normalized_effects/2026-01-13/pve.json`,
  each citing its wiki line (wiki text saved under
  `GW2_Build_Optimizer-work/necro-pve-2026-09-23/wiki/`). Shroud enter (11):
  Awaken the Pain, Armored Shroud, Shrouded Removal, Furious Demise, Speed of
  Shadows x4, Soul Barbs x2, Eternal Life. Shroud exit (5): Life from Death,
  Unholy Martyr x2, Soul Barbs x2. In shroud (6): Shrouded Removal every 3s,
  Vampiric Presence x2, Death Perception, Reaper's Onslaught x2. Shroud skill
  1 (3): Reaper's Might, Unyielding Blast, Dhuumfire. On fear (4): Dread x3
  (Fury 10s, Quickness 5s, +20% strike 2s, 1s ICD), Fear of Death. On
  condition removed (1): Shrouded Removal. Finding: on the player's Reaper
  (Spite: Bitter Chill, Spiteful Fortitude, Dread; Death Magic: Shrouded
  Removal, Dark Defense, Corrupter's Fervor; Reaper: Chilling Nova, Decimate
  Defenses, Blighter's Boon), Fury and Quickness come from Dread on fear and
  the "Chilled to the Bone!" shout, not from the shroud traits. Consumer:
  `engine::form_for_build` also loads scoped `OnSkillUse` and
  `OnConditionApplied` records (boons and life force only) into
  `FormSpec::triggered` with ICD and in-shroud condition; simulator
  `fire_triggered` runs on cast, on condition applied, and on Fear/Taunt
  landed; `skill_scope_admits` is shared by both simulators
  (`wvw_timeline.rs`). Measured, Reaper golem: `dps_engaged` 4814 -> 5197
  (error -0.589 -> -0.556), Fury 0.383 -> 0.705 (log 0.993), Quickness
  0.383 -> 0.515 (log 0.830), Might 6.6 -> 8.0 (log 19.0),
  `burst_overlap_share` 0.496 -> 0.622 (log 0.888), `burst_peak_10s`
  7131 -> 7300 (log 15392). Side effect: Druid WvW rows self Fury
  0.00 -> 0.30 (logs 0.52-0.85) from the existing WvW Ranger Survival-skill
  record. `effect_coverage` Necromancer unchanged (it reports each trait's
  best verdict across modes; the shroud traits were already executable
  through their WvW records). `calibrate` PvE identical; WvW
  ProtectedExecution 55 -> 56, nothing fell.

- 2026-09-23, engine increment: auto-attack chains (E11). Auto-attack chains
  cycle in the flow sim and the WvW timeline. `engine::add_weapon_skill_ids`
  follows each weapon skill's `next_chain` and adds the later steps;
  `rotation::AutoChain` derives each step's place from the API `next_chain`
  data; both simulators play the chain's next step after an auto and reset to
  step 1 after any non-auto cast, an interrupted cast, or a gap longer than
  `CHAIN_CONTINUE_SLACK_MS` (reaction delay 180 ms + one 100 ms tick; the wiki
  Chain page gives no time figure, only the interrupt/other-skill reset rule,
  cited in the constant's doc). A chain whose next step is missing from the db
  lands on the gap line as "<name> auto chain (step N missing)"; none do
  today (118 chained skills all resolve). Four new simulator tests; no
  re-pins. Measured: Reaper golem `skill_share` TVD 0.477 -> 0.355 (Life
  Slash 0.144 and Life Reap 0.105 now cast; log 0.064 / 0.056), `dps_engaged`
  error -0.556 -> -0.529, `burst_peak_5s` unchanged at 8989 (each non-auto
  between autos resets the chain, as in the log). Druid and Willbender rows
  unchanged (no chained autos cast). Fixture Reaper `skill_share` 0.549 ->
  0.557: the sim over-uses greatsword autos that the log never casts, a
  weapon-choice gap, not a chain gap. `calibrate` WvW/PvE gate counts
  unchanged; `effect_coverage` identical. Data: WvW record `trait:892:0`
  (Fear of Death) gains its wiki 5s internal cooldown (the cooldown applies
  to the life-force gain in every mode). Closes the first half of gap E17;
  Dhuumfire per-spec values still need an elite-spec gate.

- 2026-09-23, Fidelity: stated gear provenance. `log_compare` `codes.json`
  entries may be an object `{"code": "[&...]", "gear": {armor, weapons,
  trinkets, sigils: {"Greatsword": ["Hydromancy","Rage"], ...}, rune, relic,
  food, utility}}`, every key optional, unknown keys are a parse error. New
  `Provenance::Stated` for gear; precedence Account (addon cache) > Stated >
  Corpus; mixed groups print like `Stated(armor,weapons,sigils)+Corpus
  (trinkets,rune,relic)`. Prefixes resolve through `db.itemstat_by_name`;
  sigils/rune/relic by name with or without the "Superior Sigil of" /
  "Superior Rune of the" / "Relic of the" part; unresolvable names abstain by
  name in the stat flags. The stated prefix also steers the corpus neighbour
  choice. Fixtures README documents the format. Measured with the player's
  stated Willbender kit (Dragon's armor and weapons, Marauder trinkets,
  Hydromancy+Rage greatsword, Bloodlust pistol, Fire focus, Scholar runes,
  Relic of the Brawler): golem 20260923-204811 `dps_engaged` sim 8306 -> 7863
  (log 11343; error -0.27 -> -0.31, the corpus guess had been Berserker
  gear), `burst_peak_5s` 16146 -> 14921 (log 16769), `burst_overlap_share`
  0.02 (log 0.31), Quickness self 0.10 (log 0.38); WvW 221546/221837
  `dps_engaged` 2416 -> 3364 (log 4149/4150), `burst_peak_5s` 3861 -> 5462
  (log 10053/8699), overlap 0.06 (log 0.67/0.80). Real traits plus real gear
  leave a pure engine gap: Quickness upkeep and trigger records for a build
  without a form (E15), and consumables (E19).

- 2026-09-23, data: consumables in the cache and the kit. Item download keeps
  Consumable items of detail type Food or Utility at level 80 (340 items on
  this machine: 263 Food, 77 Utility; `items.json` 17296 -> 17636 rows, build
  stamp unchanged). New `ItemDetails.description` carries the tooltip.
  Refresh runs a one-off consumable backfill when `items.json` predates this
  and has no Food/Utility row (`download.rs` `download_steps` after the items
  step; ~285 bulk requests, 30-90 s, cancellable between request groups,
  progress shown as "Items (food and utility)"); it never repeats once rows
  exist. Example `backfill_consumables` for a manual run. Closes gap E19.
  `consumables.rs` parses tooltip lines: "+N Stat" flat bonuses; "Gain X
  equal to P% of your Y" becomes a `StatConversion` applied on the stat sheet
  (Superior Sharpening Stone: Power +3% of Precision and +6% of Ferocity);
  experience/magic find/karma/gold lines ignored; triggered or unrecognised
  lines named in a flag "stated food line '...' not modeled". Known
  shortcut: conversions read the stat sheet after trait conversions. The
  optimizer's automatic food/utility pick does not score conversions yet
  (new gap E20). `kit.rs`: stated food/utility resolve by exact name, then
  unique case-insensitive substring; ambiguous text ("Sharpening Stone"
  alone) is refused with a named flag. Measured, Hammerhand golem
  20260923-204811: `dps_engaged` 7863 -> 8741 (log 11343, error -0.31 ->
  -0.23), `burst_peak_5s` 14921 -> 16647 (log 16769, within 1%). "Not in
  game data" flags across all own logs 12 -> 0. `calibrate` gate counts
  unchanged; fixture run identical.

- 2026-09-23, engine increment: trigger records for every build, timed
  modifiers (E15, E16). The trait-record loader moved to `engine.rs`
  (`equipped_trait_records`, `flow_record`, `trait_procs_for_build`); it
  reads record fields only. `form_for_build` takes only form-owned records
  (enter, exit, in-shroud periodic, while_in). `SimParams` gains `triggered`,
  `strike_add`, `condition_add`, `folded`. Triggered records fire for every
  build, with or without a form; the in-shroud requirement is an `in_form`
  gate only forms meet. Hosted triggers: on skill use (scoped), on condition
  applied, on hit (landed strikes), periodic. `FormSpec::triggered` is gone.
  Two proc kinds: Condition (through the skill condition path, duration and
  stack cap, fires on-condition records, nesting capped at 2) and Modifier
  (timed stack with expiry, `max_stacks`, refresh-all per record). Modifiers
  are additive or multiplicative per `modifier_buckets.json`; crit damage
  uses the formula ferocity-per-point. Always-on shares the fact parser
  already folded (Soul Barbs +10%, Death Perception crit damage) are
  tracked by `TraitStanding` and removed once in the flow sim so nothing
  counts twice. WvW timeline untouched. Abstains by name on the gap line as
  "<name> <trigger> (flow sim: reason)": on-crit (the flow sim averages
  crits), on-dodge, on-attunement-swap, on-boon-applied, on-disable-foe;
  records with gates, scale, foe/attunement prerequisite, proc chance,
  trait-skill cast or non-player actor; heal/cleanse/strip payloads;
  modifiers without duration; life-force or shroud records on a build with
  no form. PvE has no gap line yet (residue: add one). Measured: Reaper
  golem `dps_engaged` 5519 -> 5572, `burst_peak_5s` 8989 -> 9381 (Dread
  +20% after fear); WvW Willbender 221837 Resolution 0.075 -> 0.218 (log
  0.627), Swiftness 0 -> 0.24 (log 0.366). Willbender PvE rows unchanged:
  `pve.json` has no Guardian records. The player's Willbender traits
  (decoded with `build_template::decode`): Justice is Blind, Inspiring
  Virtue, Virtue of Resolution, Inspired Virtue, Permeating Wrath,
  Righteous Instincts, Lethal Tempo, Restorative Virtues, Tyrant's
  Momentum, Righteous Sprint; none grants Quickness or Fury by its API
  facts, so the Willbender's self Quickness 0.38 / Fury 0.34 on the golem
  come from the skill side (under investigation). `corpus_matching`: the
  frozen Revenant WvW Buffer case re-frozen to hardstuck "Zerg Boon DPS"
  with the measured reason (boon axis 0 -> 0.171, alignment -0.012 ->
  +0.088 once its trait boon records fire; "Zerg Support" measures 0 on
  every axis except sustain before and after). Workspace tests 2191
  passed; `calibrate` gate counts identical; `effect_coverage` identical.

- 2026-09-23, data + engine increment: Willbender self Quickness. Records
  (PvE, WvW and PvP files; neither wiki page splits these numbers by mode,
  and the player's own logs show the same 6.55 s self window after every
  clean "Feel My Wrath!" cast in the golem log and in 23 WvW casts across 11
  WvW logs, `GW2_Build_Optimizer-work/willbender-2026-09-23/`):
  `skill:29965:0` "Feel My Wrath!" `OnSkillUse` self Quickness 3 s (wiki: the
  quickness you grant yourself is doubled; the API fact's 3 s is the ally
  grant); `sigil:24561:0` Superior Sigil of Rage `OnCrit`, ICD 20 s, self
  Quickness 3 s, gate `SelfBoonAbsent { boon: "Quickness" }` (wiki: will not
  trigger if you already have quickness). Format: new gate
  `SelfBoonAbsent { boon }`, validated like `SelfBoon`
  (`optimizer-data-schemas.md` Sprint 4 additions). Engine (flow sim):
  duration-stacking boons (`boons.json` `stacking_mode: Duration`) add their
  time to the running instance up to `max_duration` instead of running beside
  it (`SimState::add_buff`); a skill's own unscoped `OnSkillUse` record fires
  on that skill (`ProcTrigger::OwnCast`); the bar's skill records and the
  socketed sigils' records load with the traits'
  (`equipped_skill_and_sigil_records`, seats from `sigil_seats`, the logic
  `active_normalized_effects` already used); a sigil proc is live only while
  its set is held (the set held before a form while in one); `SelfBoon` and
  `SelfBoonAbsent` gates play; `OnCrit` with an ICD of 5 s or more plays as
  a hit proc that gathers each strike's crit chance as probability mass and
  fires at one whole proc (`ProcTrigger::Crit`), shorter ICDs abstain as
  "on-crit under a 5 s cooldown". WvW timeline: evaluates the new gate.
  Measured, Willbender golem 204811 (log / before / after): Quickness 0.376 /
  0.100 / 0.350, `burst_overlap_share` 0.308 / 0.021 / 0.247, `dps_engaged`
  11343 / 8741 / 9840, `burst_peak_5s` 16769 / 16647 / 21348 (now 27% over:
  the sim's best 5 s sits inside a Quickness window more often than the
  log's). WvW Willbender 221837: Quickness 0.210 / 0.100 / 0.300, overlap
  0.796 / 0.045 / 0.230, `dps_engaged` 4150 / 3727 / 3801. Reaper golem
  204313: `dps_engaged` 5572 -> 5668, Quickness 0.515 -> 0.577 (log 0.830),
  Fury 0.663 -> 0.725 (log 0.993). Side effect: WvW Reaper rows that already
  over-supplied Quickness go further (Lucian Lord 0.785 -> 0.997, logs
  0.2-0.6; gap E22). `effect_coverage`: sigils 7 -> 8 executable, elite
  skills 0 -> 1, coverage unchanged. `calibrate` PvE identical; WvW identical
  except ProtectedExecution, which flickers 55/56 between runs of one binary
  (pre-existing run-to-run nondeterminism, three runs of the after binary:
  55, 56, 56). `corpus_matching`: Necromancer WvW Disabler flips hardstuck
  "Zerg Support" -> "Zerg Power DPS" (power-reaper-2 control axis 0.320 ->
  0.363, alignment 0.333 -> 0.363 over Zerg Support's unchanged 0.348;
  measured with duration stacking switched off: 0.320 again, so the cause is
  more Quickness from stacked durations). Re-frozen with that reason;
  duration stacking stays (the game rule).

Engine gaps the review measured (counted Executable, never run). The next
engine increment (single-writer) closes these before more professions are
authored, or the executable column overstates:

| Gap | Shape | Where |
|---|---|---|
| E2 | `Conditional` loads only strike / crit records with a `prerequisite`; gated stat records (SelfBoon, InCombat, HealthThreshold gates with no prerequisite; FlatStat / IncomingStrikeMultiplier on attunement) never run | `wvw_timeline.rs` record loading (~1271-1310, 1383) |
| E3 | `OnHealthThreshold` loads only `StrikeDamagePct` | ~1314 |
| E4 | `Passive` records carrying `gates` or `scale` are skipped | ~1204 |
| E5 | timed `ConditionDamagePct` procs have no branch (Twice as Vicious 2127:1, 2356:1) | `trigger_procs` ~3872/3935 |
| E6 | `MechanicUnlock` has no consumer (33 records, 19 sources) | none yet |
| E7 | `FlatStat` / `StatConversion` records name no attribute, so no engine can execute them | format |
| E8 | `is_attuned` counts a Weaver's off-hand for core traits | `attunement.rs:102` |
| E9 | `AppliesBoon` with a non-boon `status_kind` (auras, Elemental Empowerment) fires as an inert marker scaled by boon duration | timeline |
| E10 | fact parser sums 3- and 4-way API splits and takes PvE values in WvW (Electric Discharge +100 crit damage, Pure of Sight ~+42 %, Laser's Edge ~+45 %, Vow of the Untamed ~+51 %) | `combat.rs` `absorb_pair` |
| E11 | CLOSED 2026-09-23: auto-attack chains: the simulator casts only step 1 of every chain and drops the follow-ups (`context.rs` skips `prev_chain` skills; nothing in `simulator.rs` reads `next_chain`); Life Slash/Life Reap and every weapon's chain 2/3 never happen | `rotation/builder.rs`, `rotation/simulator.rs` |
| E12 | PvE record file has no shroud-trigger records, so Reaper Fury/Quickness inside shroud never fire in PvE (WvW records exist) | `data/normalized_effects/*/pve.json` |
| E13 | forms not covered by the form mechanism: Photon Forge, kits, Tempest overloads, Lich Form and other elite transforms; the WvW timeline does not enter Celestial Avatar | `engine::form_for_build` |
| E14 | astral force from healing is not credited (the flow dummy takes no damage), Eclipse/Grace of the Land/pet boon sources absent | flow sim |
| E15 | CLOSED 2026-09-23: triggered trait records (`OnSkillUse`, `OnConditionApplied`) ride on `FormSpec`, so a build with no form (Scourge, core) fires none of them in the flow sim | `engine::form_for_build`, `simulator.rs fire_triggered` |
| E16 | CLOSED 2026-09-23: flow sim ignores timed damage modifiers (Dread +20%, Soul Barbs +10%, in-shroud crit damage) and condition-applying procs | flow sim |
| E17 | HALF CLOSED 2026-09-23 (Fear of Death done, Dhuumfire open): WvW record file: Fear of Death lacks the wiki 5s recharge; Dhuumfire uses the Scourge values (1s burning, 5s recharge) for every Necromancer because the format has no elite-spec gate | `wvw.json`, format |
| E18 | weapon choice: the flow sim spends casts on greatsword autos where the log's Reaper stays in shroud or uses the other set; the scheduler's weapon/set preference is not measured against logs | `rotation/simulator.rs` scheduler |
| E19 | CLOSED 2026-09-23: consumables: the item download filters out type Consumable, so stated food/utility (Superior Sharpening Stone, +100 Power/+70 Ferocity food) cannot resolve and abstain; the stat sheet already has a food/utility path (`consumables.rs`) | `gw2api download.rs` filter, `consumables.rs` |
| E20 | optimizer food/utility selection ignores `StatConversion` consumables; conversions are applied after trait conversions rather than before | `consumables.rs`, `search_v2.rs` |
| E21 | WvW timeline runs boons side by side (`apply_buff` pushes a parallel `TimedBuff`), so duration-stacking records add nothing there (the "Feel My Wrath!" self record, any overlapping Quickness/Fury grant) | `wvw_timeline.rs apply_buff` |
| E22 | WvW Reaper rows over-produce Quickness once it stacks in duration: Lucian Lord 0.785 -> 0.997 vs logs 0.2-0.6; some Necromancer WvW Quickness record fires too often, unidentified | `wvw.json` Necromancer records, flow sim |

Format gaps the builders named (each is a coverage block today): on-weapon-swap,
on-struck, on-ally-healed, on-kill, on-combo / on-aura, first-strike-after-
combat-entry, endurance-regen category, barrier category, recharge-reduction
category, distance scale with a floor, "either boon" gates, per-virtue / single-
skill scope, pet-side stats and boon targets, heat / astral force / unleashed /
Photon Forge / Radiant Forge / Celestial Avatar state, distinct-condition-count
scale, "others only" healing.

Necromancer traits still on the gap line by name: Soul Comprehension (no
carapace resource), Unholy Sanctuary, Soul Eater, Relentless Pursuit, Vital
Persistence, Gluttony, Soul Battery, Sinister Shroud, Shroud Knight (percent
heals, damage-scaled heals, incoming duration, life-force scaling,
recharge), Spiteful Spirit and Weakening Shroud (trait skills cast on entry),
the Dhuumfire Scourge/Harbinger variants (elite-spec gate), and the
Harbinger shroud traits (not in scope).

## Gate 2: the state machine runs on data

| Number | Now | Target |
|---|---|---|
| Profession resource rules expressed as data records (initial, cap, regen, gain, cost unit, spend rule, swap/upkeep) | Warrior, Revenant, Thief, Mesmer, Necro, Bladesworn in code branches | all 9 professions + elite variants in `data/resources.json`; `wvw_resource_rules` reads it, no `match profession` |
| WvW builds where ResourceLegality abstains | 79 / 140 | 0 |
| Weapon-swap, attunement, kit, legend, shroud transitions modelled as timed state | partial | each with a wiki-cited fixture |

Acceptance: `resource_model_complete` true for every published build;
calibrate prints no abstentions; fixtures pin the wiki numbers.

## Gate 3: fidelity against the corpus

| Number | Now | Target |
|---|---|---|
| Published WvW builds refused by blocking gates | 25 / 140 | <= 10, every one classified (c) correct refusal with the note |
| Published PvP builds refused | 17 / 120 | <= 8, classified |
| Published PvE builds refused | 14 / 458 | <= 8, classified |
| Validator rejects that are ours | 0 | 0 (guarded) |
| Unplatable rows that are ours | 0 | 0 (guarded) |

Acceptance: `EXPECTED_REFUSALS` / `EXPECTED_UNPLATABLE` budgets ratchet down
to these numbers; each remaining row carries its cause.

### Gate 3b: fidelity against combat logs (added 2026-09-23)

Gate pass rates say whether the referee agrees with the corpus; they do not say
whether the simulator's numbers are the game's. The ground truth for that is
arcdps logs parsed by Elite Insights: per-skill casts and hits, boon uptimes,
cleanses, damage taken, downs, per player, per fight. `fidelity/` reads such a
log, rebuilds each squad player's kit (spec, weapons, bar and opener from the
log; traits from a chat code when one is supplied, else from the nearest
validator-clean published build, every field with its provenance), runs it
through the addon's own path (plate, validate, scenario, referee, flow sim) and
prints per-observable errors and bands per (profession, mode, observable).
Instrument: `cargo run -p gw2-optimizer --example log_compare -- <log|dir>`;
suite: `crates/optimizer/tests/fidelity_logs.rs`; fixtures: three trimmed
logs under `tests/fixtures/ei_logs/` (one Snow Crows golem log with its chat
code, two WvW party logs).

The three committed fixtures were re-downloaded from dps.report (the
permalink id is the full file stem) and re-trimmed with `log_compare --trim`,
and now carry `damage1S`, `conditionDamage1S`, `buffUptimes[].states` and
`dpsAll` actor damage/condi-damage splits (sizes 66 -> 71 KB, 160 -> 182 KB,
173 -> 198 KB); the golem test pins `burst_overlap_share` 1.0,
`condition_share` 0.0029 and `condition_ramp_s` 11 as facts of the file.

A log carries no traits, gear or chat code (verified against the EI model and
the EVTC format), so WvW players without a supplied code are compared on
corpus traits and the band partly measures distance from meta. Chat codes per
log go in `codes.json`.

Baseline (run 3 after review fixes, 11 of 11 squad players compared, bands keyed by elite spec; `EXPECTED_FIDELITY` ships
empty and is seeded from the next run after the items below):

| Finding | Number | Cause | Next |
|---|---|---|---|
| Golem Power Reaper DPS | ours 12.5k vs log 42.5k; shroud skills 0 for us, 15 of 20 log skills present | two causes multiplied: the golem grants all boons and 25 might while the sim is self-only, and the flow simulation never enters shroud (`rotation/simulator.rs` only swaps sets 1 and 2; shroud entry exists only in the WvW timeline; Lich Form likewise) | simulator: shroud entry in the flow sim (single-writer engine increment); benchmark boon assumptions into the flow sim |
| WvW `dps_engaged` | one fight overshoots +67 % to +396 %, the other undershoots 27-75 % | not one cause: the simulator hits a target 100 % of 60 s (logs engage 27-46 % of active time, weapon skills connect 66-88 %), roaming neighbours chosen for zerg players, no miss/evade model, one simulator outlier (Spinal Shivers at 34 % of a Reaper's damage) | consume the fight profile (`fight_profile::extract`: availability, hit rate per class, incoming DPS, CC/min) as scenario data; supply chat codes for WvW fixtures |
| PvE boon uptimes | scored against the player's self-generated share (EI `generated`): Quickness self 0.48 vs ours 0.10, Might self 9.9 vs ours 5.8 stacks; the golem's external boons (total 1.0 / 25) are printed beside, not scored | the simulator is self-only and its flow run never enters shroud | feed the benchmark's boon assumptions into the flow sim; re-measure after shroud |
| `skill_share` TVD | 0.58-0.96 across WvW bands | the two items above plus name joins | re-measure after both |
| `condi_fraction` | Engineer .005, Necromancer PvE .003, Mesmer .02 median | duration-free, the honest signal today | seed the ratchet on this and on uptimes once PvE boons are handled |

Player's own logs (2026-09-23 evening, gear and traits from the account
cache, no external boons on the golem; converted locally, not committed):

| Character | Log DPS | Ours | condi share log / ours | skill_share TVD | Self Fury log / ours |
|---|---|---|---|---|---|
| Druid (condition), golem stationary | 7325 | 1656 (dps_engaged error -0.774) | 0.85 / 0.74 | 0.205 | 0.68 / 0.00 |
| Druid, golem moving | 5511 | 1651 | 0.88 / 0.74 | 0.20 | 0.46 / 0.00 |
| Reaper (power, Demolisher), golem | 11707 | 4814 (dps_engaged error -0.589) | 0.005 / 0.000 | 0.478 | 0.99 / 0.38 |
| Willbender (power, corpus build), golem | 11343 | 7156 | 0.09 / 0.04 | 0.41 | 0.34 / 0.33 |
| Druid, six WvW solo fights | 500-2766 | 1479 | median error 0.07 | 0.15-0.24 | 0.52-0.85 / 0.00 |

After forms (measured on the main tree, forms and the 1.14.41
Account-provenance comparator together): `skill_share` TVD moved to 0.478 for
Reaper (was 0.647) and 0.205 for Druid (was 0.716). The Druid gain comes from
two changes landing together: Account gear provenance (the character's real
Ritualist/Apothecary gear from the addon cache instead of a corpus neighbour)
plus Celestial Avatar in the flow sim. Boons did not move, because the PvE
record file has no shroud-trigger records yet (gap E12).

Reads: every shroud and Celestial Avatar skill is 0 on our side (forms never
entered in the flow sim); self-generated boons from pets and forms are absent;
the moving golem lost 1.8k DPS with hit rate and availability both still 1.00,
so cast density is a third fight-shape term next to hit rate and availability.

New WvW squad logs (Hammerhand The Bold, Willbender, kit from the corpus
neighbour Celestial Willbender roaming, because the character has no chat
code and is not in the addon cache):

| Log | Squad | Duration | Availability | CC/min | dps_engaged log/sim (error) | burst_peak_5s log/sim | burst_overlap_share log/sim | Fury self/total | Quickness self/total |
|---|---|---|---|---|---|---|---|---|---|
| 20260923-221546 | 6 | 77s | 0.33 | 1.3 | 4149 / 1417 (-0.658) | 10053 / 2309 | 0.667 / 0.171 | 0.28 / 0.57 | 0.25 / - |
| 20260923-221837 | 13 | 156s | 0.27 | 0.6 | 4150 / 1242 (-0.701) | 8699 / 1986 | 0.796 / 0.162 | 0.24 / 0.81 | 0.21 / 0.35 |

In the 13-player fight, melee power players undershoot in the sim (Willbender,
Reapers) while Dragonhunter (+2.1) and Berserker (+1.6) overshoot, because
every simulated hit lands on a target present 27% of the time; group boons
(total vs self uptime) are a scenario input the simulator does not take.

Stated gear (2026-09-23): with the player's own stated kit replacing the
corpus guess (Berserker), the golem Willbender's `dps_engaged` error moved
from -0.27 to -0.31, because the stated kit (Dragon's/Marauder) is not the
same as the corpus guess's higher-power set; the WvW logs' `dps_engaged`
improved 2416 -> 3364 and the 8699 log's `burst_peak_5s` reading moved
3861 -> 5462. What is left after real traits and real gear is a pure engine
gap: Quickness upkeep and trigger records for a build without a form (E15),
and consumables (E19).

Fight profiles the logs yielded (WvW party, 77-84 s): target availability
0.27 / 0.46; incoming 1.8-1.9k DPS; CC received 1.9 / 5.2 per minute; strips
received 11 per minute; downs 0.9-1.0 per player-minute. These are the numbers
the scripted `WvwProfile::for_scenario` pressure cycle and the every-hit-lands
assumption are to be replaced with.

### Yardsticks (player doctrine, 2026-09-23)

Power builds have no sustained damage; damage comes in bursts where Quickness,
Fury, 100% crit and the form/burst skills overlap for about five seconds.
Average DPS and average uptime are secondary. Headline observables to add to
`log_compare`: peak N-second damage and the share of damage dealt while
Quickness and Fury are both up (the EI JSON carries per-second damage and
per-second buff states).

Condition damage is the only sustained damage. Condition builds are judged on
sustained pressure: condition DPS over the fight, ramp time, stack upkeep. The
Druid golem log (92% condition) is the reference case.

WvW: no enemy stands still, none lets themselves be disabled, all try to
disable you. The longer the synergy chain a rotation strings together the
better, but only if it survives disruption (non-damaging conditions, CC,
strips). Stability, evades, blocks prolong the window; cleanses restore it.
The fight profile (availability 0.27-0.46, 2-5 CC/min, 1.8-1.9k incoming DPS)
is the disruption budget the simulator must spend against the burst windows.

Playstyle picks the window: surprise builds are judged on the opening burst
before the opponent reacts; duelists/brawlers/trolls on the prolonged fight
under the full disruption budget. Never rank one on the other's yardstick;
directional intent chooses the shape.

The player runs matched food and utility on every build, so consumables are
build slots for simulation and recommendation, not extras.

Planned order: burst observable + Necromancer PvE shroud-trigger records
(E12), condition-pressure observable, disruption scenarios from the fight
profile, then E11 auto chains.

The first two yardsticks are now measured: `burst_peak_5s`,
`burst_peak_10s` and `burst_overlap_share` for the power/burst yardstick,
`condition_share` and `condition_ramp_s` for the sustained-condition yardstick
(see the 2026-09-23 "Fidelity: burst and condition observables" increment).
The WvW disruption-budget and playstyle-window yardsticks remain unmeasured.

## Gate 4: intent and matching

| Number | Now | Target |
|---|---|---|
| Role x scale combinations with their own profile | WvW Damage at roam maps to zerg DPS | every chip x scale names a profile whose focus/avoid fits (add `WvW_Roam_DPS`, review Revenant support sitting at the floor) |
| Selectable combinations returning a correct card or a recorded none | 369 walked | 369, with the six named cases frozen |
| Label/family concepts in the matching path | 0 | 0 (guarded by grep test) |

## Gate 5: beat meta

For every (profession, mode, role, scale) that has at least one published
reference: run the optimizer offline (meta-seeded), evaluate the result and
the best reference under the same intent, and assert
`ours.intent_alignment >= best_reference.intent_alignment` and
`ours.is_viable`.

| Number | Now | Target |
|---|---|---|
| Combinations where ours >= best reference | 1 measured (Warrior WvW havoc support) | 100% of covered combinations |

Acceptance: `crates/optimizer/tests/beat_meta.rs` (db-backed, ignored in
plain CI, run in the release checklist) prints the table; a combination that
loses fails with both kits and both axes.

## Gate 6: the theorist protocol

- Choya's advisor runs hypothesis -> simulate -> keep: it proposes synergy
  chains from the records (trait -> trigger -> effect -> window), asks the
  oracle (`simulate_rotation`, `score_build`), keeps the best, and returns a
  recipe: the chain, the rotation it is built around, and why each kit piece
  is there.
- The served build's explanation is that recipe, not prose about stats.

Acceptance: a fixture request yields an explanation naming at least one
trait -> proc -> effect chain that the simulator confirms fired.

## Gate 7: the player sees the model

- Minor traits rendered per specialization (always-on row) in the build view
  and lock panel, with their facts.
- Every gate outcome visible: pass, fail with note, skipped with mechanic.
- The recipe rendered with the build.

## Working rules for this sprint

- One increment at a time, each ending with: corpus suite green, calibrate
  numbers recorded in this file, DLL deployed, in-game check, release.
- Builders per profession may run in parallel only on disjoint data files;
  the timeline and engine are single-writer per increment.
- No new subsystem (profile stores, classifiers, caches) unless a gate
  number cannot be met without it.
