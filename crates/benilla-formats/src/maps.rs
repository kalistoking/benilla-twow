//! Map.dbc loader: resolves a server-side `mapId` to its **MPQ directory name** (`0` →
//! `"Azeroth"`, `1` → `"Kalimdor"`, `36` → `"DeadminesInstance"`, …) so the cross-map teleport
//! handler can call `MapTiles::load(chain, dir)` for the new world.
//!
//! Layout — VERIFIED against build 5875 (`xxd` on extracted `DBFilesClient\Map.dbc`,
//! 2026-05-29; field 38 confirmed 2026-06-02): the WDBC header reports **44 records · 42 fields
//! · 168 B/record**. Load-bearing for us: `ID` (0), `Directory` (1), and **`LoadingScreenID`
//! (38)** — an FK into `LoadingScreens.dbc` (see [`crate::LoadingScreenCatalog`]) selecting the
//! map's full-screen load art. Verified empirically: field 38 ∈ the LoadingScreens id-set for 39
//! of 44 maps (the 5 zeros are dev/test maps), with exact known pairs (mapId 0 `Azeroth` → 4,
//! mapId 1 `Kalimdor` → 3, `DeadminesInstance` → 142). The remaining dwords are `MapName_loc`
//! (9), `MapDescription0_loc` (9), `MapDescription1_loc` (9), plus a few ints we don't need.
//! Reading them as `UInt32` placeholders is fine — DBC fields are 4 bytes regardless of declared
//! type, so the schema only has to add up to 42 × 4 = 168.

use std::collections::HashMap;

use crate::Chain;
use anyhow::{Context, Result};
use benilla_dbc::{FieldType, Schema, SchemaField};

use crate::dbc::{f32_at, parse, str_at, u32_at};

const MAP: &str = "DBFilesClient\\Map.dbc";

/// Field index of the `LoadingScreenID` FK (see module docs). 0 means "no art" (dev/test maps).
const LOADING_SCREEN_FIELD: usize = 38;

/// Resolved per-map data, built once at startup off the client's Map.dbc.
pub struct MapCatalog {
    dirs: HashMap<u32, String>,
    /// `mapId → MapName_Lang[enUS]` (field 4, `+0x10` — the offset the binary's map-name reader
    /// `0x4a65a0` uses against this same patch-2 layout). The world map's continent dropdown
    /// displays THIS ("Eastern Kingdoms"), not the WorldMapArea art-folder string ("Azeroth") —
    /// wow-re Q3 verdict, 2026-07-07.
    names: HashMap<u32, String>,
    /// `mapId → LoadingScreenID` FK into `LoadingScreens.dbc` (only maps with a non-zero FK).
    loading_screens: HashMap<u32, u32>,
    /// `mapId → InstanceType` (field 2, `+0x8` — the offset the binary reads at `0x48a772`,
    /// `0x495cb9` and `0x495d33`). VERIFIED by dumping the shipped patch-2 `Map.dbc`, 2026-08-30:
    /// **0** none (Azeroth, Kalimdor, Deeprun Tram), **1** party dungeon (Deadmines, Scholomance),
    /// **2** raid (Molten Core, Naxxramas), **3** battleground (Alterac Valley, Warsong Gulch) —
    /// the same four the client's own `IsInInstance` string table spells `none`/`party`/`raid`/
    /// `pvp` (`0x83de58`, indexed by this value with a `< 4` guard).
    instance_types: HashMap<u32, u32>,
    /// `mapId → ` the columns the battleground list and queue verbs read off the row
    /// (see [`MapBattlegroundColumns`]) — every row, because the client resolves map 0 too.
    battleground: HashMap<u32, MapBattlegroundColumns>,
    /// Ids [`restore_shadowed_rows`] took from a shadowed copy, in the order restored.
    restored: Vec<u32>,
}

