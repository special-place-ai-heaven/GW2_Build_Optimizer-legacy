//! NeedsMechanic Engine E3: Elementalist AttunementState.
//!
//! One state owns current (+ Weaver secondary). Profession attune skills
//! mutate it; swaps emit TriggerBus::OnAttunementSwap. While-attuned is
//! `is(current)` / optional `prerequisite.attunement`. Not an aura engine.

use super::trigger_bus::{BusEvent, TriggerBus};

/// The four elemental attunements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Element {
    Fire,
    Water,
    Air,
    Earth,
}

impl Element {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fire => "Fire",
            Self::Water => "Water",
            Self::Air => "Air",
            Self::Earth => "Earth",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "fire" => Some(Self::Fire),
            "water" => Some(Self::Water),
            "air" => Some(Self::Air),
            "earth" => Some(Self::Earth),
            _ => None,
        }
    }

    /// Map a profession attunement skill name to an element.
    pub fn from_skill_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if !lower.contains("attunement") {
            return None;
        }
        if lower.contains("fire") {
            Some(Self::Fire)
        } else if lower.contains("water") {
            Some(Self::Water)
        } else if lower.contains("air") {
            Some(Self::Air)
        } else if lower.contains("earth") {
            Some(Self::Earth)
        } else {
            None
        }
    }
}

/// Sole writer of current attunement (+ Weaver secondary on this same state).
/// Shared by `simulator` and `wvw_timeline`. Default primary = Fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttunementState {
    pub current: Element,
    /// Weaver dual attunement: previous primary after a swap. Non-Weaver: None.
    pub secondary: Option<Element>,
    /// When true, swaps stash outgoing primary into `secondary`.
    pub weaver: bool,
}

impl Default for AttunementState {
    fn default() -> Self {
        Self {
            current: Element::Fire,
            secondary: None,
            weaver: false,
        }
    }
}

impl AttunementState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_weaver(mut self, weaver: bool) -> Self {
        self.weaver = weaver;
        self
    }

    /// Shared constructor for both sim surfaces (flow + WvW timeline).
    pub fn for_build(is_weaver: bool) -> Self {
        Self::new().with_weaver(is_weaver)
    }

    /// While-attuned check: true iff `current` is `element`.
    pub fn is(&self, element: Element) -> bool {
        self.current == element
    }

    /// Usability check for an attunement prerequisite: a Weaver is attuned to
    /// both halves of its dual attunement, so a skill gated on `element` is
    /// usable off the stashed secondary too. Non-Weaver: same as `is`.
    pub fn is_attuned(&self, element: Element) -> bool {
        self.is(element) || (self.weaver && self.secondary == Some(element))
    }

    /// Switch primary to `to`. Returns `Some((from, to))` on a real swap;
    /// `None` when already on `to` (no emit). Weaver: secondary = outgoing
    /// primary. Non-Weaver: secondary = None.
    pub fn swap(&mut self, to: Element) -> Option<(Element, Element)> {
        if self.current == to {
            return None;
        }
        let from = self.current;
        if self.weaver {
            self.secondary = Some(from);
        } else {
            self.secondary = None;
        }
        self.current = to;
        Some((from, to))
    }
}

/// GW2 API elite specialization id for Weaver (Elementalist). Not 65 (Willbender).
pub const WEAVER_SPEC_ID: u32 = 56;

/// Weaver spec id, or a Weaver-labeled source/skill name (`Weaver`,
/// `Weaver's Prowess`, `Weave Self`). Skill ids are not spec ids.
pub fn is_weaver_source(id: u32, name: &str) -> bool {
    id == WEAVER_SPEC_ID || is_weaver_name(name)
}

pub fn is_weaver_name(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    n == "weave self" || n.starts_with("weaver")
}

/// Mutate AttunementState from a profession attune skill and emit
/// OnAttunementSwap on the shared TriggerBus iff the primary changed.
/// Returns the new element when a swap landed.
pub fn apply_attunement_skill(
    state: &mut AttunementState,
    bus: &mut TriggerBus,
    at_ms: u32,
    skill_name: &str,
) -> Option<Element> {
    let to = Element::from_skill_name(skill_name)?;
    state.swap(to)?;
    bus.emit(BusEvent::OnAttunementSwap, at_ms);
    Some(to)
}

