use std::collections::{HashSet, VecDeque};

pub const PAGE_SIDE: usize = 512;
pub const SLOT_SIDE: usize = 64;
pub const SLOTS_PER_PAGE: usize = (PAGE_SIDE / SLOT_SIDE) * (PAGE_SIDE / SLOT_SIDE);
pub const PAGE_COUNT: usize = 2;
pub const PENDING_LIMIT: usize = 128;
pub const UPLOAD_LIMIT: usize = 8;
pub const NEGATIVE_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub page: usize,
    pub slot: usize,
}

pub struct Policy {
    occupied: Vec<Vec<Option<u64>>>,
    order: VecDeque<(usize, usize, u64)>,
    queued: HashSet<u64>,
    queue: VecDeque<u64>,
    failed: VecDeque<u64>,
}

impl Policy {
    pub fn new() -> Self {
        Self {
            occupied: vec![vec![None; SLOTS_PER_PAGE]; PAGE_COUNT],
            order: VecDeque::new(),
            queued: HashSet::new(),
            queue: VecDeque::new(),
            failed: VecDeque::new(),
        }
    }
    pub fn queue(&mut self, key: u64) -> bool {
        if self.queued.contains(&key) || self.failed.contains(&key) {
            return false;
        }
        if self.queue.len() == PENDING_LIMIT {
            return false;
        }
        self.queue.push_back(key);
        self.queued.insert(key);
        true
    }
    pub fn pop_queue(&mut self) -> Option<u64> {
        let key = self.queue.pop_front()?;
        self.queued.remove(&key);
        Some(key)
    }
    pub fn fail(&mut self, key: u64) {
        if self.failed.len() == NEGATIVE_LIMIT {
            self.failed.pop_front();
        }
        self.failed.push_back(key);
    }
    pub fn is_failed(&self, key: u64) -> bool {
        self.failed.contains(&key)
    }
    pub fn allocate(&mut self, key: u64) -> Slot {
        let slot = self
            .occupied
            .iter()
            .enumerate()
            .find_map(|(p, page)| {
                page.iter()
                    .position(Option::is_none)
                    .map(|s| Slot { page: p, slot: s })
            })
            .unwrap_or_else(|| {
                let (p, s, old) = self.order.pop_front().expect("full cache has an LRU slot");
                self.occupied[p][s] = None;
                let _ = old;
                Slot { page: p, slot: s }
            });
        self.occupied[slot.page][slot.slot] = Some(key);
        self.order.push_back((slot.page, slot.slot, key));
        slot
    }
    pub fn touch(&mut self, key: u64) {
        if let Some(pos) = self.order.iter().position(|(_, _, k)| *k == key) {
            if let Some(item) = self.order.remove(pos) {
                self.order.push_back(item);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_queue_deduplicates_and_slots_evict_lru() {
        let mut p = Policy::new();
        assert!(p.queue(7));
        assert!(!p.queue(7));
        for k in 0..PENDING_LIMIT as u64 {
            p.queue(1000 + k);
        }
        assert_eq!(p.queue.len(), PENDING_LIMIT);
        assert!(!p.queue(9999));
        for k in 0..(SLOTS_PER_PAGE * PAGE_COUNT) as u64 {
            p.allocate(k);
        }
        p.touch(0);
        let first = p.allocate(999);
        assert_eq!(first, Slot { page: 0, slot: 1 });
        assert_eq!(p.occupied[0][0], Some(0));
        assert_eq!(p.occupied[0][1], Some(999));
        assert_eq!(
            p.occupied
                .iter()
                .map(|page| page.iter().filter(|s| s.is_some()).count())
                .sum::<usize>(),
            SLOTS_PER_PAGE * PAGE_COUNT
        );
    }
}
