# Optimizer Data Schemas

Status: Draft for adoption
Owner: Project maintainers
Audience: Human maintainers, AI coding agents, reviewers
Last updated: 2026-08-23

This document defines the machine-readable contracts that implement `docs/optimizer-source-of-truth.md`.

It is intentionally practical. The goal is to let a coding agent create files, validators, loaders, tests, and migration code without inventing structure.

## Relationship To Source Of Truth

`docs/optimizer-source-of-truth.md` defines:

- what the optimizer must model
- what is factual vs heuristic
- which invariants are non-negotiable

This document defines:

- file layout
- field names
- types
- required validation rules
- serialization examples

If these two documents ever disagree:

1. update this schema document
2. update code
3. update tests

Do not let the code become the de facto schema.

## Design Goals

The data model must support:

1. patch-versioned balance data
2. mode-specific overrides
3. profession-specific constants
4. effect normalization
5. explicit heuristics
6. forward migration when large patches land

The data model must not:

1. flatten PvE, PvP, and WvW into one coefficient table
2. store heuristics next to factual constants without labeling them
3. overwrite old patch data in place
4. assume one static balance snapshot is enough

## Recommended Directory Layout

```text
data/
  manifests/
    2026-01-13.json
    2026-07-15.json
  profession_profiles.json
  slot_budgets/
    level80_ascended.json
    level80_exotic.json
  formulas/
    universal.json
    conditions.json
    boons.json
  balance_overrides/
    2026-01-13/
      pve.json
      pvp.json
      wvw.json
    2026-07-15/
      pve.json
      pvp.json
      wvw.json
  patch_ledgers/
    2026-01-13.yaml
    2026-07-15.yaml
  normalized_effects/
    2026-01-13/
      pve.json
      pvp.json
      wvw.json
  rotation_profiles/
    pve.json
    pvp.json
    wvw.json
  objective_profiles/
    pve.json
    pvp.json
    wvw.json
```

## Global Conventions

### 1. Patch IDs

Patch IDs use ISO date format:

```text
YYYY-MM-DD
```

Example:

```text
2026-01-13
```

### 2. Game mode enum

Allowed values:

```json
["PvE", "PvP", "WvW"]
```

### 3. Evidence level enum

Allowed values:

```json
["Factual", "Derived", "Heuristic", "Unknown"]
```

Meaning:

- `Factual`: from API, wiki formula, or patch note
- `Derived`: deterministic output from factual inputs
- `Heuristic`: approved estimate
- `Unknown`: explicitly unresolved after a patch

### 4. Numeric conventions

Required:

1. percent-like values are stored as decimal ratios unless explicitly named `_pct_points`
2. raw display percentages are never stored as ambiguous integers
3. formulas must declare units

Examples:

- `0.25` for `+25% crit chance`
- `0.33` for `33% mitigation`
- `10.0` only for literal stat values, damage coefficients, or display values with clear field names

### 5. Null and unknown rules

Use:

- `null` for structurally optional values
- `Unknown` evidence state for known-but-unresolved numbers

Do not:

- silently default unresolved values to `0`
- silently reuse a previous patch value without marking inheritance explicitly

## Schema 1: Patch Manifest

Path:

```text
data/manifests/<patch_id>.json
```

Purpose:

- define the patch snapshot
- declare inheritance chain
- declare source links

Schema:

```json
{
  "patch_id": "2026-01-13",
  "game_build_id": 175218,
  "release_date": "2026-01-13",
  "inherits_from": "2025-11-25",
  "sources": [
    {
      "kind": "wiki_patch_notes",
      "url": "https://wiki.guildwars2.com/wiki/Game_updates/2026-01-13"
    }
  ],
  "supported_modes": ["PvE", "PvP", "WvW"],
  "status": "active",
  "authoring_notes": "Optional free-text notes for manifest authors."
}
```

Fields:

- `game_build_id` (integer, required): The GW2 client build number this manifest was verified against. Used by staleness detection to warn when the live game build diverges from the data snapshot. The value is informational — it does not gate optimizer functionality.
- `authoring_notes` (string, optional): Free-text notes from the manifest author explaining context, limitations, or baseline assumptions.

Validation rules:

