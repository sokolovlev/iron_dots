//! Balance parameters (`assets/balance.ron`, spec 2.8) and the integer
//! per-tick rules derived from them.

use crate::fx::{Fx, Milli, ONE};
use crate::map::Terrain;
use crate::unit::UnitKind;
use serde::{Deserialize, Serialize};

/// The default balance shipped with the game.
pub const DEFAULT_BALANCE_RON: &str = include_str!("../../../assets/balance.ron");

/// HP and morale are stored as `u16` in hundredths: 100.00 HP = 10 000.
pub const HP_SCALE: i64 = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Balance {
    pub tick_rate: u32,
    pub unit: UnitParams,
    pub combat: CombatParams,
    pub morale: MoraleParams,
    pub economy: EconomyParams,
    pub supply: SupplyParams,
    pub territory: TerritoryParams,
    pub city: CityParams,
    pub match_rules: MatchParams,
    pub water: WaterParams,
    pub terrain: TerrainTable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitParams {
    pub max_hp: u32,
    pub separation_radius: Milli,
    pub engaged_speed: Milli,
    pub light: UnitTypeParams,
    pub heavy: UnitTypeParams,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitTypeParams {
    pub speed: Milli,
    pub atk: Milli,
    pub def: Milli,
    pub supply: u32,
    pub cost: u32,
    pub upkeep: Milli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombatParams {
    pub contact_radius: Milli,
    pub base_dps: Milli,
    pub attacker_taken: Milli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoraleParams {
    pub max: u32,
    pub loss_attack: Milli,
    pub loss_defend: Milli,
    pub regen: Milli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EconomyParams {
    pub city_income: Milli,
    pub start_money: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyParams {
    pub per_city: u32,
    pub starve_dps: Milli,
    pub broke_starve_dps: Milli,
    pub heal_dps: Milli,
    pub heal_city_dps: Milli,
    pub heal_delay: Milli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerritoryParams {
    /// Control radius of a unit, tiles.
    pub radius: u32,
    /// Control radius of a city around itself, tiles.
    pub city_radius: u32,
    /// Rear: land within this many tiles of two or more cities of one
    /// team, and of no city of another team, is that team's.
    pub rear_radius: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CityParams {
    pub capture_time: Milli,
    pub defense_bonus: Milli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchParams {
    pub start_light_units: u32,
    pub win_city_share: Milli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaterParams {
    pub damage_dps: Milli,
    pub ship_after: Milli,
}

/// `(speed, atk)` multipliers for light and heavy units plus a defense bonus.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainRow {
    pub light: (Milli, Milli),
    pub heavy: (Milli, Milli),
    pub defense: Milli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainTable {
    pub plain: TerrainRow,
    pub forest: TerrainRow,
    pub hills: TerrainRow,
    pub sand: TerrainRow,
    pub snow: TerrainRow,
    pub mud: TerrainRow,
    pub water: TerrainRow,
    pub ship: TerrainRow,
}

impl Balance {
    pub fn from_ron(text: &str) -> Result<Balance, String> {
        let b: Balance = ron::from_str(text).map_err(|e| e.to_string())?;
        b.validate()?;
        Ok(b)
    }

    pub fn default_balance() -> Balance {
        Balance::from_ron(DEFAULT_BALANCE_RON).expect("assets/balance.ron must be valid")
    }

    /// Range checks, so a broken file or a hostile balance mod fails loudly
    /// instead of overflowing integers inside the simulation.
    pub fn validate(&self) -> Result<(), String> {
        fn range(name: &str, v: Milli, lo: i64, hi: i64) -> Result<(), String> {
            if v.0 < lo || v.0 > hi {
                Err(format!("{name} = {v} is out of range [{}, {}]", Milli(lo), Milli(hi)))
            } else {
                Ok(())
            }
        }
        if !(1..=120).contains(&self.tick_rate) {
            return Err(format!("tick_rate = {} is out of range [1, 120]", self.tick_rate));
        }
        if !(1..=655).contains(&self.unit.max_hp) {
            return Err("unit.max_hp must be in [1, 655]".into());
        }
        if !(1..=655).contains(&self.morale.max) {
            return Err("morale.max must be in [1, 655]".into());
        }
        range("unit.separation_radius", self.unit.separation_radius, 0, 4_000)?;
        range("unit.engaged_speed", self.unit.engaged_speed, 0, 1_000)?;
        for (n, u) in [("light", &self.unit.light), ("heavy", &self.unit.heavy)] {
            range(&format!("unit.{n}.speed"), u.speed, 0, 20_000)?;
            range(&format!("unit.{n}.atk"), u.atk, 1, 10_000)?;
            range(&format!("unit.{n}.def"), u.def, 1, 10_000)?;
            range(&format!("unit.{n}.upkeep"), u.upkeep, 0, 1_000_000)?;
        }
        range("combat.contact_radius", self.combat.contact_radius, 250, 8_000)?;
        if self.unit.separation_radius > self.combat.contact_radius {
            return Err("unit.separation_radius must not exceed combat.contact_radius".into());
        }
        range("combat.base_dps", self.combat.base_dps, 0, 1_000_000)?;
        range("combat.attacker_taken", self.combat.attacker_taken, 1, 10_000)?;
        range("morale.loss_attack", self.morale.loss_attack, 0, 1_000_000)?;
        range("morale.loss_defend", self.morale.loss_defend, 0, 1_000_000)?;
        range("morale.regen", self.morale.regen, 0, 1_000_000)?;
        range("water.damage_dps", self.water.damage_dps, 0, 1_000_000)?;
        range("water.ship_after", self.water.ship_after, 0, 600_000)?;
        range("city.defense_bonus", self.city.defense_bonus, 0, 10_000)?;
        range("city.capture_time", self.city.capture_time, 0, 600_000)?;
        range("economy.city_income", self.economy.city_income, 0, 1_000_000)?;
        range("supply.starve_dps", self.supply.starve_dps, 0, 1_000_000)?;
        range("supply.broke_starve_dps", self.supply.broke_starve_dps, 0, 1_000_000)?;
        range("supply.heal_dps", self.supply.heal_dps, 0, 1_000_000)?;
        range("supply.heal_city_dps", self.supply.heal_city_dps, 0, 1_000_000)?;
        range("supply.heal_delay", self.supply.heal_delay, 0, 600_000)?;
        if !(1..=16).contains(&self.territory.radius) {
            return Err("territory.radius must be in [1, 16]".into());
        }
        if !(1..=64).contains(&self.territory.city_radius) || !(0..=64).contains(&self.territory.rear_radius) {
            return Err("territory.city_radius must be in [1, 64] and rear_radius in [0, 64]".into());
        }
        if !(1..=100).contains(&self.supply.per_city) {
            return Err("supply.per_city must be in [1, 100]".into());
        }
        for (n, u) in [("light", &self.unit.light), ("heavy", &self.unit.heavy)] {
            if !(1..=100).contains(&u.supply) {
                return Err(format!("unit.{n}.supply must be in [1, 100]"));
            }
            if u.cost > 1_000_000 {
                return Err(format!("unit.{n}.cost must be at most 1000000"));
            }
        }
        if self.economy.start_money > 1_000_000 || self.match_rules.start_light_units > 1_000 {
            return Err("economy.start_money or match_rules.start_light_units is too large".into());
        }
        range("match_rules.win_city_share", self.match_rules.win_city_share, 1, 1_000)?;
        let t = &self.terrain;
        for (n, r) in [
            ("plain", &t.plain),
            ("forest", &t.forest),
            ("hills", &t.hills),
            ("sand", &t.sand),
            ("snow", &t.snow),
            ("mud", &t.mud),
            ("water", &t.water),
            ("ship", &t.ship),
        ] {
            range(&format!("terrain.{n}.light.speed"), r.light.0, 0, 10_000)?;
            range(&format!("terrain.{n}.light.atk"), r.light.1, 1, 10_000)?;
            range(&format!("terrain.{n}.heavy.speed"), r.heavy.0, 0, 10_000)?;
            range(&format!("terrain.{n}.heavy.atk"), r.heavy.1, 1, 10_000)?;
            range(&format!("terrain.{n}.defense"), r.defense, 0, 10_000)?;
        }
        Ok(())
    }

    /// Stable hash of the balance content (not of the file's formatting).
    /// Part of `sim_version`.
    pub fn content_hash(&self) -> u64 {
        let canonical = ron::to_string(self).expect("balance serializes");
        xxhash_rust::xxh3::xxh3_64(canonical.as_bytes())
    }
}

/// Number of terrain rows in [`Rules`] lookup tables: every [`Terrain`] plus the ship row.
pub const TERRAIN_ROWS: usize = Terrain::COUNT + 1;
/// Index of the "unit is a ship on water" row.
pub const SHIP_ROW: usize = Terrain::COUNT;

/// Balance converted to integer per-tick quantities. Lookups are small
/// tables indexed by terrain and unit kind (no `match` in hot loops).
#[derive(Clone, Debug)]
pub struct Rules {
    pub tick_rate: u32,
    pub max_hp: u16,
    pub max_morale: u16,
    /// Contact radius and its square in raw Q32.32.
    pub contact_radius: Fx,
    pub contact_r2: i64,
    pub separation_radius: Fx,
    pub engaged_speed: Milli,
    /// Plain-terrain speed per tick, by [`UnitKind`].
    pub speed_per_tick: [Fx; UnitKind::COUNT],
    pub atk: [Milli; UnitKind::COUNT],
    pub def: [Milli; UnitKind::COUNT],
    /// `[terrain row][kind]` multipliers.
    pub terrain_speed: [[Milli; UnitKind::COUNT]; TERRAIN_ROWS],
    pub terrain_atk: [[Milli; UnitKind::COUNT]; TERRAIN_ROWS],
    /// Defender multiplier `1 + bonus` per terrain row (cities include the city bonus).
    pub terrain_def: [Milli; TERRAIN_ROWS],
    pub passable: [bool; TERRAIN_ROWS],
    pub attacker_taken: Milli,
    /// Base damage per tick in Q16 HP-hundredths.
    pub base_dmg_q16: u64,
    pub morale_loss_attack_q16: u64,
    pub morale_loss_defend_q16: u64,
    pub morale_regen_q16: u64,
    pub water_dmg_q16: u64,
    pub ship_after_ticks: u16,
    /// Money is kept in thousandths. Cost of a unit by kind.
    pub cost: [i64; UnitKind::COUNT],
    /// Upkeep per second outside cities, thousandths of money.
    pub upkeep: [i64; UnitKind::COUNT],
    /// Supply places a unit takes, by kind.
    pub supply: [u32; UnitKind::COUNT],
    /// Income of one city per second, thousandths of money.
    pub city_income: i64,
    pub start_money: i64,
    pub per_city: u32,
    /// Max starvation damage per tick (Q16 HP hundredths) at full overload.
    pub starve_q16: u64,
    pub broke_starve_q16: u64,
    pub heal_q16: u64,
    pub heal_city_q16: u64,
    pub heal_delay_ticks: u16,
    pub territory_radius: u32,
    pub city_radius: u32,
    pub rear_radius: u32,
    pub capture_ticks: u16,
    pub win_city_share: Milli,
    pub start_light_units: u32,
    pub balance_hash: u64,
}

/// `per_second` (in units of the field, e.g. HP) → Q16 hundredths per tick.
fn rate_q16(per_second: Milli, tick_rate: u32) -> u64 {
    (per_second.0.max(0) as u64 * HP_SCALE as u64 * (1 << 16)) / (1000 * tick_rate as u64)
}

impl Rules {
    pub fn new(b: &Balance) -> Rules {
        let tr = b.tick_rate;
        let row = |r: &TerrainRow| ([r.light.0, r.heavy.0], [r.light.1, r.heavy.1], r.defense);
        let t = &b.terrain;
        let mut terrain_speed = [[Milli::ONE; UnitKind::COUNT]; TERRAIN_ROWS];
        let mut terrain_atk = [[Milli::ONE; UnitKind::COUNT]; TERRAIN_ROWS];
        let mut terrain_def = [Milli::ONE; TERRAIN_ROWS];
        let mut passable = [true; TERRAIN_ROWS];
        let mut set = |idx: usize, r: &TerrainRow| {
            let (s, a, d) = row(r);
            terrain_speed[idx] = s;
            terrain_atk[idx] = a;
            terrain_def[idx] = Milli(1000 + d.0);
        };
        set(Terrain::Plain as usize, &t.plain);
        set(Terrain::Forest as usize, &t.forest);
        set(Terrain::Hills as usize, &t.hills);
        set(Terrain::Sand as usize, &t.sand);
        set(Terrain::Snow as usize, &t.snow);
        set(Terrain::Mud as usize, &t.mud);
        set(Terrain::Water as usize, &t.water);
        set(Terrain::Bridge as usize, &t.plain);
        set(Terrain::City as usize, &t.plain);
        set(SHIP_ROW, &t.ship);
        terrain_def[Terrain::City as usize] = Milli(terrain_def[Terrain::Plain as usize].0 + b.city.defense_bonus.0);
        passable[Terrain::Mountain as usize] = false;
        terrain_speed[Terrain::Mountain as usize] = [Milli(0); UnitKind::COUNT];

        let contact_radius = Fx::from_milli(b.combat.contact_radius);
        let cr = contact_radius.0 as i64;
        let per_tick = |m: Milli| Fx::from_ratio(m.0, 1000 * tr as i64);
        Rules {
            tick_rate: tr,
            max_hp: (b.unit.max_hp as i64 * HP_SCALE) as u16,
            max_morale: (b.morale.max as i64 * HP_SCALE) as u16,
            contact_radius,
            contact_r2: cr * cr,
            separation_radius: Fx::from_milli(b.unit.separation_radius),
            engaged_speed: b.unit.engaged_speed,
            speed_per_tick: [per_tick(b.unit.light.speed), per_tick(b.unit.heavy.speed)],
            atk: [b.unit.light.atk, b.unit.heavy.atk],
            def: [b.unit.light.def, b.unit.heavy.def],
            terrain_speed,
            terrain_atk,
            terrain_def,
            passable,
            attacker_taken: b.combat.attacker_taken,
            base_dmg_q16: rate_q16(b.combat.base_dps, tr),
            morale_loss_attack_q16: rate_q16(b.morale.loss_attack, tr),
            morale_loss_defend_q16: rate_q16(b.morale.loss_defend, tr),
            morale_regen_q16: rate_q16(b.morale.regen, tr),
            water_dmg_q16: rate_q16(b.water.damage_dps, tr),
            ship_after_ticks: ((b.water.ship_after.0 * tr as i64) / 1000).min(u16::MAX as i64) as u16,
            cost: [b.unit.light.cost as i64 * 1000, b.unit.heavy.cost as i64 * 1000],
            upkeep: [b.unit.light.upkeep.0, b.unit.heavy.upkeep.0],
            supply: [b.unit.light.supply, b.unit.heavy.supply],
            city_income: b.economy.city_income.0,
            start_money: b.economy.start_money as i64 * 1000,
            per_city: b.supply.per_city,
            starve_q16: rate_q16(b.supply.starve_dps, tr),
            broke_starve_q16: rate_q16(b.supply.broke_starve_dps, tr),
            heal_q16: rate_q16(b.supply.heal_dps, tr),
            heal_city_q16: rate_q16(b.supply.heal_city_dps, tr),
            heal_delay_ticks: ((b.supply.heal_delay.0 * tr as i64) / 1000).min(u16::MAX as i64) as u16,
            territory_radius: b.territory.radius,
            city_radius: b.territory.city_radius,
            rear_radius: b.territory.rear_radius,
            capture_ticks: ((b.city.capture_time.0 * tr as i64) / 1000).clamp(1, u16::MAX as i64) as u16,
            win_city_share: b.match_rules.win_city_share,
            start_light_units: b.match_rules.start_light_units,
            balance_hash: b.content_hash(),
        }
    }

    /// Grid cell size: the contact radius, but at least a quarter tile.
    pub fn cell_size(&self) -> Fx {
        Fx(self.contact_radius.0.max(ONE / 4))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_balance_loads() {
        let b = Balance::default_balance();
        let r = Rules::new(&b);
        assert_eq!(r.max_hp, 10_000);
        // 9.2 HP/s at 30 tick/s = 30.666 hundredths per tick.
        assert_eq!(r.base_dmg_q16 >> 16, 30);
        assert_eq!(r.ship_after_ticks, 150);
        assert_eq!(r.terrain_def[Terrain::City as usize], Milli(1500));
        assert!(!r.passable[Terrain::Mountain as usize]);
    }

    #[test]
    fn content_hash_ignores_formatting() {
        let a = Balance::from_ron(DEFAULT_BALANCE_RON).unwrap();
        let squashed: String =
            DEFAULT_BALANCE_RON.lines().map(|l| l.split("//").next().unwrap()).collect::<Vec<_>>().join(" ");
        let b = Balance::from_ron(&squashed).unwrap();
        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn rejects_bad_values() {
        let bad = DEFAULT_BALANCE_RON.replace("contact_radius: \"1.75\"", "contact_radius: \"100\"");
        assert!(Balance::from_ron(&bad).unwrap_err().contains("contact_radius"));
        let float = DEFAULT_BALANCE_RON.replace("base_dps: \"9.2\"", "base_dps: 9.2");
        assert!(Balance::from_ron(&float).is_err());
    }
}
