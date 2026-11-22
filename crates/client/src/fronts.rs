//! Positions (front lines): the player draws a curve, places are laid out
//! along it, and units take them. Places that empty (a unit died or got
//! another order) are refilled from idle units, city garrisons included.
//!
//! This is the player's helper, not a simulation rule: it only issues
//! ordinary Move commands, so it works the same online.

use sim_core::UnitHandle;
use sim_core::unit::{NO_ORDER, UnitStore, flags};

/// Distance between places along a front (a body is 1.5 tiles across).
pub const SPACING: f32 = 1.6;
/// Most places in one front.
pub const MAX_PLACES: usize = 400;
/// Units further than this from their place count as not there yet.
const AT_PLACE: f32 = 1.0;

pub struct Front {
    /// The curve as drawn (world tiles).
    pub line: Vec<[f32; 2]>,
    pub places: Vec<[f32; 2]>,
    /// The unit holding (or heading to) each place.
    pub held: Vec<Option<UnitHandle>>,
}

impl Front {
    /// Places every [`SPACING`] tiles along `line`.
    pub fn new(line: Vec<[f32; 2]>) -> Front {
        let mut places = Vec::new();
        if let Some(&first) = line.first() {
            places.push(first);
            let mut carry = 0.0;
            for w in line.windows(2) {
                let (a, b) = (w[0], w[1]);
                let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
                let mut t = SPACING - carry;
                while t <= len && places.len() < MAX_PLACES {
                    let f = t / len;
                    places.push([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]);
                    t += SPACING;
                }
                carry = (carry + len) % SPACING;
            }
        }
        let held = vec![None; places.len()];
        Front { line, places, held }
    }

    pub fn filled(&self, u: &UnitStore) -> usize {
        self.held.iter().filter(|h| h.is_some_and(|h| u.index_of(h).is_some())).count()
    }
}

fn d2(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)
}

fn pos(u: &UnitStore, i: usize) -> [f32; 2] {
    [u.pos_x[i] as f32 / 65536.0, u.pos_y[i] as f32 / 65536.0]
}

/// Gives the empty places of `front` to the nearest of `candidates`
/// (unit indices); returns `(unit, place)` moves to order.
pub fn fill(front: &mut Front, u: &UnitStore, candidates: &mut Vec<usize>) -> Vec<(UnitHandle, [f32; 2])> {
    let mut moves = Vec::new();
    for k in 0..front.places.len() {
        if front.held[k].is_some() || candidates.is_empty() {
            continue;
        }
        let p = front.places[k];
        let best = (0..candidates.len())
            .min_by(|&a, &b| d2(pos(u, candidates[a]), p).total_cmp(&d2(pos(u, candidates[b]), p)))
            .expect("not empty");
        let i = candidates.swap_remove(best);
        let h = u.handle(i);
        front.held[k] = Some(h);
        moves.push((h, p));
    }
    moves
}

/// Frees places whose unit died or no longer belongs there: it is idle
/// far from its place (it was ordered elsewhere and arrived) or `released`
/// by a player order.
pub fn refresh(front: &mut Front, u: &UnitStore, released: &[UnitHandle]) {
    for k in 0..front.places.len() {
        let Some(h) = front.held[k] else { continue };
        let gone = match u.index_of(h) {
            None => true,
            Some(i) => {
                released.contains(&h)
                    || (u.order[i] == NO_ORDER
                        && u.flags[i] & flags::IN_COMBAT == 0
                        && d2(pos(u, i), front.places[k]) > (AT_PLACE + 2.0) * (AT_PLACE + 2.0))
            }
        };
        if gone {
            front.held[k] = None;
        }
    }
}