/// The Map.dbc columns the client's battleground family reads by row offset (wow-re
/// `battlefield-verb-family.md` §3.4, §3.6, §4.1, §5.2; decision 1974). Offsets are into the
/// 168-byte record with the id at `+0x00`, so `+0x4·k` is field `k`. VERIFIED by dumping the
/// shipped patch-2 `Map.dbc` (2026-09-04): Warsong Gulch `10, 60, 10, −1, (0, 0), span 10, group 1`;
/// Arathi Basin `20, 60, 15, …, span 10, group 1`; Alterac Valley `51, 60, 40, −1, (0.74, 0.34),
/// span 0, group 0`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct MapBattlegroundColumns {
    /// Field 13 (`+0x34`) — the bracket base: the list and status handlers compute a bracket's
    /// floor as `bracket · span + min_level` (§4.1). `GetBattlefieldInfo`'s third value.
    pub min_level: u32,
    /// Field 14 (`+0x38`) — `GetBattlefieldInfo`'s fourth value.
    pub max_level: u32,
    /// Field 15 (`+0x3c`) — the size a group join is checked against (`0x4a9f60`: both the party
    /// count and the raid count must be `<=` this, else message 442 and nothing sent).
    pub max_players: u32,
    /// Field 16 (`+0x40`, signed) — `GetBattlefieldInfo`'s fifth value (`−1` on every shipped row).
    pub field_16: i32,
    /// Fields 17–18 (`+0x44`/`+0x48`, f32) — `GetBattlefieldInfo`'s sixth and seventh values.
    pub field_17: f32,
    pub field_18: f32,
    /// Fields 20 and 29 (`+0x50`, `+0x74`) — the two localized descriptions, indexed by the
    /// client's faction-group index: `0` for a FactionTemplate mask with bit `0x4`, `1` for bit
    /// `0x2` (§3.4). The shipped rows carry the SAME text in both, so nothing observable rides on
    /// which side is which.
    pub descriptions: [String; 2],
    /// Field 39 (`+0x9c`) — the level-bracket span; `0` means one bracket and zeroed bounds.
    pub bracket_span: u32,
    /// Field 40 (`+0xa0`) — non-zero when the battleground can be queued as a group
    /// (`CanJoinBattlefieldAsGroup`, §3.6).
    pub group_queue: u32,
    /// Field 41 (`+0xa4`, f32, the record's last) — `MinimapIconScale`, what
    /// `GetBattlefieldMapIconScale()` answers for the active queue slot's map (wow-re
    /// `worldmap-arrow-and-positions.md` §3.6; 1980). Shipped: `1.25` for Arathi Basin, `1.0`
    /// for every other row read.
    pub minimap_icon_scale: f32,
}

impl MapBattlegroundColumns {
    /// The bracket's `(min, max)` for a wire bracket index (§4.1/§4.2): with a positive span,
    /// `min = bracket · span + min_level` and `max = min(span + min − 1, 60)`; else both zero.
    pub fn bracket_levels(&self, bracket: u8) -> (u32, u32) {
        if self.bracket_span == 0 {
            return (0, 0);
        }
        let min = u32::from(bracket) * self.bracket_span + self.min_level;
        (min, (self.bracket_span + min).saturating_sub(1).min(60))
    }
}

impl MapCatalog {
    /// MPQ directory name (the `Directory` column) for `map_id`, or `None` if the DBC has no
    /// such row. Use with `MapTiles::load(chain, dir)`.
    pub fn directory(&self, map_id: u32) -> Option<&str> {
        self.dirs.get(&map_id).map(String::as_str)
    }

    /// The localized display name (`MapName_Lang`, enUS) for `map_id` — "Eastern Kingdoms",
    /// "Kalimdor", "The Deadmines", …
    pub fn name(&self, map_id: u32) -> Option<&str> {
        self.names.get(&map_id).map(String::as_str)
    }

    /// `LoadingScreenID` for `map_id` — the FK to resolve against [`crate::LoadingScreenCatalog`]
    /// for the load-art BLP. `None` for dev/test maps that carry no screen. This is the same
    /// mechanism for *every* map kind (open world, instance, battleground) — only the art row
    /// differs.
    pub fn loading_screen_id(&self, map_id: u32) -> Option<u32> {
        self.loading_screens.get(&map_id).copied()
    }

    /// `InstanceType` for `map_id` (see [`MapCatalog::instance_types`]), or `None` for a map id
    /// the DBC has no row for. The client treats a missing row as "not an instance" everywhere it
    /// asks — it null-checks the record pointer first and takes the same branch as type 0.
    pub fn instance_type(&self, map_id: u32) -> Option<u32> {
        self.instance_types.get(&map_id).copied()
    }

    /// Whether `map_id` is a **party dungeon** (`InstanceType == 1`). This exact predicate — not
    /// "is an instance" — is the one the reference's lockout bookkeeping runs on: `cmp [rec+8],1`
    /// gates both what `SMSG_UPDATE_LAST_INSTANCE` records and both halves of
    /// `CanShowResetInstances` (decision 1748).
    pub fn is_party_dungeon(&self, map_id: u32) -> bool {
        self.instance_type(map_id) == Some(1)
    }

    /// The battleground family's columns for `map_id` — every row has them (the client reads map
    /// 0's when nothing was ever listed), `None` only for an id with no row.
    pub fn battleground(&self, map_id: u32) -> Option<&MapBattlegroundColumns> {
        self.battleground.get(&map_id)
    }

