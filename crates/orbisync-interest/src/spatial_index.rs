//! Spatial index derived view for the uniform grid.

use std::collections::{HashMap, HashSet};

use orbisync_domain::{EntityId, transform::Vec3};

use crate::uniform_grid::{CellCoord, UniformGrid};

/// Pure, I/O-free spatial index mapping entity positions to grid cells.
///
/// This is the derived view owned by `interest` (MRIB §7.2). It is
/// recomputable from `instance_runtime` state and never performs I/O.
#[derive(Debug, Clone)]
pub struct SpatialIndex {
    grid: UniformGrid,
    cells: HashMap<CellCoord, HashSet<EntityId>>,
    positions: HashMap<EntityId, (CellCoord, Vec3)>,
}

impl SpatialIndex {
    /// Creates an empty index for the given grid.
    #[must_use]
    pub fn new(grid: UniformGrid) -> Self {
        Self {
            grid,
            cells: HashMap::new(),
            positions: HashMap::new(),
        }
    }

    /// Returns the grid used for cell assignment.
    #[must_use]
    pub const fn grid(&self) -> &UniformGrid {
        &self.grid
    }

    /// Returns the number of indexed entities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    /// Returns true when no entity is indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    /// Assigns `position` to its cell using the grid.
    #[must_use]
    pub fn cell_for(&self, position: Vec3) -> CellCoord {
        self.grid.cell_for(position)
    }

    /// Inserts or updates `entity_id` at `position`.
    ///
    /// Returns the previous cell when the entity was already indexed.
    pub fn insert(&mut self, entity_id: EntityId, position: Vec3) -> Option<CellCoord> {
        let new_cell = self.grid.cell_for(position);
        let previous = self.positions.insert(entity_id, (new_cell, position));
        if let Some((prev_cell, _)) = previous {
            if prev_cell != new_cell {
                if let Some(set) = self.cells.get_mut(&prev_cell) {
                    set.remove(&entity_id);
                    if set.is_empty() {
                        self.cells.remove(&prev_cell);
                    }
                }
            } else {
                // Same cell: ensure entry exists in cells map.
                self.cells.entry(new_cell).or_default().insert(entity_id);
                return Some(prev_cell);
            }
        }
        self.cells.entry(new_cell).or_default().insert(entity_id);
        previous.map(|(c, _)| c)
    }

    /// Removes an entity from the index.
    ///
    /// Returns true when the entity was present.
    pub fn remove(&mut self, entity_id: EntityId) -> bool {
        let Some((cell, _)) = self.positions.remove(&entity_id) else {
            return false;
        };
        if let Some(set) = self.cells.get_mut(&cell) {
            set.remove(&entity_id);
            if set.is_empty() {
                self.cells.remove(&cell);
            }
        }
        true
    }

    /// Updates an entity's position, inserting it when absent.
    ///
    /// Returns true when the entity was already present.
    pub fn update(&mut self, entity_id: EntityId, new_position: Vec3) -> bool {
        let existed = self.positions.contains_key(&entity_id);
        self.insert(entity_id, new_position);
        existed
    }

    /// Returns the entity's current cell and position when indexed.
    #[must_use]
    pub fn get(&self, entity_id: EntityId) -> Option<(CellCoord, Vec3)> {
        self.positions.get(&entity_id).copied()
    }

    /// Returns all entity ids whose cells are subscribed by a viewer at
    /// `viewer_pos` (own cell + 8 neighbours).
    #[must_use]
    pub fn candidates_for(&self, viewer_pos: Vec3) -> Vec<EntityId> {
        let subscribed = self.grid.subscribed_cells(viewer_pos);
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < subscribed.len() {
            if let Some(set) = self.cells.get(&subscribed[i]) {
                out.extend(set.iter().copied());
            }
            i += 1;
        }
        out
    }

    /// Returns all entity ids in a specific cell.
    #[must_use]
    pub fn entities_in(&self, coord: CellCoord) -> Vec<EntityId> {
        self.cells
            .get(&coord)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Clears all entries.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.positions.clear();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::SpatialIndex;
    use crate::uniform_grid::UniformGrid;
    use orbisync_domain::{EntityId, transform::Vec3};

    fn vec3(x: f64, y: f64, z: f64) -> Vec3 {
        Vec3::new(x, y, z).expect("valid vec3")
    }

    #[test]
    fn test_insert_and_candidates_within_neighbourhood() {
        let grid = UniformGrid::default();
        let mut index = SpatialIndex::new(grid);
        let viewer = vec3(0.0, 0.0, 0.0);
        let near = EntityId::generate();
        let far = EntityId::generate();
        index.insert(near, vec3(5.0, 0.0, 5.0));
        index.insert(far, vec3(200.0, 0.0, 200.0));
        let cands = index.candidates_for(viewer);
        assert!(cands.contains(&near));
        assert!(!cands.contains(&far));
    }

    #[test]
    fn test_update_moves_between_cells() {
        let grid = UniformGrid::default();
        let mut index = SpatialIndex::new(grid);
        let id = EntityId::generate();
        index.insert(id, vec3(0.0, 0.0, 0.0));
        let cell_before = index.cell_for(vec3(0.0, 0.0, 0.0));
        assert_eq!(index.get(id).map(|(c, _)| c), Some(cell_before));
        index.update(id, vec3(100.0, 0.0, 0.0));
        let cell_after = index.cell_for(vec3(100.0, 0.0, 0.0));
        assert_eq!(index.get(id).map(|(c, _)| c), Some(cell_after));
        assert_ne!(cell_before, cell_after);
        assert!(index.entities_in(cell_before).is_empty());
        assert!(index.entities_in(cell_after).contains(&id));
    }

    #[test]
    fn test_remove_clears_index() {
        let grid = UniformGrid::default();
        let mut index = SpatialIndex::new(grid);
        let id = EntityId::generate();
        index.insert(id, vec3(0.0, 0.0, 0.0));
        assert_eq!(index.len(), 1);
        assert!(index.remove(id));
        assert!(index.is_empty());
        assert!(!index.remove(id));
    }

    #[test]
    fn test_subscribed_cells_uses_cell_assignment() {
        let grid = UniformGrid::default();
        let mut index = SpatialIndex::new(grid);
        let viewer = vec3(20.0, 0.0, 20.0);
        let origin_cell = grid.cell_for(viewer);
        let diagonal = vec3(45.0, 0.0, 45.0);
        let diag_cell = grid.cell_for(diagonal);
        // viewer at (1,1) -> diagonal (2,2) is within 3x3 neighbourhood
        let e1 = EntityId::generate();
        index.insert(e1, diagonal);
        assert!(index.candidates_for(viewer).contains(&e1));
        assert_eq!(origin_cell, crate::uniform_grid::CellCoord::new(1, 1));
        assert_eq!(diag_cell, crate::uniform_grid::CellCoord::new(2, 2));
    }
}
