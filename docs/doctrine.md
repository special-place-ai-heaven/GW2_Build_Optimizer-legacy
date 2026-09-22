# Doctrine

Rules every agent and every session follows in this repository. A change that
contradicts a rule here is wrong until the rule is changed here first.

## The goal

For any profession, mode, role and scale, the optimizer produces a build that
scores at or above the best published meta build under that intent, in our
simulator, and the simulator's scores are validated against those same
published builds. Beat meta, measured.

## The division of labour

- The **state machine** (rotation simulator, WvW timeline, referee) is the
  oracle. It executes data records and reports what happened: uptimes,
  overlaps, damage, healing, gates. It knows nothing about intent.
- The **search** proposes kits and asks the oracle. It is seeded from published
  meta builds so proposals start from proven synergy structures.
- The **LLM** is the theorist: it reads the same records a human theory-crafter
  reads, proposes synergy chains toward the player's intent, has the oracle
  confirm or refute each, keeps what wins, and writes the recipe (which trait
  feeds which proc, why this rune, what the rotation is built around).
- The **published corpus** is ground truth: calibration, seeds and regression
  guard.

## Rules

1. **Measure, never label.** A build's role is what its measured six-axis
   output says under the player's intent. Site headings, role words and job
   families are tie-breaks at most, never a gate, never a fallback.
2. **Intent is a direction.** Every objective profile declares, per scale,
   which axes it focuses and which it avoids. Importance vectors alone cannot
   say "not power". `scoring::intent_alignment` is the one metric; one floor.
3. **Scale sets self-reliance.** Solo depends on no one (sustain, cleanse,
   stunbreak, stability are focus and hard gates). Havoc is moderate. Zerg is
   carried by overlap and focuses group contribution.
4. **Damage has opposite sub-archetypes.** Glass cannon avoids sustain;
   brawler lives on it; troll avoids damage. The chips map to these directions.
5. **Rules live in data, not in code branches.** When a profession or mechanic
   is wrong, the fix is a record (trigger, duration, cooldown, resource rule),
   not a `match profession`. If the record format cannot express it, extend
   the format once, for everyone.
6. **Abstain rather than pass.** An unmodelled mechanic yields a skipped gate
   that names the mechanic. No silent passes, no invented facts.
7. **Every rule is a test.** A decision that is not data or a test is a wish.
   The corpus suite (`crates/optimizer/tests/corpus_matching.rs`) budgets
   refusals and unplatable rows per profession with a cause; budgets ratchet.
8. **Example = addon = test by construction.** Offline instruments call the
   same library entry points the addon calls (`ScenarioSpec::for_request`,
   `picks::rank`). A hand-built scenario in an example is a bug.
9. **A meta build is the most successful combination of synergies.** Judge it
   by simulating the whole kit; use aligned kits as beam seeds.
10. **Shared tree hygiene.** Builders never run `git checkout/restore/stash/
    reset` on any path. Probes live in the scratchpad. The Chief stages
    verified files early.

## Measured state (2026-09-22, release 1.14.39)

| Measure | Value |
|---|---|
| Minor traits with a trigger record | 168 / 243 (145 of them flat "Passive") |
| Published builds our validator rejects | WvW 5/145, PvE 16/474, PvP 1/121 |
| Published builds passing all blocking gates | WvW 115/140, PvE 444/458, PvP 103/120 |
| Resource gate abstaining (unmodelled) in WvW | 79 of 140 builds (Ele, Engi, Guardian, Ranger) |
| Unplatable references | 22 / 740 (site data) |
| Warrior WvW Havoc Support: best reference vs ours | 0.201 vs 0.295 alignment |
