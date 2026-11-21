use crate::core::cell::{Cursor, Row};
use crate::core::geometry::Size;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Frame {
    pub size: Size,
    pub rows: Vec<Row>,
    pub cursor: Cursor,
    pub seq: u64,
}

/// Return the indices of rows that differ between two frames.
pub fn changed_rows(old: &[Row], new: &[Row]) -> Vec<usize> {
    let common = old.len().min(new.len());
    let mut changed: Vec<usize> = (0..common).filter(|&i| old[i] != new[i]).collect();
    changed.extend(common..new.len());
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cell::Cell;

    #[test]
    fn diff_finds_changed_and_added_rows() {
        let old = vec![Row::default(), Row::default()];
        let mut changed_row = Row::default();
        changed_row.cells.push(Cell::blank());
        let new = vec![Row::default(), changed_row, Row::default()];
        assert_eq!(changed_rows(&old, &new), vec![1, 2]);
        assert!(changed_rows(&new, &new).is_empty());
    }
}