    /// The map ids whose row came from a shadowed Map.dbc copy (see [`restore_shadowed_rows`]).
    pub fn restored(&self) -> &[u32] {
        &self.restored
    }

    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }
}

/// Field index of the enUS `MapName_Lang` string (`+0x10`, see [`MapCatalog::names`]).
const MAP_NAME_FIELD: usize = 4;

/// Field index of `InstanceType` (`+0x8`, see [`MapCatalog::instance_types`]).
const INSTANCE_TYPE_FIELD: usize = 2;

/// The battleground family's columns (see [`MapBattlegroundColumns`]).
const MIN_LEVEL_FIELD: usize = 13;
const MAX_LEVEL_FIELD: usize = 14;
const MAX_PLAYERS_FIELD: usize = 15;
const FIELD_16: usize = 16;
const FIELD_17: usize = 17;
const FIELD_18: usize = 18;
const DESCRIPTION_0_FIELD: usize = 20;
const DESCRIPTION_1_FIELD: usize = 29;
const BRACKET_SPAN_FIELD: usize = 39;
const GROUP_QUEUE_FIELD: usize = 40;
const MINIMAP_ICON_SCALE_FIELD: usize = 41;

/// 42 fields total; field 0 = ID, field 1 = Directory string, field 2 = InstanceType,
/// field 4 = MapName (enUS), field 38 = LoadingScreenID (FK), plus the battleground columns
/// ([`MapBattlegroundColumns`]). Remaining fields are placeholders (4-byte dwords; the schema only
/// needs to total 168 B).
fn map_schema() -> Schema {
    let mut s = Schema::new("Map");
    s.add_field(SchemaField::new("ID", FieldType::UInt32));
    s.add_field(SchemaField::new("Directory", FieldType::String));
    for i in 2..42 {
        match i {
            INSTANCE_TYPE_FIELD => s.add_field(SchemaField::new("InstanceType", FieldType::UInt32)),
            MAP_NAME_FIELD => s.add_field(SchemaField::new("MapName", FieldType::String)),
            DESCRIPTION_0_FIELD => {
                s.add_field(SchemaField::new("MapDescription0", FieldType::String))
            }
            DESCRIPTION_1_FIELD => {
                s.add_field(SchemaField::new("MapDescription1", FieldType::String))
            }
            FIELD_17 | FIELD_18 | MINIMAP_ICON_SCALE_FIELD => {
                s.add_field(SchemaField::new(format!("_f{i}"), FieldType::Float32))
            }
            LOADING_SCREEN_FIELD => {
                s.add_field(SchemaField::new("LoadingScreenID", FieldType::UInt32))
            }
            _ => s.add_field(SchemaField::new(format!("_pad{i}"), FieldType::UInt32)),
        }
    }
    s
}

/// Read Map.dbc off the patch chain into a [`MapCatalog`], then [`restore_shadowed_rows`] from
/// the copies the winning file shadows.
pub fn load_map_catalog(chain: &mut Chain) -> Result<MapCatalog> {
    let bytes = chain
        .read_file(MAP)
        .with_context(|| format!("reading {MAP}"))?;
    let mut catalog = parse_map_dbc(&bytes, |_, _| true)?;
    let shadowed: Vec<Vec<u8>> = chain
        .read_each(MAP)
        .into_iter()
        .skip(1)
        .map(|c| c.1)
        .collect();
    restore_shadowed_rows(&mut catalog, &shadowed, |dir| {
        chain.contains(&format!("World\\Maps\\{dir}\\{dir}.wdt"))
    });
    Ok(catalog)
}

/// **Turtle (B115).** A DBC overrides whole-file, so a later patch that drops a row drops the
/// map: Turtle's `patch-3` Map.dbc has `45 ScarletCitadel`, `patch-4`..`patch-9` do not, yet
/// `World\Maps\ScarletCitadel` still ships in `patch-3` — and a restoration server sends map 45.
/// For an id the winning file has **no row** for, take the highest-priority shadowed copy's row,
/// but only when its world exists (`has_wdt(dir)`): an id with a row is never touched, and a row
/// whose WDT is gone stays absent (the terrain streamer's vacuity path keeps covering it).
/// `shadowed` is highest priority first; a copy our 42-field schema cannot read is skipped.
pub(crate) fn restore_shadowed_rows(
    catalog: &mut MapCatalog,
    shadowed: &[Vec<u8>],
    has_wdt: impl Fn(&str) -> bool,
) {
    for bytes in shadowed {
        let present: Vec<u32> = catalog.battleground.keys().copied().collect();
        let Ok(old) = parse_map_dbc(bytes, |id, dir| !present.contains(&id) && has_wdt(dir)) else {
            continue;
        };
        let mut ids: Vec<u32> = old.dirs.keys().copied().collect();
        ids.sort_unstable();
        catalog.restored.extend(ids);
        catalog.dirs.extend(old.dirs);
        catalog.names.extend(old.names);
        catalog.loading_screens.extend(old.loading_screens);
        catalog.instance_types.extend(old.instance_types);
        catalog.battleground.extend(old.battleground);
    }
}

