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
| Minor traits with a record | 168 / 243 | 243 / 243 |
| Minor-trait records that are flat "Passive" but the wiki describes a trigger, interval, combat state or weapon condition | 145 unclassified | 0 (each is either genuinely passive, with the wiki line cited, or a real trigger record) |
| Major traits with a record | measure first | 729 / 729 |
| Rune, sigil, relic effects modelled as records (not text parse) | 2 / 3 / 1 records | every tier bonus and proc that the wiki lists |
| Records lacking duration, ICD or stacking where the wiki gives one | measure | 0 |

Acceptance: `calibrate_viability` per mode plus a new `effect_coverage`
example that prints the table above; the corpus suite gains a test that the
counts do not regress.

Order: Elementalist, Engineer, Guardian, Ranger (the four that abstain), then
Warrior, Mesmer, Revenant, Thief; Necromancer is the reference (27/27).

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