1. `patch_id` must equal filename stem
2. `game_build_id` must be > 0
3. `inherits_from` may be null only for the earliest snapshot
4. at least one `source` is required
5. `status` must be one of: `active`, `superseded`, `draft`
6. `supported_modes` must be non-empty
7. Set validation: no duplicate `patch_id`, no circular inheritance, no two `active` manifests in the same lineage

## Schema 2: Profession Profiles

Path:

```text
data/profession_profiles.json
```

Purpose:

- canonical base constants per profession

Schema:

```json
[
  {
    "profession": "Guardian",
    "armor_weight": "Heavy",
    "health_class": "Low",
    "base_health_level_80": 1645,
    "base_defense_level_80": 1271,
    "evidence_level": "Factual",
    "sources": [
      "https://wiki.guildwars2.com/wiki/Armor",
      "https://wiki.guildwars2.com/wiki/Health"
    ]
  }
]
```

Validation rules:

1. exactly one entry per core profession
2. `base_defense_level_80` must match armor class
3. armor class and health class must be modeled independently

## Schema 3: Universal Formulas

Path:

```text
data/formulas/universal.json
```

Purpose:

- store factual universal formulas and numeric constants

Schema:

```json
{
  "level_80": {
    "base_primary_attribute": 1000,
    "vitality_to_health": 10.0,
    "precision_offset": 895.0,
    "precision_per_crit_pct": 21.0,
    "ferocity_per_crit_damage_pct": 15.0,
    "expertise_per_condition_duration_pct": 15.0,
    "concentration_per_boon_duration_pct": 15.0,
    "condition_duration_cap": 1.0,
    "boon_duration_cap": 1.0
  },
  "damage_reference": {
    "tooltip_reference_armor": 2597.0
  },
  "evidence_level": "Factual",
  "sources": [
    "https://wiki.guildwars2.com/wiki/Attribute",
    "https://wiki.guildwars2.com/wiki/Damage"
  ]
}
```

Validation rules:

1. all caps are stored as decimal ratios
2. no mode-specific field is allowed in this file

## Schema 4: Condition Formulas

Path:

```text
data/formulas/conditions.json
```

Purpose:

- store canonical condition formulas by mode

Schema:

```json
{
  "Bleeding": {
    "PvE": {
      "base_per_tick": 22.0,
      "condition_damage_coeff": 0.06,
      "delivery": "tick"
    },
    "PvP": {
      "base_per_tick": 22.0,
      "condition_damage_coeff": 0.06,
      "delivery": "tick"
    },
    "WvW": {
      "base_per_tick": 22.0,
      "condition_damage_coeff": 0.06,
      "delivery": "tick"
    }
  },
  "Torment": {
    "PvE": {
      "stationary": {
        "base_per_tick": 31.8,
        "condition_damage_coeff": 0.09
      },
      "moving": {
        "base_per_tick": 22.0,
        "condition_damage_coeff": 0.06
      }
    },
    "PvP": {
      "stationary": {
        "base_per_tick": 26.0,
        "condition_damage_coeff": 0.07
      },
      "moving": {
        "base_per_tick": 19.8,
        "condition_damage_coeff": 0.054
      }
    },
    "WvW": {
      "stationary": {
        "base_per_tick": 26.0,
        "condition_damage_coeff": 0.07
      },
      "moving": {
        "base_per_tick": 19.8,
        "condition_damage_coeff": 0.054
      }
    }
  }
}
```

Validation rules:

1. every supported condition must declare all supported modes
2. multi-state conditions like `Torment` and `Confusion` must declare their state dimensions explicitly
3. mode-specific splits must not be collapsed

## Schema 5: Boon And Modifier Formulas

Path:

```text
data/formulas/boons.json
```

Purpose:

- store factual boon and mitigation values, including mode splits

Schema:

```json
{
  "Fury": {
    "PvE": {
      "crit_chance_bonus": 0.25
    },
    "PvP": {
      "crit_chance_bonus": 0.20
    },
    "WvW": {
      "crit_chance_bonus": 0.20
    }
  },
  "Protection": {
    "all_modes": {
      "incoming_strike_multiplier": 0.67
    }
  },
  "Resolution": {
    "all_modes": {
      "incoming_condition_multiplier": 0.67
    }
  },
  "Might": {
    "all_modes": {
      "power_per_stack": 30,
      "condition_damage_per_stack": 30
    }
  },
  "Vulnerability": {
    "all_modes": {
      "incoming_damage_pct_per_stack": 0.01,
      "max_stacks": 25
    }
  }
}
```

