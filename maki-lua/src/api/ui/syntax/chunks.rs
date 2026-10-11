//! A list of things placed in a text, sorted by where they start and cut
//! into pieces that copies share. An edit moves the pieces after it by a
//! number kept on each piece, and a change copies only the piece it lands
//! in. So a copy for every parse costs a pointer per piece, and a keystroke
//! touches a few pieces, not every mark.

use std::sync::Arc;

/// Pieces split above twice this and merge below half of it.
const PIECE: usize = 64;

pub(super) trait Spot: Clone {
    fn start(&self) -> usize;
    fn end(&self) -> usize;
    fn shift(&mut self, by: isize);
}

#[derive(Clone)]
struct Piece<T> {
    /// Added to every position in `items`.
    shift: isize,
    items: Arc<Vec<T>>,
    /// Where the first and the last item start and the furthest one ends,
    /// with `shift` in.
    first: usize,
    last: usize,
    end: usize,
}

impl<T: Spot> Piece<T> {
    fn new(items: Vec<T>) -> Self {
        let mut piece = Self {
            shift: 0,
            items: Arc::new(items),
            first: 0,
            last: 0,
            end: 0,
        };
        piece.measure();
        piece
    }

    fn at(&self, pos: usize) -> usize {
        shifted(pos, self.shift)
    }

    fn measure(&mut self) {
        let (Some(first), Some(last)) = (self.items.first(), self.items.last()) else {
            return;
        };
        self.first = self.at(first.start());
        self.last = self.at(last.start());
        self.end = self.at(self.items.iter().map(Spot::end).max().unwrap_or(0));
    }

    /// The items, with `shift` folded into them, and copied first when
    /// another copy of the list shares them.
    fn items_mut(&mut self) -> &mut Vec<T> {
        let items = Arc::make_mut(&mut self.items);
        if self.shift != 0 {
            for item in items.iter_mut() {
                item.shift(self.shift);
            }
            self.shift = 0;
        }
        items
    }

    fn move_by(&mut self, by: isize) {
        self.shift += by;
        self.first = shifted(self.first, by);
        self.last = shifted(self.last, by);
        self.end = shifted(self.end, by);
    }
}

#[derive(Clone)]
pub(super) struct Chunks<T> {
    pieces: Vec<Piece<T>>,
}

impl<T> Default for Chunks<T> {
    fn default() -> Self {
        Self { pieces: Vec::new() }
    }
}