#[cfg(test)]
mod kent_tests {
    use super::*;
    use crate::rotation::trigger_bus::TriggerBus;

    #[test]
    fn kent_e3_fire_to_water_changes_current() {
        let mut state = AttunementState::new();
        let mut bus = TriggerBus::new();
        assert!(state.is(Element::Fire));
        assert!(!state.is(Element::Water));
        let landed = apply_attunement_skill(&mut state, &mut bus, 100, "Water Attunement");
        assert_eq!(landed, Some(Element::Water));
        assert!(state.is(Element::Water));
        assert!(!state.is(Element::Fire));
        assert_eq!(state.secondary, None);
        assert_eq!(bus.count(BusEvent::OnAttunementSwap), 1);
    }

    #[test]
    fn kent_e3_while_earth_off_in_fire_on_in_earth() {
        let mut state = AttunementState::new();
        assert!(!state.is(Element::Earth));
        let mut bus = TriggerBus::new();
        apply_attunement_skill(&mut state, &mut bus, 0, "Earth Attunement");
        assert!(state.is(Element::Earth));
        assert!(!state.is(Element::Fire));
    }

    #[test]
    fn kent_e3_weaver_secondary_is_outgoing_primary() {
        let mut state = AttunementState::new().with_weaver(true);
        let mut bus = TriggerBus::new();
        apply_attunement_skill(&mut state, &mut bus, 0, "Air Attunement");
        assert_eq!(state.current, Element::Air);
        assert_eq!(state.secondary, Some(Element::Fire));
        apply_attunement_skill(&mut state, &mut bus, 50, "Earth Attunement");
        assert_eq!(state.current, Element::Earth);
        assert_eq!(state.secondary, Some(Element::Air));
    }

    /// FCR-004: a Weaver stays attuned to the stashed half, so a prerequisite
    /// on the outgoing element still holds after the swap. A core Elementalist
    /// with the same swap history is only attuned to `current`.
    #[test]
    fn fcr004_weaver_is_attuned_to_secondary() {
        let mut weaver = AttunementState::for_build(true);
        weaver.swap(Element::Water);
        assert!(weaver.is_attuned(Element::Water), "current half counts");
        assert!(weaver.is_attuned(Element::Fire), "stashed half counts too");
        assert!(!weaver.is(Element::Fire), "`is` stays current-only");
        assert!(
            !weaver.is_attuned(Element::Air),
            "an unheld element does not"
        );

        let mut core = AttunementState::for_build(false);
        core.swap(Element::Water);
        assert!(core.is_attuned(Element::Water));
        assert!(
            !core.is_attuned(Element::Fire),
            "a core Elementalist holds one attunement"
        );
    }

    #[test]
    fn kent_e3_same_attunement_is_noop() {
        let mut state = AttunementState::new();
        let mut bus = TriggerBus::new();
        assert!(apply_attunement_skill(&mut state, &mut bus, 0, "Fire Attunement").is_none());
        assert_eq!(bus.count(BusEvent::OnAttunementSwap), 0);
        assert!(state.is(Element::Fire));
    }

    #[test]
    fn for_build_enables_weaver_secondary() {
        let mut weaver = AttunementState::for_build(true);
        let mut core = AttunementState::for_build(false);
        assert!(weaver.weaver);
        assert!(!core.weaver);
        assert_eq!(WEAVER_SPEC_ID, 56);
        assert!(is_weaver_source(WEAVER_SPEC_ID, "Trait"));
        assert!(!is_weaver_source(65, "Willbender"));
        assert!(is_weaver_name("Weaver's Prowess"));
        assert!(is_weaver_name("Weave Self"));
        assert!(!is_weaver_name("Water Attunement"));
        weaver.swap(Element::Water);
        core.swap(Element::Water);
        assert_eq!(weaver.secondary, Some(Element::Fire));
        assert_eq!(core.secondary, None);
    }
}
