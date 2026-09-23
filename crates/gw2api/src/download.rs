//! Full data download orchestration.
//! Downloads all game data endpoints and caches them locally.
//!
//! Ada FOLD3 (SCHEMA N): Refresh is idempotent per KEPT catalog key.
//! - `RefreshMode::Default` + same `CacheEntry.build` as live `/v2/build`:
//!   skip body fetches **and** id-list probes for that key.
//! - Build mismatch / `Verify`: refresh KEPT only, compare-before-write.
//! - Items keep-set is the type+rarity filter plus level-80 Food / Utility
//!   consumables (`consumable_is_kept`); Refresh never
//!   body-fetches the discarded bulk when `items.json` already exists.
//!
//! First-fill (no `items.json`) persists DataCache key `items.partial` after
//! each completed 200-id batch so a killed install resumes
//! (`live_ids` minus `fetched_ids`). Warm-complete is `exists("items")` only;
//! partial is never treated as warm. `refresh_items` is unchanged.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cache::DataCache;
use crate::client::{with_cancel_bridge, ApiError, Gw2Client};
use crate::models;

/// Progress update sent during download.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub current_step: usize,
    pub total_steps: usize,
    pub step_name: String,
    pub done: bool,
    /// Optional sub-step detail (e.g. "batch 5/500")
    pub detail: Option<String>,
    /// Intra-step counts (items/icons). `0` total means no inner bar.
    pub inner_done: usize,
    pub inner_total: usize,
}

/// How Refresh Game Data treats KEPT catalogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshMode {
    /// Skip a KEPT key when `CacheEntry.build == live /v2/build`.
    /// On build mismatch, incremental refresh + compare-before-write.
    #[default]
    Default,
    /// Refetch KEPT catalogs and compare even when the build matches.
    /// Still never walks discarded (non-keep) item bodies when `items.json` exists.
    Verify,
}

/// Hopper UX probe for the items catalog path.
///
/// `None` is the existing Refresh path (build mismatch / Verify) — not a
/// FirstFill / Resume / SameBuildSkip string. Partial is ignored when
/// `items` already exists (warm-complete).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemsFillKind {
    FirstFill,
    Resume,
    SameBuildSkip,
}

/// Classify the items path for Hopper. Looks at `items` then `items.partial`.
/// `needs_catalog_refresh("items")` / `exists("items")` still ignore partial.
pub fn items_fill_kind(
    cache: &DataCache,
    live_build: u32,
    mode: RefreshMode,
) -> Option<ItemsFillKind> {
    if cache.exists("items") {
        if mode == RefreshMode::Default && !cache.is_stale("items", live_build) {
            Some(ItemsFillKind::SameBuildSkip)
        } else {
            None
        }
    } else if cache.exists(ITEMS_PARTIAL_KEY) {
        Some(ItemsFillKind::Resume)
    } else {
        Some(ItemsFillKind::FirstFill)
    }
}

const TOTAL_STEPS: usize = 10;
const ITEMS_IDS_KEY: &str = "items.ids";
const ITEMS_PARTIAL_KEY: &str = "items.partial";

const RELEVANT_TYPES: &[&str] = &[
    "Armor",
    "Weapon",
    "Trinket",
    "Back",
    "UpgradeComponent",
    "Relic",
];
const RELEVANT_RARITIES: &[&str] = &["Exotic", "Ascended", "Legendary"];

fn report(
    on_progress: &mut impl FnMut(DownloadProgress),
    step: &mut usize,
    name: &str,
    detail: Option<String>,
) {
    *step += 1;
    on_progress(DownloadProgress {
        current_step: *step,
        total_steps: TOTAL_STEPS,
        step_name: name.to_string(),
        done: *step >= TOTAL_STEPS,
        detail,
        inner_done: 0,
        inner_total: 0,
    });
}

/// `download_missing` already skips per-icon failures. Errors that escape
/// it are terminal: `Cancelled` stays `Cancelled`; `Cache` (graphics-dir
/// create) and any other `Err` must fail the refresh so the UI cannot
/// report success when icons can never be written.
fn propagate_icon_step<T>(result: Result<T, ApiError>) -> Result<T, ApiError> {
    result
}

fn needs_catalog_refresh(cache: &DataCache, key: &str, build: u32, mode: RefreshMode) -> bool {
    match mode {
        RefreshMode::Default => cache.is_stale(key, build),
        RefreshMode::Verify => true,
    }
}

fn values_eq<T: Serialize>(a: &T, b: &T) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(va), Ok(vb)) => va == vb,
        _ => false,
    }
}

fn item_is_kept(item: &models::Item) -> bool {
    (RELEVANT_TYPES.contains(&item.item_type.as_str())
        && RELEVANT_RARITIES.contains(&item.rarity.as_str()))
        || consumable_is_kept(item)
}

/// Food (Nourishment) and Utility (Enhancement) a level-80 character eats;
/// ascended feasts are level 80 too. Every other consumable stays out.
fn consumable_is_kept(item: &models::Item) -> bool {
    item.item_type == "Consumable"
        && item.level == 80
        && matches!(
            item.details.as_ref().and_then(|d| d.detail_type.as_deref()),
            Some("Food" | "Utility")
        )
}