impl<T: Spot> Chunks<T> {
    /// {items} must be sorted by start.
    pub fn from_sorted(items: Vec<T>) -> Self {
        let pieces = items
            .chunks(PIECE)
            .map(|chunk| Piece::new(chunk.to_vec()))
            .collect();
        Self { pieces }
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// Every item, with how far it moved since it was put in.
    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = (isize, &T)> {
        self.pieces
            .iter()
            .flat_map(|piece| piece.items.iter().map(|item| (piece.shift, item)))
    }

    pub fn starting_in(&self, lo: usize, hi: usize) -> impl Iterator<Item = (isize, &T)> {
        let first = self.pieces.partition_point(|piece| piece.last < lo);
        self.pieces[first..]
            .iter()
            .take_while(move |piece| piece.first <= hi)
            .flat_map(move |piece| {
                let skip = piece
                    .items
                    .partition_point(|item| piece.at(item.start()) < lo);
                piece.items[skip..]
                    .iter()
                    .take_while(move |item| piece.at(item.start()) <= hi)
                    .map(move |item| (piece.shift, item))
            })
    }

    /// The items touching `from..=to`. Every piece is looked at, so this is
    /// for few items or a rare call.
    pub fn touching(&self, from: usize, to: usize) -> impl Iterator<Item = (isize, &T)> {
        self.pieces
            .iter()
            .filter(move |piece| piece.first <= to && piece.end >= from)
            .flat_map(move |piece| {
                piece
                    .items
                    .iter()
                    .filter(move |item| {
                        piece.at(item.start()) <= to && piece.at(item.end()) >= from
                    })
                    .map(move |item| (piece.shift, item))
            })
    }

    /// Moves every item at or after {old_end} by what the edit added. No
    /// item may start inside the edit or reach into it from before.
    pub fn edit(&mut self, old_end: usize, new_end: usize) {
        let by = new_end as isize - old_end as isize;
        for piece in &mut self.pieces {
            if piece.first >= old_end {
                piece.move_by(by);
            } else if piece.last >= old_end {
                for item in piece.items_mut().iter_mut() {
                    if item.start() >= old_end {
                        item.shift(by);
                    }
                }
                piece.measure();
            }
        }
    }

    /// Puts {item} in after the items {before} says go first.
    pub fn insert(&mut self, item: T, before: impl Fn(&T, &T) -> bool) {
        if self.pieces.is_empty() {
            self.pieces.push(Piece::new(vec![item]));
            return;
        }
        let start = item.start();
        let at = self
            .pieces
            .partition_point(|piece| piece.first <= start)
            .saturating_sub(1);
        let items = self.pieces[at].items_mut();
        let pos = items.partition_point(|other| before(other, &item));
        items.insert(pos, item);
        self.pieces[at].measure();
        self.settle(at);
    }

    /// The first item starting at {start} that {pick} wants, to change or
    /// take out. `pick` sees items with their shift in.
    pub fn update(
        &mut self,
        start: usize,
        pick: impl Fn(&T) -> bool,
        change: impl FnOnce(&mut T) -> bool,
    ) -> bool {
        let first = self.pieces.partition_point(|piece| piece.last < start);
        for at in first..self.pieces.len() {
            let piece = &self.pieces[at];
            if piece.first > start {
                break;
            }
            let found = piece.items.iter().position(|item| {
                piece.at(item.start()) == start && {
                    let mut moved = item.clone();
                    moved.shift(piece.shift);
                    pick(&moved)
                }
            });
            let Some(pos) = found else {
                continue;
            };
            let items = self.pieces[at].items_mut();
            if !change(&mut items[pos]) {
                items.remove(pos);
            }
            self.pieces[at].measure();
            self.settle(at);
            return true;
        }
        false
    }

    /// Keeps the piece at {at} in shape: gone when empty, split when big,
    /// merged into the next when small.
    fn settle(&mut self, at: usize) {
        let len = self.pieces[at].items.len();
        if len == 0 {
            self.pieces.remove(at);
            return;
        }
        if len > 2 * PIECE {
            let tail = self.pieces[at].items_mut().split_off(len / 2);
            self.pieces[at].measure();
            self.pieces.insert(at + 1, Piece::new(tail));
        } else if len < PIECE / 2 && at + 1 < self.pieces.len() {
            let next = self.pieces.remove(at + 1);
            let mut next_items = next.items.as_ref().clone();
            for item in &mut next_items {
                item.shift(next.shift);
            }
            self.pieces[at].items_mut().append(&mut next_items);
            self.pieces[at].measure();
            self.settle(at);
        }
    }
}

/// {pos} moved by {by}. A spot never moves past either end of the text.
pub(super) fn shifted(pos: usize, by: isize) -> usize {
    debug_assert!(
        pos.checked_add_signed(by).is_some(),
        "{pos} shifted by {by} leaves the text"
    );
    pos.saturating_add_signed(by)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    impl Spot for (usize, usize) {
        fn start(&self) -> usize {
            self.0
        }
        fn end(&self) -> usize {
            self.1
        }
        fn shift(&mut self, by: isize) {
            self.0 = shifted(self.0, by);
            self.1 = shifted(self.1, by);
        }
    }

    fn spots(chunks: &Chunks<(usize, usize)>) -> Vec<(usize, usize)> {
        chunks
            .iter()
            .map(|(shift, item)| {
                let mut item = *item;
                item.shift(shift);
                item
            })
            .collect()
    }

    /// Edits, removals and inserts on a copy leave the original as it was,
    /// and the copy reads like a plain sorted list given the same changes.
    #[test_case(10  ; "one_piece")]
    #[test_case(500 ; "many_pieces")]
    fn a_copy_changes_like_a_sorted_list_and_leaves_the_original(count: usize) {
        let items: Vec<_> = (0..count).map(|at| (at * 10, at * 10 + 5)).collect();
        let original = Chunks::from_sorted(items.clone());
        let mut copy = original.clone();
        let mut plain = items.clone();

        let (from, to) = (count * 4, count * 6);
        plain.retain(|item| {
            let touching = item.0 <= to && item.1 >= from;
            if touching {
                assert!(copy.update(item.0, |other| other == item, |_| false));
            }
            !touching
        });

        copy.edit(to, to + 7);
        for item in &mut plain {
            if item.0 >= to {
                item.shift(7);
            }
        }
        for at in [0, from + 1, count * 20] {
            copy.insert((at, at + 1), |a, b| a.0 <= b.0);
            let pos = plain.partition_point(|item| item.0 <= at);
            plain.insert(pos, (at, at + 1));
        }
        assert_eq!(spots(&copy), plain);
        assert_eq!(spots(&original), items);
        let starting: Vec<_> = copy
            .starting_in(from, to + 100)
            .map(|(shift, item)| shifted(item.0, shift))
            .collect();
        let expected: Vec<_> = plain
            .iter()
            .map(|item| item.0)
            .filter(|start| (from..=to + 100).contains(start))
            .collect();
        assert_eq!(starting, expected);
    }
}
