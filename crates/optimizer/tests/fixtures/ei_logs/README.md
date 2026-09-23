# Elite Insights log fixtures

Trimmed Elite Insights (EI) JSON logs for the fidelity comparator
(`crates/optimizer/src/fidelity`). Downloaded 2026-09-23 with
`https://dps.report/getJson?permalink=<permalink>`.

| File | Source | Content |
|---|---|---|
| `1f33-20260720-163045_golem.json` | https://dps.report/1f33-20260720-163045_golem | Snow Crows benchmark, Power Reaper Greatsword/Spear (https://snowcrows.com/builds/raids/necromancer/power-reaper-greatsword-spear). Standard Kitty Golem, 1 player, EI 3.26.0.0. |
| `lRBj-20260604-210631_wvw.json` | https://dps.report/lRBj-20260604-210631_wvw | Detailed WvW, Eternal Battlegrounds, EI 3.30.0.0. Squad group 2, 5 players. |
| `aBtd-20260604-211449_wvw.json` | https://dps.report/aBtd-20260604-211449_wvw | Detailed WvW, Eternal Battlegrounds, EI 3.30.0.0. Squad group 1, 5 players. |
| `codes.json` | Snow Crows build page above | Build chat code per log file and character name. |

The two WvW logs come from the SOCK guild comp doc:
https://github.com/theextendedname/SOCK_BUILDS/blob/60d87f68789eb7108f990796be24a8d9f7441183/Archive/SOCK-Bulds_5-6-2026/testing/Wildfire%20Comp.txt

## Trim rule

1. Drop players with `notInSquad` or `isFake`.
2. WvW only: keep the squad group with the most players (ties go to the lowest
   group number), at most 10 players. Every group in both logs has 5 players,
   so each fixture is one party, not the full squad (20 and 30 players).
3. Record the untrimmed squad size as `trimmedSquadSize` (not an EI key), so
   the fixture keeps the scale it was played at: 1, 20 and 30.
4. Keep only the wearer's own entry in each boon's `generated` and
   `generatedPresence` source maps, so no other player's name is kept.
5. Parse with `fidelity::ei_log::parse` and write `serde_json::to_string` of the
   result. This drops every field the model does not read, including
   `targets` and account names. Each file is a fixed point of parse then
   serialize, and the unit tests check it.

Reproduce with
`cargo run -p gw2-optimizer --example log_compare -- <download.json> --trim <fixture.json>`.