/// One-off upgrade for an `items.json` written before Food / Utility were
/// kept. Refresh never body-fetches the discarded bulk, so an existing cache
/// only gains them here (or on a first fill): walk every live id not already
/// cached, append the kept consumables, keep the cached build stamp. Returns
/// the number of rows added; `items.json` is not rewritten when it is zero.
///
/// Runs only while the cache holds no kept consumable, so once it has
/// completed it never walks again; a cancelled walk wrote nothing and the
/// next Refresh redoes it.
pub fn backfill_consumables(
    client: &Gw2Client,
    cache: &DataCache,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<usize, ApiError> {
    let mut items: Vec<models::Item> = cache
        .load("items")
        .map_err(cache_err)?
        .ok_or_else(|| cache_err("no items cache to backfill"))?;
    if items.iter().any(consumable_is_kept) {
        return Ok(0);
    }
    let build = cache.cached_build("items").unwrap_or(0);
    let have: HashSet<u32> = items.iter().map(|i| i.id).collect();
    let live: Vec<serde_json::Value> = client.get("items")?;
    let todo: Vec<serde_json::Value> = parse_u32_ids(&live)
        .into_iter()
        .filter(|id| !have.contains(id))
        .map(|id| serde_json::json!(id))
        .collect();
    let before = items.len();
    let mut done = 0;
    for slice in todo.chunks(crate::client::MAX_BULK_IDS * 10) {
        let (raw, _skipped): (Vec<serde_json::Value>, _) =
            client.fetch_by_ids_with_skips("items", slice, |fetched, _| {
                on_progress(done + fetched, todo.len())
            })?;
        items.extend(
            raw.into_iter()
                .filter_map(|v| serde_json::from_value::<models::Item>(v).ok())
                .filter(consumable_is_kept),
        );
        done += slice.len();
        on_progress(done, todo.len());
    }
    let added = items.len() - before;
    if added > 0 {
        cache.save("items", &items, build).map_err(cache_err)?;
    }
    Ok(added)
}

fn cache_err(e: impl ToString) -> ApiError {
    ApiError::Cache(e.to_string())
}

fn save_or_stamp<T: Serialize>(
    cache: &DataCache,
    key: &str,
    data: &T,
    build: u32,
    data_changed: bool,
) -> Result<(), ApiError> {
    if data_changed || !cache.exists(key) {
        cache.save(key, data, build).map_err(cache_err)
    } else {
        // All rows equal: do not rewrite the data payload; stamp build so the
        // next Default pass is a same-build skip.
        cache.stamp_build(key, build).map_err(cache_err)
    }
}

/// Ids to body-fetch on an items Refresh when `items.json` already exists.
///
/// `previous_live` is the last persisted `/v2/items` id list (`items.ids`).
/// `cached_keep` is ids currently stored in `items.json`.
/// Fetch set = (live \ previous) ∪ (cached ∩ live) — never the discarded bulk.
pub(crate) fn items_refresh_fetch_ids(
    live_ids: &[u32],
    previous_live: &[u32],
    cached_keep: &[u32],
) -> Vec<u32> {
    let previous: HashSet<u32> = previous_live.iter().copied().collect();
    let cached: HashSet<u32> = cached_keep.iter().copied().collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for &id in live_ids {
        let is_new = !previous.contains(&id);
        let is_surviving_cached = cached.contains(&id);
        if (is_new || is_surviving_cached) && seen.insert(id) {
            out.push(id);
        }
    }
    out
}

/// Merge fetched item bodies into the kept vec. Equal rows reuse `old`.
/// Vanished cached ids (not in `live_ids`) are dropped.
/// Returns `(merged, any_logical_change)`.
pub(crate) fn merge_kept_items(
    old: &[models::Item],
    fetched: Vec<models::Item>,
    live_ids: &[u32],
) -> (Vec<models::Item>, bool) {
    let live_set: HashSet<u32> = live_ids.iter().copied().collect();
    let mut fetched_map: HashMap<u32, models::Item> = fetched
        .into_iter()
        .filter(item_is_kept)
        .map(|i| (i.id, i))
        .collect();

    let mut changed = false;
    let mut merged = Vec::with_capacity(old.len());
    let mut emitted = HashSet::new();

    for old_item in old {
        if !live_set.contains(&old_item.id) {
            changed = true;
            continue;
        }
        emitted.insert(old_item.id);
        match fetched_map.remove(&old_item.id) {
            Some(new_item) if values_eq(old_item, &new_item) => {
                merged.push(old_item.clone());
            }
            Some(new_item) => {
                changed = true;
                merged.push(new_item);
            }
            None => merged.push(old_item.clone()),
        }
    }

    for &id in live_ids {
        if emitted.contains(&id) {
            continue;
        }
        if let Some(item) = fetched_map.remove(&id) {
            changed = true;
            merged.push(item);
        }
    }

    (merged, changed)
}

fn merge_by_id<K, T>(old: Vec<T>, new_rows: Vec<T>, id_of: impl Fn(&T) -> K) -> (Vec<T>, bool)
where
    K: Eq + Hash + Clone,
    T: Serialize + Clone,
{
    let old_map: HashMap<K, T> = old
        .into_iter()
        .map(|r| {
            let id = id_of(&r);
            (id, r)
        })
        .collect();
    let old_len = old_map.len();
    let mut merged = Vec::with_capacity(new_rows.len());
    let mut equal_reused = 0usize;
    let mut seen_new = HashSet::new();

    for new_row in new_rows {
        let id = id_of(&new_row);
        seen_new.insert(id.clone());
        if let Some(old_row) = old_map.get(&id) {
            if values_eq(old_row, &new_row) {
                merged.push(old_row.clone());
                equal_reused += 1;
            } else {
                merged.push(new_row);
            }
        } else {
            merged.push(new_row);
        }
    }

    let vanished = old_map.keys().any(|k| !seen_new.contains(k));
    let all_equal = !vanished && equal_reused == merged.len() && merged.len() == old_len;
    (merged, !all_equal)
}

fn refresh_fetch_all_catalog<T, K>(
    client: &Gw2Client,
    cache: &DataCache,
    key: &str,
    endpoint: &str,
    build: u32,
    id_of: impl Fn(&T) -> K,
) -> Result<(), ApiError>
where
    T: DeserializeOwned + Serialize + Clone + Send,
    K: Eq + Hash + Clone,
{
    let old: Vec<T> = cache.load(key).map_err(cache_err)?.unwrap_or_default();
    let new_rows: Vec<T> = client.fetch_all(endpoint)?;
    let (merged, changed) = merge_by_id(old, new_rows, id_of);
    save_or_stamp(cache, key, &merged, build, changed)
}

fn refresh_ids_all_catalog<T, K>(
    client: &Gw2Client,
    cache: &DataCache,
    key: &str,
    endpoint: &str,
    params: &[(&str, &str)],
    build: u32,
    id_of: impl Fn(&T) -> K,
) -> Result<(), ApiError>
where
    T: DeserializeOwned + Serialize + Clone,
    K: Eq + Hash + Clone,
{
    let old: Vec<T> = cache.load(key).map_err(cache_err)?.unwrap_or_default();
    let new_rows: Vec<T> = client.get_with_params(endpoint, params)?;
    let (merged, changed) = merge_by_id(old, new_rows, id_of);
    save_or_stamp(cache, key, &merged, build, changed)
}

fn parse_u32_ids(raw: &[serde_json::Value]) -> Vec<u32> {
    raw.iter()
        .filter_map(|v| v.as_u64().map(|n| n as u32))
        .collect()
}

/// Durable first-fill cursor. Sidecar via DataCache save/load/delete only.
/// `fetched_ids` = every id whose body was already requested (keep AND discarded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ItemsPartial {
    pub live_ids: Vec<u32>,
    pub fetched_ids: Vec<u32>,
    pub kept: Vec<models::Item>,
    pub skipped: Vec<serde_json::Value>,
}

/// Body-fetch set on install/resume: `live_ids \ fetched_ids`.
pub(crate) fn items_install_remaining(live_ids: &[u32], fetched_ids: &[u32]) -> Vec<u32> {
    let fetched: HashSet<u32> = fetched_ids.iter().copied().collect();
    live_ids
        .iter()
        .copied()
        .filter(|id| !fetched.contains(id))
        .collect()
}

fn load_or_new_partial(cache: &DataCache, live_ids: &[u32]) -> Result<ItemsPartial, ApiError> {
    Ok(
        match cache
            .load::<ItemsPartial>(ITEMS_PARTIAL_KEY)
            .map_err(cache_err)?
        {
            Some(p) => p,
            None => ItemsPartial {
                live_ids: live_ids.to_vec(),
                fetched_ids: Vec::new(),
                kept: Vec::new(),
                skipped: Vec::new(),
            },
        },
    )
}

fn persist_items_partial(
    cache: &DataCache,
    partial: &ItemsPartial,
    build: u32,
) -> Result<(), ApiError> {
    cache
        .save(ITEMS_PARTIAL_KEY, partial, build)
        .map_err(cache_err)
}

fn apply_install_batch(
    partial: &mut ItemsPartial,
    chunk: &[serde_json::Value],
    raw_items: Vec<serde_json::Value>,
    batch_skipped: Vec<serde_json::Value>,
) {
    let mut fetched: HashSet<u32> = partial.fetched_ids.iter().copied().collect();
    for id in parse_u32_ids(chunk) {
        if fetched.insert(id) {
            partial.fetched_ids.push(id);
        }
    }
    let mut kept_ids: HashSet<u32> = partial.kept.iter().map(|i| i.id).collect();
    for val in raw_items {
        if let Ok(item) = serde_json::from_value::<models::Item>(val) {
            if item_is_kept(&item) && kept_ids.insert(item.id) {
                partial.kept.push(item);
            }
        }
    }
    partial.skipped.extend(batch_skipped);
}