Validation rules:

1. store mitigation as final multiplier where possible
2. `all_modes` may be used only when the value is truly mode-invariant for the targeted patch

## Schema 6: Slot Budgets

Path:

```text
data/slot_budgets/level80_ascended.json
```

Purpose:

- canonical level-80 slot attribute budgets
- input for prefix search, stat reconstruction, and validation

Schema:

```json
{
  "rarity": "Ascended",
  "level": 80,
  "entries": [
    {
      "slot": "WeaponOneHand",
      "shape": "ThreeStat",
      "major": 125,
      "minor_1": 90,
      "minor_2": 90,
      "evidence_level": "Factual"
    },
    {
      "slot": "WeaponTwoHand",
      "shape": "ThreeStat",
      "major": 251,
      "minor_1": 179,
      "minor_2": 179,
      "evidence_level": "Factual"
    },
    {
      "slot": "Amulet",
      "shape": "ThreeStat",
      "major": 157,
      "minor_1": 108,
      "minor_2": 108,
      "evidence_level": "Factual"
    }
  ],
  "sources": [
    "https://wiki.guildwars2.com/wiki/Attribute_combinations",
    "https://wiki.guildwars2.com/wiki/API:2/itemstats"
  ]
}
```

Validation rules:

1. slot names must be normalized, not UI-local names like `WeaponA1`
2. shape must be explicit:
   - `ThreeStat`
   - `FourStat`
   - `CelestialLike`
   - `PvPAmulet`
3. armor weight must not change attribute budgets for the same slot and rarity

## Schema 7: Patch Ledger

Path:

```text
data/patch_ledgers/<patch_id>.yaml
```

Purpose:

- machine-readable patch diff
- bridge between patch notes and normalized data

Schema:

```yaml
patch_id: 2026-01-13
inherits_from: 2025-11-25
changes:
  - source_type: skill
    source_id: 12345
    source_name: Example Skill
    mode: WvW
    field: power_coefficient
    old_value: 1.2
    new_value: 0.9
    evidence_level: Factual
    source: https://wiki.guildwars2.com/wiki/Game_updates/2026-01-13
  - source_type: trait
    source_id: 67890
    source_name: Example Trait
    mode: PvP
    field: outgoing_healing_pct
    old_value: 0.15
    new_value: 0.10
    evidence_level: Factual
    source: https://wiki.guildwars2.com/wiki/Game_updates/2026-01-13
```

Validation rules:

1. every override must be traceable to a source URL
2. every changed entity must appear in the relevant mode override file or be marked unresolved

## Schema 8: Balance Overrides

Path:

```text
data/balance_overrides/<patch_id>/<mode>.json
```

Purpose:

- mode-specific and patch-specific trait/skill/upgrade overrides

Schema:

```json
{
  "patch_id": "2026-01-13",
  "mode": "WvW",
  "entities": [
    {
      "source_type": "skill",
      "source_id": 12345,
      "name": "Example Skill",
      "overrides": {
        "power_coefficient": {
          "value": 0.9,
          "evidence_level": "Factual",
          "source": "https://wiki.guildwars2.com/wiki/Game_updates/2026-01-13"
        }
      }
    }
  ]
}
```

Validation rules:

1. `mode` must match the filename path
2. any override field must be defined by the normalized effect schema
3. unresolved fields must use:

```json
{
  "value": null,
  "evidence_level": "Unknown"
}
```

## Schema 9: Normalized Effects

Path:

```text
data/normalized_effects/<patch_id>/<mode>.json
```

Purpose:

- canonical normalized effect representation for skills, traits, runes, sigils, relics

This file should be generated where practical, not hand-authored for everything.

Schema:

```json
{
  "patch_id": "2026-01-13",
  "mode": "PvE",
  "effects": [
    {
      "effect_id": "trait:12345:0",
      "source_type": "trait",
      "source_id": 12345,
      "source_name": "Example Trait",
      "category": "StrikeDamagePct",
      "value": 0.10,
      "stacking_rule": "Multiplicative",
      "trigger_rule": "Passive",
      "uptime_model": {
        "kind": "AlwaysOn"
      },
      "evidence_level": "Factual",
      "source": "https://wiki.guildwars2.com/wiki/Game_updates/2026-01-13"
    }
  ]
}
```

