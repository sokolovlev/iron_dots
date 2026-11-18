//! Unit storage: Structure of Arrays plus generational handles.
//!
//! Units are addressed from outside (commands, UI) by a [`UnitHandle`], which
//! stays valid while the unit is alive even though its index in the arrays
//! changes when other units die (swap-remove) or are re-sorted.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum UnitKind {
    Light = 0,
    Heavy = 1,
}

impl UnitKind {
    pub const COUNT: usize = 2;
    pub const ALL: [UnitKind; UnitKind::COUNT] = [UnitKind::Light, UnitKind::Heavy];
}

/// Stable reference to a unit: slot number plus generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnitHandle {
    pub slot: u32,
    pub generation: u32,
}

/// Bit flags in [`UnitStore::flags`].
pub mod flags {
    /// Spent long enough in water to become a ship.
    pub const SHIP: u8 = 1 << 0;
    /// Had at least one enemy in contact during the last combat phase.
    pub const IN_COMBAT: u8 = 1 << 1;
    /// In combat while having a movement order (takes more damage, loses more morale).
    pub const ATTACKING: u8 = 1 << 2;
}

/// "No order" marker in [`UnitStore::order`].
pub const NO_ORDER: u32 = u32::MAX;
const FREE: u32 = u32::MAX;

/// All per-unit data, one array per field. Index `i` in every array is the
/// same unit. Order of units in the arrays is deterministic.
#[derive(Clone, Debug, Default)]
pub struct UnitStore {
    /// Position in tiles, Q16.16.
    pub pos_x: Vec<i32>,
    pub pos_y: Vec<i32>,
    /// HP in hundredths (0..=10 000 with default balance).
    pub hp: Vec<u16>,
    /// Morale in hundredths.
    pub morale: Vec<u16>,
    /// Owning player id (1..=8).
    pub owner: Vec<u8>,
    /// [`UnitKind`] discriminant.
    pub kind: Vec<u8>,
    pub flags: Vec<u8>,
    /// Terrain rule row the unit stands on (terrain discriminant or ship row).
    pub row: Vec<u8>,
    /// Consecutive ticks spent in water.
    pub water_ticks: Vec<u16>,
    /// Current order id in the order pool, or [`NO_ORDER`].
    pub order: Vec<u32>,
    /// Index of the next waypoint within the current order.
    pub waypoint: Vec<u8>,
    /// Formation offset added to every waypoint, Q16.16.
    pub off_x: Vec<i32>,
    pub off_y: Vec<i32>,
    /// Ticks without progress toward the current waypoint.
    pub stuck: Vec<u8>,
    /// Slot of this unit (stable id).
    pub slot: Vec<u32>,

    slot_generation: Vec<u32>,
    slot_index: Vec<u32>,
    free_slots: Vec<u32>,
}

impl UnitStore {
    #[inline]
    pub fn len(&self) -> usize {
        self.pos_x.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pos_x.is_empty()
    }

    /// Adds a unit and returns its handle. Slots are reused LIFO, which is
    /// deterministic because spawns happen in a deterministic order.
    #[allow(clippy::too_many_arguments)]
    pub fn push(&mut self, owner: u8, kind: UnitKind, x: i32, y: i32, hp: u16, morale: u16) -> UnitHandle {
        let index = self.len() as u32;
        let slot = match self.free_slots.pop() {
            Some(s) => {
                self.slot_index[s as usize] = index;
                s
            }
            None => {
                self.slot_generation.push(0);
                self.slot_index.push(index);
                (self.slot_generation.len() - 1) as u32
            }
        };
        self.pos_x.push(x);
        self.pos_y.push(y);
        self.hp.push(hp);
        self.morale.push(morale);
        self.owner.push(owner);
        self.kind.push(kind as u8);
        self.flags.push(0);
        self.row.push(0);
        self.water_ticks.push(0);
        self.order.push(NO_ORDER);
        self.waypoint.push(0);
        self.off_x.push(0);
        self.off_y.push(0);
        self.stuck.push(0);
        self.slot.push(slot);
        UnitHandle { slot, generation: self.slot_generation[slot as usize] }
    }

    /// Removes unit `i` by moving the last unit into its place.
    /// The caller must release the unit's order first.
    pub fn swap_remove(&mut self, i: usize) {
        let slot = self.slot[i];
        let last = self.len() - 1;
        if i != last {
            let moved_slot = self.slot[last];
            self.slot_index[moved_slot as usize] = i as u32;
        }
        self.pos_x.swap_remove(i);
        self.pos_y.swap_remove(i);
        self.hp.swap_remove(i);
        self.morale.swap_remove(i);
        self.owner.swap_remove(i);
        self.kind.swap_remove(i);
        self.flags.swap_remove(i);
        self.row.swap_remove(i);
        self.water_ticks.swap_remove(i);
        self.order.swap_remove(i);
        self.waypoint.swap_remove(i);
        self.off_x.swap_remove(i);
        self.off_y.swap_remove(i);
        self.stuck.swap_remove(i);
        self.slot.swap_remove(i);
        self.slot_generation[slot as usize] = self.slot_generation[slot as usize].wrapping_add(1);
        self.slot_index[slot as usize] = FREE;
        self.free_slots.push(slot);
    }

    /// Current array index of a live unit, or `None` if the handle is stale.
    #[inline]
    pub fn index_of(&self, h: UnitHandle) -> Option<usize> {
        let s = h.slot as usize;
        if s < self.slot_generation.len() && self.slot_generation[s] == h.generation && self.slot_index[s] != FREE {
            Some(self.slot_index[s] as usize)
        } else {
            None
        }
    }

    /// Handle of the unit at array index `i`.
    #[inline]
    pub fn handle(&self, i: usize) -> UnitHandle {
        let slot = self.slot[i];
        UnitHandle { slot, generation: self.slot_generation[slot as usize] }
    }

    /// Feeds slot bookkeeping into a hasher (slot reuse affects future handles).
    pub(crate) fn hash_slots(&self, h: &mut xxhash_rust::xxh3::Xxh3) {
        crate::world::hash_u32s(h, &self.slot_generation);
        crate::world::hash_u32s(h, &self.slot_index);
        crate::world::hash_u32s(h, &self.free_slots);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_survive_swap_remove() {
        let mut s = UnitStore::default();
        let a = s.push(1, UnitKind::Light, 0, 0, 100, 100);
        let b = s.push(1, UnitKind::Light, 1, 0, 100, 100);
        let c = s.push(2, UnitKind::Heavy, 2, 0, 100, 100);
        s.swap_remove(s.index_of(a).unwrap());
        assert_eq!(s.index_of(a), None);
        assert_eq!(s.index_of(c), Some(0));
        assert_eq!(s.pos_x[s.index_of(c).unwrap()], 2);
        assert_eq!(s.index_of(b), Some(1));
        // Slot of `a` is reused with a new generation.
        let d = s.push(1, UnitKind::Light, 3, 0, 100, 100);
        assert_eq!(d.slot, a.slot);
        assert_ne!(d.generation, a.generation);
        assert_eq!(s.index_of(a), None);
        assert_eq!(s.index_of(d), Some(2));
        assert_eq!(s.handle(2), d);
    }
}