fn finish_items_install(
    cache: &DataCache,
    partial: &ItemsPartial,
    live_ids: &[u32],
    build: u32,
) -> Result<(), ApiError> {
    let live_set: HashSet<u32> = live_ids.iter().copied().collect();
    let kept: Vec<models::Item> = partial
        .kept
        .iter()
        .filter(|i| live_set.contains(&i.id))
        .cloned()
        .collect();
    cache.save("items", &kept, build).map_err(cache_err)?;
    cache
        .save(ITEMS_IDS_KEY, &live_ids, build)
        .map_err(cache_err)?;
    cache
        .save("items.skipped", &partial.skipped, build)
        .map_err(cache_err)?;
    cache.delete(ITEMS_PARTIAL_KEY);
    Ok(())
}

fn install_items(
    client: &Gw2Client,
    cache: &DataCache,
    build: u32,
    step: usize,
    on_progress: &mut impl FnMut(DownloadProgress),
) -> Result<(), ApiError> {
    on_progress(DownloadProgress {
        current_step: step,
        total_steps: TOTAL_STEPS,
        step_name: "Items (equipment)".to_string(),
        done: false,
        detail: Some("fetching item IDs...".into()),
        inner_done: 0,
        inner_total: 0,
    });
    let ids: Vec<serde_json::Value> = client.get("items")?;
    let live_ids = parse_u32_ids(&ids);

    let mut partial = load_or_new_partial(cache, &live_ids)?;
    partial.live_ids = live_ids.clone();
    let live_set: HashSet<u32> = live_ids.iter().copied().collect();
    partial.kept.retain(|item| live_set.contains(&item.id));

    let remaining = items_install_remaining(&live_ids, &partial.fetched_ids);
    let remaining_vals: Vec<serde_json::Value> =
        remaining.iter().map(|id| serde_json::json!(*id)).collect();
    let batches: Vec<&[serde_json::Value]> =
        remaining_vals.chunks(crate::client::MAX_BULK_IDS).collect();

    on_progress(DownloadProgress {
        current_step: step,
        total_steps: TOTAL_STEPS,
        step_name: "Items (equipment)".to_string(),
        done: false,
        detail: Some(format!(
            "{} / {} items fetched",
            partial.fetched_ids.len(),
            live_ids.len()
        )),
        inner_done: partial.fetched_ids.len(),
        inner_total: live_ids.len(),
    });

    for group in batches.chunks(5) {
        if client.is_cancelled() {
            return Err(ApiError::Cancelled);
        }
        #[allow(clippy::type_complexity)]
        let group_results: Vec<
            Result<(Vec<serde_json::Value>, Vec<serde_json::Value>), ApiError>,
        > = std::thread::scope(|s| {
            let handles: Vec<_> = group
                .iter()
                .map(|chunk| s.spawn(|| client.fetch_bulk_chunk("items", chunk)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        Err(ApiError::Internal(
                            "Batch fetch thread panicked on items".into(),
                        ))
                    })
                })
                .collect()
        });

        for (chunk, batch_result) in group.iter().zip(group_results) {
            let (raw_items, batch_skipped) = batch_result?;
            apply_install_batch(&mut partial, chunk, raw_items, batch_skipped);
            persist_items_partial(cache, &partial, build)?;
            on_progress(DownloadProgress {
                current_step: step,
                total_steps: TOTAL_STEPS,
                step_name: "Items (equipment)".to_string(),
                done: false,
                detail: Some(format!(
                    "{} / {} items fetched",
                    partial.fetched_ids.len(),
                    live_ids.len()
                )),
                inner_done: partial.fetched_ids.len(),
                inner_total: live_ids.len(),
            });
        }
    }

    finish_items_install(cache, &partial, &live_ids, build)
}

fn refresh_items(
    client: &Gw2Client,
    cache: &DataCache,
    build: u32,
    step: usize,
    on_progress: &mut impl FnMut(DownloadProgress),
) -> Result<(), ApiError> {
    let old: Vec<models::Item> = cache.load("items").map_err(cache_err)?.unwrap_or_default();

    on_progress(DownloadProgress {
        current_step: step,
        total_steps: TOTAL_STEPS,
        step_name: "Items (equipment)".to_string(),
        done: false,
        detail: Some("fetching item IDs...".into()),
        inner_done: 0,
        inner_total: 0,
    });
    let live_raw: Vec<serde_json::Value> = client.get("items")?;
    let live_ids = parse_u32_ids(&live_raw);

    let previous: Vec<u32> = match cache.load::<Vec<u32>>(ITEMS_IDS_KEY).map_err(cache_err)? {
        Some(ids) => ids,
        // Upgrade path: no id snapshot yet. Treat live as already-seen so we
        // only revalidate the keep-set (never body-fetch the discarded bulk).
        None => live_ids.clone(),
    };
    let cached_keep: Vec<u32> = old.iter().map(|i| i.id).collect();
    let fetch_ids = items_refresh_fetch_ids(&live_ids, &previous, &cached_keep);
    let fetch_vals: Vec<serde_json::Value> =
        fetch_ids.iter().map(|id| serde_json::json!(*id)).collect();

    let (raw_items, skipped): (Vec<serde_json::Value>, Vec<serde_json::Value>) = client
        .fetch_by_ids_with_skips("items", &fetch_vals, |fetched, total| {
            on_progress(DownloadProgress {
                current_step: step,
                total_steps: TOTAL_STEPS,
                step_name: "Items (equipment)".to_string(),
                done: false,
                detail: Some(format!("{fetched} / {total} items fetched")),
                inner_done: fetched,
                inner_total: total,
            });
        })?;

    let fetched: Vec<models::Item> = raw_items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let (merged, changed) = merge_kept_items(&old, fetched, &live_ids);
    save_or_stamp(cache, "items", &merged, build, changed)?;
    cache
        .save("items.skipped", &skipped, build)
        .map_err(cache_err)?;
    cache
        .save(ITEMS_IDS_KEY, &live_ids, build)
        .map_err(cache_err)?;
    Ok(())
}

/// Download all game data, calling `on_progress` after each endpoint.
/// Skips endpoints that are already cached at the current build when
/// `mode` is [`RefreshMode::Default`]. Returns the game build number on success.
///
/// `cancelled` is checked between steps *and* bridged into `client`'s cancel
/// flag (see `with_cancel_bridge`), because the waits worth interrupting -
/// retry backoff, rate-limit sleeps - happen inside `client` while this thread
/// is blocked and cannot poll anything.
pub fn download_all(
    client: &Gw2Client,
    cache: &DataCache,
    cancelled: impl Fn() -> bool + Sync,
    mode: RefreshMode,
    on_progress: impl FnMut(DownloadProgress),
) -> Result<u32, ApiError> {
    if cancelled() {
        client.cancel();
        return Err(ApiError::Cancelled);
    }
    with_cancel_bridge(client, &cancelled, || {
        download_steps(client, cache, &cancelled, mode, on_progress)
    })
}

