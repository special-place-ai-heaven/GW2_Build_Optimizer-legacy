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
| Rune tier lines / sigils / relics executable | 2 of 642 lines / 7 of 81 / 4 of 128 | all |
| Profession-mechanic skills / elite skills executable | 1 of 367 / 0 of 133 | all with a proc or effect |
| Coverage placeholders still typed `Passive` (schema artifact) | 0 (582 migrated to NotApplicable, 2026-09-22) | 0, guarded by validation rule 15 |
| Minor-trait records that are flat "Passive" but the wiki describes a trigger, interval, combat state or weapon condition | 0 for Ele/Engi/Guardian/Ranger (every remaining coverage block names the trigger or field the format lacks); other four professions not yet read | 0 (each is either genuinely passive, with the wiki line cited, or a real trigger record) |
| Rune, sigil, relic effects modelled as records (not text parse) | 2 / 3 / 1 records | every tier bonus and proc that the wiki lists |
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

Format gaps the builders named (each is a coverage block today): on-weapon-swap,
on-struck, on-ally-healed, on-kill, on-combo / on-aura, first-strike-after-
combat-entry, endurance-regen category, barrier category, recharge-reduction
category, distance scale with a floor, "either boon" gates, per-virtue / single-
skill scope, pet-side stats and boon targets, heat / astral force / unleashed /
Photon Forge / Radiant Forge / Celestial Avatar state, distinct-condition-count
scale, "others only" healing.

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

Fight profiles the logs yielded (WvW party, 77-84 s): target availability
0.27 / 0.46; incoming 1.8-1.9k DPS; CC received 1.9 / 5.2 per minute; strips
received 11 per minute; downs 0.9-1.0 per player-minute. These are the numbers
the scripted `WvwProfile::for_scenario` pressure cycle and the every-hit-lands
assumption are to be replaced with.

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
