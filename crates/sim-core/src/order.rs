//! Movement orders. One order (a path of up to 32 points) is shared by every
//! unit that received it in the same command; each unit adds its own
//! formation offset. Orders form immutable chains through `next` (Shift-queue).

use crate::fx::FxPoint;
use crate::unit::NO_ORDER;

pub const MAX_PATH_POINTS: usize = 32;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Order {
    pub points: Vec<FxPoint>,
    /// Order to continue with after the last point, or [`NO_ORDER`].
    pub next: u32,
    /// Units and orders referencing this one. 0 means the slot is free.
    refs: u32,
}

/// Reference-counted pool of orders. Ids are reused LIFO, so allocation is
/// deterministic as long as commands are applied in a deterministic order.
#[derive(Clone, Debug, Default)]
pub struct OrderPool {
    orders: Vec<Order>,
    free: Vec<u32>,
}

impl OrderPool {
    /// Allocates an order with zero references; the caller retains it.
    pub fn alloc(&mut self, points: Vec<FxPoint>, next: u32) -> u32 {
        let order = Order { points, next, refs: 0 };
        match self.free.pop() {
            Some(id) => {
                self.orders[id as usize] = order;
                id
            }
            None => {
                self.orders.push(order);
                (self.orders.len() - 1) as u32
            }
        }
    }

    #[inline]
    pub fn get(&self, id: u32) -> &Order {
        &self.orders[id as usize]
    }

    #[inline]
    pub fn retain(&mut self, id: u32) {
        if id != NO_ORDER {
            self.orders[id as usize].refs += 1;
        }
    }

    /// Drops one reference; frees the order (and releases its `next`) at zero.
    pub fn release(&mut self, mut id: u32) {
        while id != NO_ORDER {
            let o = &mut self.orders[id as usize];
            debug_assert!(o.refs > 0, "release of a free order {id}");
            o.refs -= 1;
            if o.refs > 0 {
                return;
            }
            let next = o.next;
            o.points = Vec::new();
            o.next = NO_ORDER;
            self.free.push(id);
            id = next;
        }
    }

    /// Number of live orders (for tests and debug panels).
    pub fn live(&self) -> usize {
        self.orders.len() - self.free.len()
    }

    /// Copies the chain starting at `id` from waypoint `from` on, optionally
    /// dropping everything after the first order, and appending `tail`.
    /// Returns the new chain head with zero references.
    pub fn copy_chain(&mut self, id: u32, from: usize, keep_queue: bool, tail: u32) -> u32 {
        let mut segments: Vec<Vec<FxPoint>> = Vec::new();
        let mut cur = id;
        let mut first = true;
        while cur != NO_ORDER {
            let o = self.get(cur);
            let start = if first { from.min(o.points.len()) } else { 0 };
            segments.push(o.points[start..].to_vec());
            first = false;
            if !keep_queue {
                break;
            }
            cur = o.next;
        }
        let mut next = tail;
        for seg in segments.into_iter().rev() {
            if seg.is_empty() {
                continue;
            }
            let n = self.alloc(seg, next);
            self.retain(next);
            next = n;
        }
        next
    }

    pub(crate) fn hash_into(&self, h: &mut xxhash_rust::xxh3::Xxh3) {
        for o in &self.orders {
            h.update(&o.refs.to_le_bytes());
            if o.refs == 0 {
                continue;
            }
            h.update(&o.next.to_le_bytes());
            h.update(&(o.points.len() as u32).to_le_bytes());
            for p in &o.points {
                h.update(&p.x.0.to_le_bytes());
                h.update(&p.y.0.to_le_bytes());
            }
        }
        crate::world::hash_u32s(h, &self.free);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::Fx;

    fn p(x: i32) -> FxPoint {
        FxPoint::new(Fx::from_int(x), Fx::ZERO)
    }

    #[test]
    fn refcount_frees_chains() {
        let mut pool = OrderPool::default();
        let b = pool.alloc(vec![p(2)], NO_ORDER);
        let a = pool.alloc(vec![p(1)], b);
        pool.retain(b); // referenced by a
        pool.retain(a); // unit 1
        pool.retain(a); // unit 2
        assert_eq!(pool.live(), 2);
        pool.release(a);
        assert_eq!(pool.live(), 2);
        pool.release(a);
        assert_eq!(pool.live(), 0);
    }

    #[test]
    fn copy_chain_appends() {
        let mut pool = OrderPool::default();
        let a = pool.alloc(vec![p(1), p(2), p(3)], NO_ORDER);
        pool.retain(a);
        let t = pool.alloc(vec![p(9)], NO_ORDER);
        let c = pool.copy_chain(a, 1, true, t);
        pool.retain(c);
        assert_eq!(pool.get(c).points, vec![p(2), p(3)]);
        assert_eq!(pool.get(c).next, t);
        pool.release(a);
        pool.release(c);
        assert_eq!(pool.live(), 0);
    }
}