/// The ten download steps. Split out so `download_all` reads as "arm
/// cancellation, run the steps, disarm".
fn download_steps(
    client: &Gw2Client,
    cache: &DataCache,
    cancelled: &impl Fn() -> bool,
    mode: RefreshMode,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<u32, ApiError> {
    let check = || {
        if cancelled() {
            Err(ApiError::Cancelled)
        } else {
            Ok(())
        }
    };

    let build = client.get_build_number()?;
    let mut step = 0;

    check()?;
    if needs_catalog_refresh(cache, "itemstats", build, mode) {
        refresh_fetch_all_catalog::<models::ItemStat, _>(
            client,
            cache,
            "itemstats",
            "itemstats",
            build,
            |r| r.id,
        )?;
    }
    report(&mut on_progress, &mut step, "Item stats", None);

    check()?;
    if needs_catalog_refresh(cache, "specializations", build, mode) {
        refresh_fetch_all_catalog::<models::Specialization, _>(
            client,
            cache,
            "specializations",
            "specializations",
            build,
            |r| r.id,
        )?;
    }
    report(&mut on_progress, &mut step, "Specializations", None);

    check()?;
    if needs_catalog_refresh(cache, "traits", build, mode) {
        refresh_fetch_all_catalog::<models::Trait, _>(
            client,
            cache,
            "traits",
            "traits",
            build,
            |r| r.id,
        )?;
    }
    report(&mut on_progress, &mut step, "Traits", None);

    check()?;
    if needs_catalog_refresh(cache, "skills", build, mode) {
        refresh_fetch_all_catalog::<models::Skill, _>(
            client,
            cache,
            "skills",
            "skills",
            build,
            |r| r.id,
        )?;
    }
    report(&mut on_progress, &mut step, "Skills", None);

    // Schema version that includes skills_by_palette.
    check()?;
    if needs_catalog_refresh(cache, "professions", build, mode) {
        refresh_ids_all_catalog::<models::Profession, _>(
            client,
            cache,
            "professions",
            "professions",
            &[("ids", "all"), ("v", "2019-12-19T00:00:00.000Z")],
            build,
            |r| r.id.clone(),
        )?;
    }
    report(&mut on_progress, &mut step, "Professions", None);

    // Schema version that includes template `code`.
    check()?;
    if needs_catalog_refresh(cache, "legends", build, mode) {
        refresh_ids_all_catalog::<models::Legend, _>(
            client,
            cache,
            "legends",
            "legends",
            &[("ids", "all"), ("v", "2019-12-19T00:00:00.000Z")],
            build,
            |r| r.id.clone(),
        )?;
    }
    report(&mut on_progress, &mut step, "Legends", None);

    check()?;
    if needs_catalog_refresh(cache, "pets", build, mode) {
        refresh_fetch_all_catalog::<models::Pet, _>(client, cache, "pets", "pets", build, |r| {
            r.id
        })?;
    }
    report(&mut on_progress, &mut step, "Pets", None);

    check()?;
    if needs_catalog_refresh(cache, "pvp_amulets", build, mode) {
        refresh_fetch_all_catalog::<models::PvpAmulet, _>(
            client,
            cache,
            "pvp_amulets",
            "pvp/amulets",
            build,
            |r| r.id,
        )?;
    }
    report(&mut on_progress, &mut step, "PvP Amulets", None);

    check()?;
    let had_items = cache.exists("items");
    if needs_catalog_refresh(cache, "items", build, mode) {
        if had_items {
            refresh_items(client, cache, build, step, &mut on_progress)?;
        } else {
            install_items(client, cache, build, step, &mut on_progress)?;
        }
    }
    // A cache filled before Food / Utility were kept gains them once, on any
    // Refresh (same-build skip included). A first fill already kept them.
    if had_items {
        backfill_consumables(client, cache, |done, total| {
            on_progress(DownloadProgress {
                current_step: step,
                total_steps: TOTAL_STEPS,
                step_name: "Items (food and utility)".to_string(),
                done: false,
                detail: Some(format!("{done} / {total} items checked")),
                inner_done: done,
                inner_total: total,
            })
        })?;
    }
    report(&mut on_progress, &mut step, "Items (equipment)", None);

    // Icons are separate from JSON. Skip files already on disk.
    check()?;
    let urls = crate::graphics::collect_from_cache(cache);
    let gfx = cache.graphics_dir();
    propagate_icon_step(crate::graphics::download_missing(
        client,
        &gfx,
        &urls,
        |done, total| {
            on_progress(DownloadProgress {
                current_step: step,
                total_steps: TOTAL_STEPS,
                step_name: "Icons".into(),
                done: false,
                detail: Some(if total == 0 {
                    "up to date".into()
                } else {
                    format!("{done} / {total}")
                }),
                inner_done: done,
                inner_total: total,
            });
        },
    ))?;
    report(&mut on_progress, &mut step, "Icons", None);

    Ok(build)
}

/// Game data plus official name packs (de/es/fr/zh). One bar; skips packs that match `build`
/// unless `mode` is [`RefreshMode::Verify`].
pub fn download_game_and_names(
    client: &Gw2Client,
    cache: &DataCache,
    cancelled: impl Fn() -> bool + Sync,
    mode: RefreshMode,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<u32, ApiError> {
    const NAME_STEPS: usize = 4;
    let total = TOTAL_STEPS + NAME_STEPS;
    let build = download_all(client, cache, &cancelled, mode, |p| {
        on_progress(DownloadProgress {
            current_step: p.current_step,
            total_steps: total,
            step_name: p.step_name,
            done: false,
            detail: p.detail,
            inner_done: p.inner_done,
            inner_total: p.inner_total,
        });
    })?;
    for (i, lang) in crate::localize::API_LANGS.iter().enumerate() {
        if cancelled() {
            client.cancel();
            return Err(ApiError::Cancelled);
        }
        let step = TOTAL_STEPS + i + 1;
        on_progress(DownloadProgress {
            current_step: step,
            total_steps: total,
            step_name: format!("Names ({lang})"),
            done: false,
            detail: None,
            inner_done: 0,
            inner_total: 0,
        });
        let key = crate::localize::cache_key(lang);
        let refresh_pack = match mode {
            RefreshMode::Default => cache.is_stale(&key, build),
            RefreshMode::Verify => true,
        };
        if refresh_pack {
            crate::localize::download(cache, lang, &cancelled, |msg| {
                let (inner_done, inner_total) = parse_items_progress(msg);
                on_progress(DownloadProgress {
                    current_step: step,
                    total_steps: total,
                    step_name: format!("Names ({lang})"),
                    done: false,
                    detail: Some(msg.to_string()),
                    inner_done,
                    inner_total,
                });
            })?;
        }
    }
    on_progress(DownloadProgress {
        current_step: total,
        total_steps: total,
        step_name: "Done".into(),
        done: true,
        detail: None,
        inner_done: 0,
        inner_total: 0,
    });
    Ok(build)
}

fn parse_items_progress(msg: &str) -> (usize, usize) {
    let Some(rest) = msg.strip_prefix("items ") else {
        return (0, 0);
    };
    let mut parts = rest.split('/');
    let done = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let total = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (done, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn temp_cache_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "gw2_download_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn sample_item(id: u32, name: &str, item_type: &str, rarity: &str) -> models::Item {
        models::Item {
            id,
            name: name.to_string(),
            description: None,
            icon: None,
            item_type: item_type.to_string(),
            rarity: rarity.to_string(),
            level: 80,
            vendor_value: None,
            chat_link: None,
            default_skin: None,
            flags: Vec::new(),
            game_types: Vec::new(),
            restrictions: Vec::new(),
            details: None,
        }
    }

    /// A caller that is already cancelled must not reach the network, must not
    /// report a step, and must leave the client cancelled so any wait it is
    /// asked for later aborts instead of sleeping out a retry ladder.
    #[test]
    fn download_all_observes_cancel() {
        let dir = temp_cache_dir("cancel");
        let cache = DataCache::new(&dir);
        let client = Gw2Client::without_key().unwrap();
        let token = Arc::new(AtomicBool::new(true));
        let watched = Arc::clone(&token);

        let mut steps = 0usize;
        let started = Instant::now();
        let err = download_all(
            &client,
            &cache,
            move || watched.load(Ordering::Relaxed),
            RefreshMode::Default,
            |_| steps += 1,
        )
        .expect_err("a cancelled download must not succeed");
        let elapsed = started.elapsed();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(matches!(err, ApiError::Cancelled), "got {err:?}");
        assert_eq!(steps, 0, "no step should have been reported");
        assert!(
            client.is_cancelled(),
            "cancellation must reach the client's own waits"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "cancelled download took {elapsed:?} - it went to the network"
        );
    }

    #[test]
    fn propagate_icon_step_does_not_swallow_cache() {
        assert!(matches!(propagate_icon_step(Ok((1u32, 2u32))), Ok((1, 2))));
        assert!(matches!(
            propagate_icon_step::<()>(Err(ApiError::Cancelled)),
            Err(ApiError::Cancelled)
        ));
        let cache_err = propagate_icon_step::<()>(Err(ApiError::Cache("denied".into())));
        assert!(
            matches!(cache_err, Err(ApiError::Cache(ref m)) if m == "denied"),
            "{cache_err:?}"
        );
        assert!(matches!(
            propagate_icon_step::<()>(Err(ApiError::Internal("x".into()))),
            Err(ApiError::Internal(_))
        ));
    }

    #[test]
    fn download_missing_file_as_graphics_dir_is_cache() {
        let path = temp_cache_dir("gfx_not_dir");
        std::fs::write(&path, b"not-a-directory").unwrap();
        let client = Gw2Client::without_key().unwrap();
        let err = crate::graphics::download_missing(&client, &path, &[], |_, _| {})
            .expect_err("create_dir_all on a file must fail");
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, ApiError::Cache(_)), "got {err:?}");
        assert!(matches!(
            propagate_icon_step::<(u32, u32)>(Err(err)),
            Err(ApiError::Cache(_))
        ));
    }

    #[test]
    fn parse_items_progress_reads_done_and_total() {
        assert_eq!(parse_items_progress("items 40/200"), (40, 200));
        assert_eq!(parse_items_progress("specializations"), (0, 0));
    }

    // --- Ada FOLD3 Kent bars -------------------------------------------------

    #[test]
    fn items_fetch_ids_is_new_union_cached_not_all_live() {
        // live has commons 1..5 plus kept 10,11 and brand-new 99.
        let live = vec![1, 2, 3, 4, 5, 10, 11, 99];
        let previous = vec![1, 2, 3, 4, 5, 10, 11]; // 99 is new
        let cached_keep = vec![10, 11];
        let fetch = items_refresh_fetch_ids(&live, &previous, &cached_keep);
        assert_eq!(fetch, vec![10, 11, 99]);
        assert!(
            !fetch.contains(&1),
            "discarded commons must not be body-fetched"
        );
        assert_eq!(fetch.len(), 3, "not all-ids ({})", live.len());
    }

    #[test]
    fn merge_kept_items_reuses_equal_row_drops_vanished() {
        let old = vec![
            sample_item(10, "Old Equal", "Armor", "Ascended"),
            sample_item(11, "Will Change", "Weapon", "Exotic"),
            sample_item(12, "Vanished", "Trinket", "Legendary"),
        ];
        let fetched = vec![
            sample_item(10, "Old Equal", "Armor", "Ascended"), // equal
            sample_item(11, "Changed", "Weapon", "Exotic"),    // unequal
            sample_item(99, "Brand New", "Armor", "Ascended"), // new kept
            sample_item(50, "Junk", "Consumable", "Basic"),    // filtered out
        ];
        let live = vec![10, 11, 99, 50];
        let (merged, changed) = merge_kept_items(&old, fetched, &live);
        assert!(changed);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].id, 10);
        assert_eq!(merged[0].name, "Old Equal"); // reused equal row
        assert_eq!(merged[1].id, 11);
        assert_eq!(merged[1].name, "Changed");
        assert_eq!(merged[2].id, 99);
        assert!(
            merged.iter().all(|i| i.id != 12),
            "vanished id must be dropped"
        );
        assert!(
            merged.iter().all(|i| i.id != 50),
            "non-keep must not be stored"
        );
    }

    #[test]
    fn keep_filter_keeps_level_80_food_and_utility_only() {
        let consumable = |detail: &str, level: u32| {
            let json = format!(
                r#"{{"id":1,"name":"C","type":"Consumable","rarity":"Fine","level":{level},"details":{{"type":"{detail}","description":"+100 Power"}}}}"#
            );
            serde_json::from_str::<models::Item>(&json).unwrap()
        };
        assert!(item_is_kept(&consumable("Food", 80)));
        assert!(item_is_kept(&consumable("Utility", 80)));
        assert_eq!(
            consumable("Food", 80)
                .details
                .and_then(|d| d.description)
                .as_deref(),
            Some("+100 Power")
        );
        assert!(!item_is_kept(&consumable("Food", 40)));
        assert!(!item_is_kept(&consumable("Booze", 80)));
        assert!(!item_is_kept(&consumable("Generic", 80)));
        assert!(!item_is_kept(&sample_item(
            50,
            "Junk",
            "Consumable",
            "Basic"
        )));
    }

    #[test]
    fn merge_kept_items_all_equal_reports_unchanged() {
        let old = vec![sample_item(10, "Same", "Armor", "Ascended")];
        let fetched = vec![sample_item(10, "Same", "Armor", "Ascended")];
        let (merged, changed) = merge_kept_items(&old, fetched, &[10]);
        assert!(!changed);
        assert_eq!(merged[0].name, "Same");
    }

    #[test]
    fn save_or_stamp_skips_data_rewrite_when_unchanged() {
        let dir = temp_cache_dir("stamp");
        let cache = DataCache::new(&dir);
        let data = vec![sample_item(1, "A", "Armor", "Exotic")];
        cache.save("items", &data, 100).unwrap();
        let path = dir.join("items.json");
        let before = std::fs::read(&path).unwrap();

        save_or_stamp(&cache, "items", &data, 101, false).unwrap();
        assert_eq!(cache.cached_build("items"), Some(101));
        let after = std::fs::read(&path).unwrap();
        // Payload bytes for `data` stay; only build/fetched_at metadata moves.
        // Full file changes, but loading data must match.
        let loaded: Vec<models::Item> = cache.load("items").unwrap().unwrap();
        assert_eq!(loaded[0].name, "A");
        assert_ne!(before, after, "build stamp must update metadata");

        // Same build + unchanged: no disk write.
        let mid = std::fs::read(&path).unwrap();
        save_or_stamp(&cache, "items", &data, 101, false).unwrap();
        let end = std::fs::read(&path).unwrap();
        assert_eq!(mid, end, "same-build stamp must be a no-op write");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_build_default_hits_only_build_endpoint() {
        let dir = temp_cache_dir("same_build");
        let cache = DataCache::new(&dir);
        // Seed every KEPT catalog at build 42 with tiny empty/minimal payloads.
        cache
            .save("itemstats", &Vec::<models::ItemStat>::new(), 42)
            .unwrap();
        cache
            .save("specializations", &Vec::<models::Specialization>::new(), 42)
            .unwrap();
        cache
            .save("traits", &Vec::<models::Trait>::new(), 42)
            .unwrap();
        cache
            .save("skills", &Vec::<models::Skill>::new(), 42)
            .unwrap();
        cache
            .save("professions", &Vec::<models::Profession>::new(), 42)
            .unwrap();
        cache
            .save("legends", &Vec::<models::Legend>::new(), 42)
            .unwrap();
        cache.save("pets", &Vec::<models::Pet>::new(), 42).unwrap();
        cache
            .save("pvp_amulets", &Vec::<models::PvpAmulet>::new(), 42)
            .unwrap();
        cache.save("items", &vec![food_row(5)], 42).unwrap();
        cache.save(ITEMS_IDS_KEY, &Vec::<u32>::new(), 42).unwrap();

        let mut server = mockito::Server::new();
        let build_mock = server
            .mock("GET", "/build")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":42}"#)
            .expect_at_least(1)
            .create();
        // Any catalog probe would 500 / unmatched and fail the run.
        let itemstats = server.mock("GET", "/itemstats").expect(0).create();
        let items = server.mock("GET", "/items").expect(0).create();
        let items_ids = server
            .mock("GET", mockito::Matcher::Regex(r"^/items\?ids=.*".into()))
            .expect(0)
            .create();

        let client = Gw2Client::without_key()
            .unwrap()
            .with_api_root(server.url());
        let build = download_all(&client, &cache, || false, RefreshMode::Default, |_| {})
            .expect("same-build Default must succeed");
        assert_eq!(build, 42);
        build_mock.assert();
        itemstats.assert();
        items.assert();
        items_ids.assert();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_bump_items_fetches_new_union_cached_only() {
        let dir = temp_cache_dir("bump_items");
        let cache = DataCache::new(&dir);
        let kept = sample_item(10, "Kept", "Armor", "Ascended");
        // Id 2 is a food row: this cache is past the consumable backfill.
        cache
            .save("items", &vec![food_row(2), kept.clone()], 100)
            .unwrap();
        cache.save(ITEMS_IDS_KEY, &vec![1u32, 2, 10], 100).unwrap();
        // Other catalogs same-build-skip at 101? No — build bump makes them stale.
        // Seed them at 101 so only items is exercised for body counts... actually
        // needs_catalog_refresh uses is_stale(key, live=101). Seed others at 101.
        for key in [
            "itemstats",
            "specializations",
            "traits",
            "skills",
            "professions",
            "legends",
            "pets",
            "pvp_amulets",
        ] {
            // empty vec as json value — type erased via serde_json
            cache
                .save(key, &Vec::<serde_json::Value>::new(), 101)
                .unwrap();
        }

        let mut server = mockito::Server::new();
        let build_mock = server
            .mock("GET", "/build")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":101}"#)
            .expect_at_least(1)
            .create();
        // live ids: commons 1,2 + kept 10 + new exotic 99 + new junk 3
        let ids_mock = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[1,2,3,10,99]"#)
            .expect_at_least(1)
            .create();
        // fetch set = new{3,99} ∪ cached{2,10} = 2,3,10,99 (live order)
        let bodies = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Regex(r"ids=2,3,10,99".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[{"id":3,"name":"Junk","type":"Consumable","rarity":"Basic","level":1},{"id":10,"name":"Kept","type":"Armor","rarity":"Ascended","level":80},{"id":99,"name":"New Gear","type":"Weapon","rarity":"Exotic","level":80}]"#,
            )
            .expect_at_least(1)
            .create();
        // Must NOT request bulk that starts with discarded id 1 (all-live walk).
        let all_bodies = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Regex(r"^ids=1,".into()))
            .expect(0)
            .create();

        let client = Gw2Client::without_key()
            .unwrap()
            .with_api_root(server.url());
        download_all(&client, &cache, || false, RefreshMode::Default, |_| {})
            .expect("build-bump items refresh");
        build_mock.assert();
        ids_mock.assert();
        bodies.assert();
        all_bodies.assert();

        let loaded: Vec<models::Item> = cache.load("items").unwrap().unwrap();
        let ids: Vec<u32> = loaded.iter().map(|i| i.id).collect();
        assert!(ids.contains(&10));
        assert!(ids.contains(&99));
        assert!(!ids.contains(&3), "non-keep new id must not be stored");
        assert_eq!(cache.cached_build("items"), Some(101));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_mode_refetches_kept_on_same_build() {
        let dir = temp_cache_dir("verify");
        let cache = DataCache::new(&dir);
        let stat = models::ItemStat {
            id: 1,
            name: "Berserker's".into(),
            attributes: vec![],
        };
        cache.save("itemstats", &vec![stat], 42).unwrap();
        for key in [
            "specializations",
            "traits",
            "skills",
            "professions",
            "legends",
            "pets",
            "pvp_amulets",
            "items",
        ] {
            cache
                .save(key, &Vec::<serde_json::Value>::new(), 42)
                .unwrap();
        }
        cache.save(ITEMS_IDS_KEY, &Vec::<u32>::new(), 42).unwrap();

        let mut server = mockito::Server::new();
        server
            .mock("GET", "/build")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":42}"#)
            .create();
        // Verify must probe itemstats even though build matches.
        let ids = server
            .mock("GET", "/itemstats")
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[1]"#)
            .expect_at_least(1)
            .create();
        let body = server
            .mock("GET", "/itemstats")
            .match_query(mockito::Matcher::Regex(r"ids=1".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[{"id":1,"name":"Berserker's","attributes":[]}]"#)
            .expect_at_least(1)
            .create();
        // Remaining catalogs: empty id lists.
        for path in [
            "/specializations",
            "/traits",
            "/skills",
            "/pets",
            "/pvp/amulets",
            "/items",
        ] {
            server
                .mock("GET", path)
                .match_query(mockito::Matcher::Missing)
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body("[]")
                .create();
        }
        server
            .mock("GET", "/professions")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create();
        server
            .mock("GET", "/legends")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create();

        let client = Gw2Client::without_key()
            .unwrap()
            .with_api_root(server.url());
        download_all(&client, &cache, || false, RefreshMode::Verify, |_| {})
            .expect("Verify same-build");
        ids.assert();
        body.assert();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- items.partial first-fill resume (SCHEMA N) ---------------------------

    /// A kept level-80 Food row.
    fn food_row(id: u32) -> models::Item {
        serde_json::from_str(&format!(
            r#"{{"id":{id},"name":"Soup {id}","type":"Consumable","rarity":"Fine","level":80,"details":{{"type":"Food","description":"+100 Power"}}}}"#
        ))
        .unwrap()
    }

    /// Refresh walks the live ids once when the cache holds no Food /
    /// Utility row, and never again once it does.
    #[test]
    fn refresh_backfills_consumables_once() {
        let dir = temp_cache_dir("backfill_once");
        let cache = DataCache::new(&dir);
        seed_kept_except_items(&cache, 42);
        let armor = sample_item(10, "Kept", "Armor", "Ascended");
        cache.save("items", &vec![armor], 42).unwrap();
        cache.save(ITEMS_IDS_KEY, &vec![5u32, 10], 42).unwrap();

        let mut server = mockito::Server::new();
        server
            .mock("GET", "/build")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":42}"#)
            .create();
        let ids = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[5,10]")
            .expect_at_least(1)
            .create();
        let food = serde_json::to_string(&vec![food_row(5)]).unwrap();
        let bodies = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Regex(r"^ids=5$".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(food)
            .expect_at_least(1)
            .create();

        let client = Gw2Client::without_key()
            .unwrap()
            .with_api_root(server.url());
        download_all(&client, &cache, || false, RefreshMode::Default, |_| {})
            .expect("same-build refresh");
        ids.assert();
        bodies.assert();
        ids.remove();
        bodies.remove();
        // Food row present now: the next Refresh touches no items endpoint.
        let again = server
            .mock("GET", mockito::Matcher::Regex(r"^/items".into()))
            .expect(0)
            .create();
        download_all(&client, &cache, || false, RefreshMode::Default, |_| {})
            .expect("second refresh");
        again.assert();
        let stored: Vec<models::Item> = cache.load("items").unwrap().unwrap();
        let stored: Vec<u32> = stored.iter().map(|i| i.id).collect();
        assert_eq!(stored, [10, 5]);
        assert_eq!(cache.cached_build("items"), Some(42));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seed_kept_except_items(cache: &DataCache, build: u32) {
        cache
            .save("itemstats", &Vec::<models::ItemStat>::new(), build)
            .unwrap();
        cache
            .save(
                "specializations",
                &Vec::<models::Specialization>::new(),
                build,
            )
            .unwrap();
        cache
            .save("traits", &Vec::<models::Trait>::new(), build)
            .unwrap();
        cache
            .save("skills", &Vec::<models::Skill>::new(), build)
            .unwrap();
        cache
            .save("professions", &Vec::<models::Profession>::new(), build)
            .unwrap();
        cache
            .save("legends", &Vec::<models::Legend>::new(), build)
            .unwrap();
        cache
            .save("pets", &Vec::<models::Pet>::new(), build)
            .unwrap();
        cache
            .save("pvp_amulets", &Vec::<models::PvpAmulet>::new(), build)
            .unwrap();
    }

    fn item_json(id: u32, keep: bool) -> String {
        if keep {
            format!(
                r#"{{"id":{id},"name":"Gear {id}","type":"Armor","rarity":"Exotic","level":80}}"#
            )
        } else {
            format!(
                r#"{{"id":{id},"name":"Junk {id}","type":"Consumable","rarity":"Basic","level":1}}"#
            )
        }
    }

    fn ids_json(ids: &[u32]) -> String {
        format!(
            "[{}]",
            ids.iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    fn bodies_json(ids: &[u32]) -> String {
        let parts: Vec<String> = ids.iter().map(|&id| item_json(id, id % 50 == 0)).collect();
        format!("[{}]", parts.join(","))
    }

    fn empty_partial() -> ItemsPartial {
        ItemsPartial {
            live_ids: Vec::new(),
            fetched_ids: Vec::new(),
            kept: Vec::new(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn items_install_remaining_skips_already_fetched() {
        let live: Vec<u32> = (1..=250).collect();
        let fetched: Vec<u32> = (1..=200).collect();
        assert_eq!(
            items_install_remaining(&live, &fetched),
            (201..=250).collect::<Vec<u32>>()
        );
        assert_eq!(
            items_install_remaining(&live, &[]),
            live,
            "no partial -> first-fill remaining is all live ids"
        );
    }

    #[test]
    fn items_fill_kind_first_resume_same_build_skip() {
        let dir = temp_cache_dir("fill_kind");
        let cache = DataCache::new(&dir);
        assert_eq!(
            items_fill_kind(&cache, 42, RefreshMode::Default),
            Some(ItemsFillKind::FirstFill)
        );

        cache
            .save(
                ITEMS_PARTIAL_KEY,
                &ItemsPartial {
                    live_ids: vec![1],
                    fetched_ids: vec![1],
                    kept: vec![],
                    skipped: vec![],
                },
                41,
            )
            .unwrap();
        assert_eq!(
            items_fill_kind(&cache, 42, RefreshMode::Default),
            Some(ItemsFillKind::Resume)
        );
        assert!(
            cache.is_stale("items", 42),
            "partial must not make items look warm"
        );
        assert!(!cache.exists("items"));
        assert!(needs_catalog_refresh(
            &cache,
            "items",
            42,
            RefreshMode::Default
        ));

        cache
            .save("items", &Vec::<models::Item>::new(), 42)
            .unwrap();
        assert_eq!(
            items_fill_kind(&cache, 42, RefreshMode::Default),
            Some(ItemsFillKind::SameBuildSkip)
        );
        assert_eq!(items_fill_kind(&cache, 43, RefreshMode::Default), None);
        assert_eq!(items_fill_kind(&cache, 42, RefreshMode::Verify), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_partial_does_not_publish_items_keys() {
        let dir = temp_cache_dir("persist_partial_only");
        let cache = DataCache::new(&dir);
        let mut partial = empty_partial();
        partial.live_ids = vec![1, 2];
        partial.fetched_ids = vec![1];
        persist_items_partial(&cache, &partial, 7).unwrap();
        assert!(cache.exists(ITEMS_PARTIAL_KEY));
        assert!(!cache.exists("items"));
        assert!(!cache.exists(ITEMS_IDS_KEY));
        assert!(!cache.exists("items.skipped"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_fill_writes_partial_per_batch_then_completes() {
        let dir = temp_cache_dir("first_fill_partial");
        let cache = DataCache::new(&dir);
        seed_kept_except_items(&cache, 42);

        let live: Vec<u32> = (1..=250).collect();
        let batch1: Vec<u32> = (1..=200).collect();
        let batch2: Vec<u32> = (201..=250).collect();

        let mut server = mockito::Server::new();
        let build_mock = server
            .mock("GET", "/build")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":42}"#)
            .expect_at_least(1)
            .create();
        let ids_mock = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ids_json(&live))
            .expect_at_least(1)
            .create();
        let b1 = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Regex(r"^ids=1,2,3,".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(bodies_json(&batch1))
            .expect_at_least(1)
            .create();
        let b2 = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Regex(r"^ids=201,202,".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(bodies_json(&batch2))
            .expect_at_least(1)
            .create();

        let client = Gw2Client::without_key()
            .unwrap()
            .with_api_root(server.url());
        let mut saw_partial_mid = false;
        download_all(
            &client,
            &cache,
            || false,
            RefreshMode::Default,
            |p| {
                if p.step_name.starts_with("Items")
                    && p.inner_total == 250
                    && p.inner_done > 0
                    && p.inner_done < 250
                {
                    assert!(
                        cache.exists(ITEMS_PARTIAL_KEY),
                        "partial must land after a completed 200-id batch"
                    );
                    assert!(
                        !cache.exists("items"),
                        "must not stamp items until install done"
                    );
                    assert!(!cache.exists(ITEMS_IDS_KEY));
                    assert!(!cache.exists("items.skipped"));
                    let part: ItemsPartial = cache.load(ITEMS_PARTIAL_KEY).unwrap().unwrap();
                    assert!(!part.fetched_ids.is_empty());
                    assert!(part.fetched_ids.iter().all(|id| live.contains(id)));
                    saw_partial_mid = true;
                }
            },
        )
        .expect("first-fill");

        assert!(saw_partial_mid, "expected a mid-install partial commit");
        assert!(cache.exists("items"));
        assert!(cache.exists(ITEMS_IDS_KEY));
        assert!(cache.exists("items.skipped"));
        assert!(
            !cache.exists(ITEMS_PARTIAL_KEY),
            "complete must delete items.partial"
        );
        let stored: Vec<models::Item> = cache.load("items").unwrap().unwrap();
        assert!(stored.iter().all(item_is_kept), "keep-set must not widen");
        let stored_ids: Vec<u32> = cache.load(ITEMS_IDS_KEY).unwrap().unwrap();
        assert_eq!(stored_ids, live);
        assert_eq!(cache.cached_build("items"), Some(42));
        build_mock.assert();
        ids_mock.assert();
        b1.assert();
        b2.assert();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_skips_already_fetched_ids() {
        let dir = temp_cache_dir("resume_partial");
        let cache = DataCache::new(&dir);
        seed_kept_except_items(&cache, 42);

        let live: Vec<u32> = (1..=250).collect();
        let fetched: Vec<u32> = (1..=200).collect();
        let kept = vec![sample_item(50, "Gear 50", "Armor", "Exotic")];
        cache
            .save(
                ITEMS_PARTIAL_KEY,
                &ItemsPartial {
                    live_ids: live.clone(),
                    fetched_ids: fetched,
                    kept,
                    skipped: vec![],
                },
                41,
            )
            .unwrap();
        assert!(!cache.exists("items"));
        assert_eq!(
            items_fill_kind(&cache, 42, RefreshMode::Default),
            Some(ItemsFillKind::Resume)
        );

        let batch2: Vec<u32> = (201..=250).collect();
        let mut server = mockito::Server::new();
        let build_mock = server
            .mock("GET", "/build")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":42}"#)
            .expect_at_least(1)
            .create();
        let ids_mock = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ids_json(&live))
            .expect_at_least(1)
            .create();
        let already = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Regex(r"^ids=1,".into()))
            .expect(0)
            .create();
        let rest = server
            .mock("GET", "/items")
            .match_query(mockito::Matcher::Regex(r"^ids=201,202,".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(bodies_json(&batch2))
            .expect_at_least(1)
            .create();

        let client = Gw2Client::without_key()
            .unwrap()
            .with_api_root(server.url());
        download_all(&client, &cache, || false, RefreshMode::Default, |_| {})
            .expect("resume install");

        already.assert();
        rest.assert();
        build_mock.assert();
        ids_mock.assert();
        assert!(
            !cache.exists(ITEMS_PARTIAL_KEY),
            "complete clears items.partial"
        );
        let stored: Vec<models::Item> = cache.load("items").unwrap().unwrap();
        assert!(
            stored.iter().any(|i| i.id == 50),
            "kept from the first batch must survive resume"
        );
        assert!(
            stored.iter().any(|i| i.id == 250),
            "kept from the resumed batch must be installed"
        );
        assert!(
            stored.iter().all(|i| i.id != 1),
            "discarded ids must not enter the keep-set"
        );
        let stored_ids: Vec<u32> = cache.load(ITEMS_IDS_KEY).unwrap().unwrap();
        assert_eq!(stored_ids, live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn warm_exists_items_same_build_skips_even_with_partial() {
        let dir = temp_cache_dir("warm_partial_ignored");
        let cache = DataCache::new(&dir);
        seed_kept_except_items(&cache, 42);
        cache.save("items", &vec![food_row(5)], 42).unwrap();
        cache.save(ITEMS_IDS_KEY, &Vec::<u32>::new(), 42).unwrap();
        cache
            .save(
                ITEMS_PARTIAL_KEY,
                &ItemsPartial {
                    live_ids: vec![1],
                    fetched_ids: vec![],
                    kept: vec![],
                    skipped: vec![],
                },
                41,
            )
            .unwrap();

        let mut server = mockito::Server::new();
        let build_mock = server
            .mock("GET", "/build")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":42}"#)
            .expect_at_least(1)
            .create();
        let items = server.mock("GET", "/items").expect(0).create();
        let items_ids = server
            .mock("GET", mockito::Matcher::Regex(r"^/items\?ids=.*".into()))
            .expect(0)
            .create();

        let client = Gw2Client::without_key()
            .unwrap()
            .with_api_root(server.url());
        let build = download_all(&client, &cache, || false, RefreshMode::Default, |_| {})
            .expect("same-build Default must still FOLD3-skip");
        assert_eq!(build, 42);
        build_mock.assert();
        items.assert();
        items_ids.assert();
        assert!(
            cache.exists(ITEMS_PARTIAL_KEY),
            "same-build skip must not rewrite leftover partial"
        );
        assert_eq!(
            items_fill_kind(&cache, 42, RefreshMode::Default),
            Some(ItemsFillKind::SameBuildSkip)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live smoke test for the whole download path. Lives here rather than in
    /// `tests/` for the same reason the `test_live_fetch_*` tests live in
    /// `client.rs`: this crate keeps its `#[ignore]` network tests beside the
    /// code they exercise, and a second test target costs a full extra link
    /// for tests that never run by default.
    ///
    /// Run with:
    /// `cargo test -p gw2-api test_full_download_pipeline -- --ignored --nocapture`
    #[test]
    #[ignore] // Requires network
    fn test_full_download_pipeline() {
        let client = Gw2Client::without_key().unwrap();

        let build = client.get_build_number().unwrap();
        println!("[OK] Build: {}", build);

        let start = Instant::now();
        let traits: Vec<models::Trait> = client.fetch_all("traits").unwrap();
        println!(
            "[OK] Traits: {} in {:.1}s",
            traits.len(),
            start.elapsed().as_secs_f64()
        );
        assert!(
            traits.len() > 100,
            "Expected >100 traits, got {}",
            traits.len()
        );

        let start = Instant::now();
        let skills: Vec<models::Skill> = client.fetch_all("skills").unwrap();
        println!(
            "[OK] Skills: {} in {:.1}s",
            skills.len(),
            start.elapsed().as_secs_f64()
        );
        assert!(
            skills.len() > 100,
            "Expected >100 skills, got {}",
            skills.len()
        );

        let start = Instant::now();
        let specs: Vec<models::Specialization> = client.fetch_all("specializations").unwrap();
        println!(
            "[OK] Specs: {} in {:.1}s",
            specs.len(),
            start.elapsed().as_secs_f64()
        );
        assert!(specs.len() > 30, "Expected >30 specs, got {}", specs.len());

        let start = Instant::now();
        let itemstats: Vec<models::ItemStat> = client.fetch_all("itemstats").unwrap();
        println!(
            "[OK] Itemstats: {} in {:.1}s",
            itemstats.len(),
            start.elapsed().as_secs_f64()
        );
        assert!(
            itemstats.len() > 50,
            "Expected >50 itemstats, got {}",
            itemstats.len()
        );

        // Full item download is too slow for this test; first 2000 IDs only.
        let start = Instant::now();
        let all_ids: Vec<serde_json::Value> = client.get("items").unwrap();
        println!(
            "[OK] Item IDs: {} in {:.1}s",
            all_ids.len(),
            start.elapsed().as_secs_f64()
        );

        let subset = &all_ids[..2000.min(all_ids.len())];
        let start = Instant::now();
        let items: Vec<serde_json::Value> = client.fetch_by_ids("items", subset).unwrap();
        println!(
            "[OK] Items (2000 subset): {} in {:.1}s",
            items.len(),
            start.elapsed().as_secs_f64()
        );
        assert!(
            items.len() > 1000,
            "Expected >1000 items from 2000 IDs, got {}",
            items.len()
        );

        let start = Instant::now();
        let profs: Vec<models::Profession> = client
            .get_with_params(
                "professions",
                &[("ids", "all"), ("v", "2019-12-19T00:00:00.000Z")],
            )
            .unwrap();
        println!(
            "[OK] Professions: {} in {:.1}s",
            profs.len(),
            start.elapsed().as_secs_f64()
        );
        assert_eq!(
            profs.len(),
            9,
            "Expected 9 professions, got {}",
            profs.len()
        );

        println!("\n=== ALL ENDPOINTS OK ===");
    }
}