Numeric fields use FactualValue semantics: a JSON value means Resolved, `null` means Unknown. For `Option<FactualValue<T>>` fields (e.g., `effect_duration`, `internal_cooldown`, `max_stacks`, `uptime`), three states are distinguished: field absent = not applicable (`None`), field = `null` = applicable but unsourced (`Some(Unknown)`), field = value = factually known (`Some(Resolved(v))`).

Required enums:

- `category`
- `stacking_rule`
- `trigger_rule`
- `uptime_model.kind`

Canonical categories (23):

Modifier categories (1-12):
- `FlatStat`
- `StatConversion`
- `StrikeDamagePct`
- `ConditionDamagePct`
- `SpecificConditionDamagePct`
- `CritDamagePct`
- `BoonDurationPct`
- `ConditionDurationPct`
- `SpecificConditionDurationPct`
- `OutgoingHealingPct`
- `IncomingStrikeMultiplier`
- `IncomingConditionMultiplier`

Application categories (13-14):
- `AppliesBoon` (carries StatusOperation payload)
- `AppliesCondition` (carries StatusOperation payload)

Interaction categories (15-20):
- `RemovesBoon` (carries StatusOperation payload)
- `StealsBoon` (carries StatusOperation payload)
- `CorruptsBoon` (carries StatusOperation payload)
- `RemovesCondition` (carries StatusOperation payload)
- `ConvertsConditionToBoon` (carries StatusOperation payload)
- `TransfersCondition` (carries StatusOperation payload)

Control/proc/meta categories (21-23):
- `DefianceDamage`
- `ProcEffect` (proc IS the final output)
- `TriggeredEffect` (trigger GATES another effect; carries `inner_category`)

Validation rules:

1. every effect must carry evidence level
2. stacking rules must be explicit
3. trigger rules must be explicit
4. if `uptime_model.kind == "Estimated"`, evidence cannot be `Factual`

### Sprint 4 additions (sprints/008-data-driven-simulator, Gate 1)

Every field below is optional and absent by default, so a file written
before this section still loads byte-for-byte unchanged.

- `trigger_rule: "NotApplicable"` - the record is classified, not executed.
  It is the **only** legal trigger on a record that carries `coverage`, and
  it is illegal anywhere else. Before this pairing the required field was
  filled with `"Passive"` on 582 coverage blocks, and a later census read
  that filler back as a factual claim.
- New trigger kinds: `OnBlock`, `OnSteal`, `OnStealthEnter`, `OnStealthExit`,
  `OnLegendSwap`, `OnBerserkEnter`, `OnSymbolHit`, `OnExplosion`,
  `OnStunbreak`, and `OnBoonGained { boon }` (an absent `boon` means any).
- `category: { "MechanicUnlock": { "mechanic", "replaces" } }` - the source
  unlocks or replaces a profession mechanic. A capability, never a stat: it
  carries no `value`.
- `stacking_rule: "RefreshAllStacks"` - gaining a stack refreshes every
  stack already held. Any other rule leaves each stack on its own clock.
- `actor` - `Player` (default), `Pet`, `Illusion` or `Any`. A pet's on-crit
  record must not fire off the player's crits.
- `gates: []` - all must hold for the record to fire:
  `InCombat`, `Interval { every_ms, while }`, `Weapon { types, hand }`,
  `Positional`, `Proximity { radius, min_targets }`,
  `HealthThreshold { below_pct, above_pct, rearm }`, `SelfBoon { boon }`,
  `SelfResourceStacks { resource, min }`. `rearm` is `OncePerFight`,
  `WhenRecovered` or `Icd`. `while` takes the same block as `prerequisite`.
- `scale` - live state added to `value` when the record fires:
  `PerDistance { per_unit, cap }`, `PerSelfResourceStack { resource,
  per_stack, cap }`, `PerSelfBoon { boon, per_stack, cap }`. The value is
  `value + step * min(n, cap)`.

Validation rules 15-18:

15. `coverage` and `trigger_rule: NotApplicable` imply each other.
16. `MechanicUnlock` needs a mechanic name and carries no resolved value.
17. `Interval.every_ms > 0`; a `Weapon` gate names at least one real weapon
    type; `Proximity` needs `radius > 0` and `min_targets >= 1`;
    `HealthThreshold` needs at least one side, each in `1..=99`;
    `SelfResourceStacks` needs a resource and `min >= 1`.
18. A `scale` step is finite and non-zero, its `cap` positive, and a
    resource or boon scale names one.