/// Parse one Map.dbc copy, keeping the rows `keep(id, directory)` accepts.
fn parse_map_dbc(bytes: &[u8], keep: impl Fn(u32, &str) -> bool) -> Result<MapCatalog> {
    let rs = parse(bytes, map_schema(), "Map")?;
    let mut dirs = HashMap::with_capacity(rs.records().len());
    let mut names = HashMap::with_capacity(rs.records().len());
    let mut loading_screens = HashMap::new();
    let mut instance_types = HashMap::with_capacity(rs.records().len());
    let mut battleground = HashMap::with_capacity(rs.records().len());
    for r in rs.records() {
        let Some(id) = u32_at(r, 0) else { continue };
        if !keep(id, &str_at(&rs, r, 1).unwrap_or_default()) {
            continue;
        }
        battleground.insert(
            id,
            MapBattlegroundColumns {
                min_level: u32_at(r, MIN_LEVEL_FIELD).unwrap_or(0),
                max_level: u32_at(r, MAX_LEVEL_FIELD).unwrap_or(0),
                max_players: u32_at(r, MAX_PLAYERS_FIELD).unwrap_or(0),
                field_16: u32_at(r, FIELD_16).unwrap_or(0) as i32,
                field_17: f32_at(r, FIELD_17).unwrap_or(0.0),
                field_18: f32_at(r, FIELD_18).unwrap_or(0.0),
                descriptions: [
                    str_at(&rs, r, DESCRIPTION_0_FIELD).unwrap_or_default(),
                    str_at(&rs, r, DESCRIPTION_1_FIELD).unwrap_or_default(),
                ],
                bracket_span: u32_at(r, BRACKET_SPAN_FIELD).unwrap_or(0),
                group_queue: u32_at(r, GROUP_QUEUE_FIELD).unwrap_or(0),
                minimap_icon_scale: f32_at(r, MINIMAP_ICON_SCALE_FIELD).unwrap_or(1.0),
            },
        );
        if let Some(dir) = str_at(&rs, r, 1) {
            dirs.insert(id, dir);
        }
        if let Some(name) = str_at(&rs, r, MAP_NAME_FIELD).filter(|n| !n.is_empty()) {
            names.insert(id, name);
        }
        // Every row's type is recorded, 0 included: "0" and "no such map" are different answers
        // to `instance_type`, and the second is what a bad map id must give.
        if let Some(ty) = u32_at(r, INSTANCE_TYPE_FIELD) {
            instance_types.insert(id, ty);
        }
        // 0 = "no loading screen" (dev/test maps); only record real FKs.
        if let Some(ls) = u32_at(r, LOADING_SCREEN_FIELD).filter(|&v| v != 0) {
            loading_screens.insert(id, ls);
        }
    }
    Ok(MapCatalog {
        dirs,
        names,
        loading_screens,
        instance_types,
        battleground,
        restored: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 42-field / 168-byte Map.dbc holding `(id, directory, loading screen)` rows.
    fn map_dbc(rows: &[(u32, &str, u32)]) -> Vec<u8> {
        let mut strings = vec![0u8];
        let mut recs = Vec::new();
        for &(id, dir, ls) in rows {
            let mut f = [0u32; 42];
            f[0] = id;
            f[1] = strings.len() as u32;
            f[LOADING_SCREEN_FIELD] = ls;
            strings.extend_from_slice(dir.as_bytes());
            strings.push(0);
            recs.extend(f.iter().flat_map(|v| v.to_le_bytes()));
        }
        let mut out = b"WDBC".to_vec();
        for v in [rows.len() as u32, 42, 168, strings.len() as u32] {
            out.extend(v.to_le_bytes());
        }
        out.extend(recs);
        out.extend(strings);
        out
    }

    /// B115: Turtle's patch-3 Map.dbc has `45 ScarletCitadel` (its WDT ships in patch-3), the
    /// winning patch-9 copy has no row 45 — the id comes back from the shadowed copy. A row the
    /// winner has is never replaced, a shadowed row without a WDT stays absent, and the higher
    /// of two shadowed copies wins.
    #[test]
    fn a_row_gone_from_the_winning_map_dbc_comes_back_when_its_world_exists() {
        let winner = map_dbc(&[(0, "Azeroth", 4), (36, "DeadminesInstance", 142)]);
        let mut catalog = parse_map_dbc(&winner, |_, _| true).expect("winner");
        let patch8 = map_dbc(&[(36, "OldDeadmines", 1), (44, "Monastery", 190)]);
        let patch3 = map_dbc(&[
            (44, "OlderMonastery", 1),
            (45, "ScarletCitadel", 509),
            (803, "Northrend", 513),
        ]);
        let worlds = [
            "Monastery",
            "OlderMonastery",
            "ScarletCitadel",
            "OldDeadmines",
        ];
        restore_shadowed_rows(&mut catalog, &[patch8, patch3], |d| worlds.contains(&d));
        assert_eq!(catalog.directory(45), Some("ScarletCitadel"));
        assert_eq!(catalog.loading_screen_id(45), Some(509));
        assert_eq!(
            catalog.directory(36),
            Some("DeadminesInstance"),
            "the winner's row stays"
        );
        assert_eq!(
            catalog.directory(44),
            Some("Monastery"),
            "the higher shadowed copy wins"
        );
        assert_eq!(catalog.directory(803), None, "no WDT, no row");
        assert_eq!(catalog.restored(), &[44, 45]);
        assert_eq!(catalog.len(), 4);
    }

    /// B115 on the real chain: an install that ships `World\Maps\ScarletCitadel` (Turtle 1.18.1,
    /// patch-3) resolves map 45 to it with its loading screen. Skips on an install without it.
    #[test]
    fn map_45_resolves_to_scarlet_citadel_on_a_turtle_install() {
        let data = crate::wow_data_or_skip!();
        let mut chain = crate::open_chain(&data).expect("open chain");
        if !chain.contains("World\\Maps\\ScarletCitadel\\ScarletCitadel.wdt") {
            return;
        }
        let catalog = load_map_catalog(&mut chain).expect("Map.dbc");
        assert_eq!(catalog.directory(45), Some("ScarletCitadel"));
        assert_eq!(catalog.loading_screen_id(45), Some(509));
        assert!(catalog.restored().contains(&45), "{:?}", catalog.restored());
    }

    /// The battleground columns off the shipped patch-2 `Map.dbc`, and the bracket arithmetic the
    /// list and status handlers run on them (wow-re `battlefield-verb-family.md` §4.1).
    #[test]
    fn the_battleground_columns_read_the_shipped_rows() {
        let data = crate::wow_data_or_skip!();
        let mut chain = crate::open_chain(&data).expect("open chain");
        let catalog = load_map_catalog(&mut chain).expect("Map.dbc");
        let wsg = catalog.battleground(489).expect("Warsong Gulch");
        assert_eq!(
            (wsg.min_level, wsg.max_level, wsg.max_players, wsg.field_16),
            (10, 60, 10, -1)
        );
        assert_eq!((wsg.bracket_span, wsg.group_queue), (10, 1));
        assert!(wsg.descriptions[0].starts_with("A valley bordering Ashenvale"));
        assert_eq!(
            wsg.descriptions[0], wsg.descriptions[1],
            "both faction descriptions carry the same text on the shipped rows"
        );
        assert_eq!(wsg.bracket_levels(0), (10, 19));
        assert_eq!(wsg.bracket_levels(5), (60, 60), "the max clamps at 60");
        let ab = catalog.battleground(529).expect("Arathi Basin");
        assert_eq!(
            (ab.min_level, ab.max_players, ab.bracket_span),
            (20, 15, 10)
        );
        assert!(
            (ab.minimap_icon_scale - 1.25).abs() < 1e-6,
            "Arathi Basin\'s MinimapIconScale"
        );
        assert!((wsg.minimap_icon_scale - 1.0).abs() < 1e-6);
        assert_eq!(ab.bracket_levels(1), (30, 39));
        let av = catalog.battleground(30).expect("Alterac Valley");
        assert_eq!((av.min_level, av.max_players, av.group_queue), (51, 40, 0));
        assert_eq!(
            av.bracket_levels(0),
            (0, 0),
            "a zero span zeroes both bounds"
        );
        assert!((av.field_17 - 0.74).abs() < 1e-3 && (av.field_18 - 0.34).abs() < 1e-3);
        assert!(
            catalog.battleground(0).is_some(),
            "every row carries the columns — the client resolves map 0's when nothing was listed"
        );
    }
}
