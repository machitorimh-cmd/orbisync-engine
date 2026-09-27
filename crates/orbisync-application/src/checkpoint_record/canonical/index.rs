//! Packed transient identities. Heap insertion/removal touches at most 16
//! levels at the generation limit. No table initialization, reallocation,
//! recursive sort, or element destructors are hidden in these operations.
use super::ScalarError;
use uuid::Uuid;

/// One UUID array; record order is never changed.
#[derive(Default)]
pub struct IdentityIndex {
    heap: Vec<Uuid>,
    previous: Option<Uuid>,
}

/// Runtime already owns the entity vector. Index its positions instead of
/// duplicating UUIDs; this leaves room for a simultaneous storage validator.
pub struct PositionIndex {
    heap: Vec<u32>,
    previous: Option<Uuid>,
}
impl PositionIndex {
    /// Maximum 256 KiB uninitialized backing allocation.
    pub fn new(limit: usize) -> Self {
        assert!(limit <= 65536);
        Self {
            heap: Vec::with_capacity(limit),
            previous: None,
        }
    }
    /// Requested backing storage, including unused slots.
    pub fn capacity_bytes(&self) -> usize {
        self.heap.capacity() * 4
    }
    /// Add a position; keys must remain unchanged until draining completes.
    /// The callback must be a constant-time lookup, without allocation.
    pub fn push(&mut self, position: u32, key: impl Fn(u32) -> Uuid) -> Result<(), ScalarError> {
        if self.previous.is_some() || self.heap.len() == self.heap.capacity() {
            return Err(ScalarError::Invalid);
        }
        let mut child = self.heap.len();
        self.heap.push(position);
        while child > 0 {
            let parent = (child - 1) / 2;
            if key(self.heap[parent]) >= key(self.heap[child]) {
                break;
            }
            self.heap.swap(parent, child);
            child = parent;
        }
        Ok(())
    }
    /// One bounded heap removal. Includes <=64 constant-time UUID lookups,
    /// <=32 comparisons and <=16 four-byte swaps: <=4096 logical work units.
    pub fn drain_step(&mut self, key: impl Fn(u32) -> Uuid) -> Result<bool, ScalarError> {
        if self.heap.is_empty() {
            return Ok(true);
        }
        let id = key(self.heap.swap_remove(0));
        if self.previous == Some(id) {
            return Err(ScalarError::Invalid);
        }
        self.previous = Some(id);
        let mut parent = 0;
        loop {
            let left = parent * 2 + 1;
            if left >= self.heap.len() {
                break;
            }
            let right = left + 1;
            let child = if right < self.heap.len() && key(self.heap[right]) > key(self.heap[left]) {
                right
            } else {
                left
            };
            if key(self.heap[parent]) >= key(self.heap[child]) {
                break;
            }
            self.heap.swap(parent, child);
            parent = child;
        }
        Ok(false)
    }
}
impl IdentityIndex {
    /// Reserve uninitialized capacity once. At most 1 MiB of requested heap.
    pub fn new(limit: usize) -> Self {
        assert!(limit <= 65_536);
        Self {
            heap: Vec::with_capacity(limit),
            previous: None,
        }
    }
    /// Requested backing capacity, including unused slots.
    pub fn capacity_bytes(&self) -> usize {
        self.heap.capacity() * 16
    }
    /// Insert in at most 16 heap levels. Duplicate checking is deferred until
    /// drain completes; callers must not publish before then.
    pub fn push(&mut self, id: Uuid) -> Result<(), ScalarError> {
        if self.previous.is_some() || self.heap.len() == self.heap.capacity() {
            return Err(ScalarError::Invalid);
        }
        let mut child = self.heap.len();
        self.heap.push(id);
        while child > 0 {
            let parent = (child - 1) / 2;
            if self.heap[parent] >= self.heap[child] {
                break;
            }
            self.heap.swap(parent, child);
            child = parent;
        }
        Ok(())
    }
    /// Remove one identity and check its descending neighbor. Observe/yield
    /// between calls. Returns true only after all identities are checked.
    /// Worst case: 16 levels * (two 32-byte comparisons + 64-byte swap),
    /// plus root exchange and control, conservatively 4096 work units.
    pub fn drain_step(&mut self) -> Result<bool, ScalarError> {
        if self.heap.is_empty() {
            return Ok(true);
        }
        let id = self.heap.swap_remove(0);
        if self.previous == Some(id) {
            return Err(ScalarError::Invalid);
        }
        self.previous = Some(id);
        let mut parent = 0;
        loop {
            let left = parent * 2 + 1;
            if left >= self.heap.len() {
                break;
            }
            let right = left + 1;
            let child = if right < self.heap.len() && self.heap[right] > self.heap[left] {
                right
            } else {
                left
            };
            if self.heap[parent] >= self.heap[child] {
                break;
            }
            self.heap.swap(parent, child);
            parent = child;
        }
        Ok(false)
    }
}