A gate or scale the simulator has no state for (positional facing, foe
distance, a resource pool it does not keep, a trigger with no emission site)
makes the record **abstain** with that reason on the coverage line. It never
passes silently (doctrine rule 6).

## Schema 10: Rotation Profiles

Path:

```text
data/rotation_profiles/<mode>.json
```

Purpose:

- capture heuristics for stack counts, uptime, target movement, and trigger frequency

Schema:

```json
{
  "mode": "PvE",
  "profiles": [
    {
      "profile_id": "pve-necromancer-scourge-condi",
      "profession": "Necromancer",
      "elite_spec": "Scourge",
      "objective_profile_id": "PvE_Condi_DPS",
      "condition_application": {
        "Bleeding": 8.0,
        "Burning": 1.0,
        "Poison": 1.5,
        "Torment": 6.0,
        "Confusion": 0.1
      },
      "target_behavior": {
        "movement_fraction": 0.35,
        "skill_use_frequency_per_second": 0.2
      },
      "buff_uptime": {
        "Might": 1.0,
        "Fury": 1.0,
        "Quickness": 1.0,
        "Alacrity": 1.0
      },
      "evidence_level": "Heuristic",
      "notes": "Temporary profile until simulation-backed rotation datasets exist."
    }
  ]
}
```

Validation rules:

1. these files are heuristic by default
2. every profile must declare assumptions explicitly
3. no profile may pretend to be factual unless backed by a stronger dataset

## Schema 11: Objective Profiles

Path:

```text
data/objective_profiles/<mode>.json
```

Purpose:

- define mode-specific scoring intents

Schema:

```json
{
  "mode": "WvW",
  "profiles": [
    {
      "objective_profile_id": "WvW_Roaming_Condi",
      "axis_weights": {
        "power": 0.1,
        "disable": 0.4,
        "condition": 0.8,
        "healing": 0.1,
        "sustain": 0.6
      },
      "notes": "Heuristic scoring profile for roaming pressure and sustain.",
      "evidence_level": "Heuristic"
    }
  ]
}
```

Validation rules:

1. objective profiles are heuristic
2. weight budget constraints must be validated by loader code
3. objective profiles must be mode-specific

## Loader Rules

The codebase should expose typed loaders for each dataset.

Recommended modules:

```text
crates/optimizer/src/data/
  manifests.rs
  profession_profiles.rs
  formulas.rs
  slot_budgets.rs
  balance_overrides.rs
  normalized_effects.rs
  rotation_profiles.rs
  objective_profiles.rs
```

Required loader behavior:

1. reject malformed enum values
2. reject duplicate IDs
3. reject patch/mode mismatches
4. preserve `Unknown` explicitly
5. surface typed errors, not `String`

## Validation Matrix

Every dataset must have:

1. parse tests
2. schema invariant tests
3. cross-file consistency tests

Required cross-file checks:

1. every profession in code exists in `profession_profiles.json`
2. every patch override references a valid patch manifest
3. every objective profile references a valid mode
4. every rotation profile references a valid objective profile
5. every normalized effect field belongs to the allowed category set

## Migration Rules

When adding a new patch:

1. create a new manifest
2. create a new patch ledger
3. create empty override files for all three modes
4. mark unresolved changed values as `Unknown`
5. only promote to `Factual` after linking a source

When changing an existing schema:

1. update this document first
2. add loader migration code if required
3. add regression tests

## Minimal First Implementation

The first coding sprint does not need every schema at once.

Recommended minimum:

1. `profession_profiles.json`
2. `formulas/universal.json`
3. `formulas/conditions.json`
4. `formulas/boons.json`
5. `slot_budgets/level80_ascended.json`
6. `manifests/<patch>.json`
7. `patch_ledgers/<patch>.yaml`

Second wave:

1. `balance_overrides/<patch>/<mode>.json`
2. `normalized_effects/<patch>/<mode>.json`

Third wave:

1. `rotation_profiles/<mode>.json`
2. `objective_profiles/<mode>.json`

## Coding-Agent Acceptance Criteria

A coding agent implementing this schema layer is done only when:

1. all schema files exist for the minimum first implementation
2. loaders are typed and tested
3. validation catches duplicate IDs and invalid enums
4. patch manifests and ledgers are wired into code
5. no optimizer math path hardcodes values that now live in schema files

