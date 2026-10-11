//! A list of things placed in a text, sorted by where they start and cut
//! into pieces that copies share. An edit moves the pieces after it by a
//! number kept on each piece, and a change copies only the piece it lands
//! in. So a copy for every parse costs a pointer per piece, and a keystroke
//! touches a few pieces, not every item.

use std::iter;
use std::sync::Arc;

/// An edit steps over every piece and rewrites the one under it, so its
/// cost is the number of pieces plus the length of one. This keeps both
/// small for the tens of thousands of marks a large input carries.
pub const PIECE_LEN: usize = 128;

pub trait Spot: Clone {
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

/// Pieces hold about {LEN} items: they split above twice it and merge below
/// half of it.
#[derive(Clone)]
pub struct Chunks<T, const LEN: usize = PIECE_LEN> {
    pieces: Vec<Piece<T>>,
}

impl<T, const LEN: usize> Default for Chunks<T, LEN> {
    fn default() -> Self {
        Self { pieces: Vec::new() }
    }
}

impl<T: Spot, const LEN: usize> Chunks<T, LEN> {
    /// {items} must be sorted by start.
    pub fn from_sorted(items: Vec<T>) -> Self {
        let mut items = items.into_iter();
        let pieces = iter::from_fn(|| {
            let piece: Vec<T> = items.by_ref().take(LEN).collect();
            (!piece.is_empty()).then(|| Piece::new(piece))
        })
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
        let by = new_end.cast_signed() - old_end.cast_signed();
        self.remap(old_end, old_end, new_end, |item| {
            if item.start() >= old_end {
                item.shift(by);
            }
            true
        });
    }

    /// Moves the items through an edit that replaced {start}..{old_end}
    /// with text ending at {new_end}. A piece starting at or after {old_end}
    /// only adds to its shift, so {remap} has to move such an item by what
    /// the edit added too. It sees, with its shift in, every item of a piece
    /// the edit reaches and returns false to take one out. It must keep the
    /// starts in order and leave an item before {start} that does not reach
    /// past it as it is, since pieces of only those are skipped.
    pub fn remap(
        &mut self,
        start: usize,
        old_end: usize,
        new_end: usize,
        mut remap: impl FnMut(&mut T) -> bool,
    ) {
        let by = new_end.cast_signed() - old_end.cast_signed();
        for piece in &mut self.pieces {
            if piece.first >= old_end {
                piece.move_by(by);
            } else if piece.last >= start || piece.end > start {
                piece.items_mut().retain_mut(&mut remap);
                piece.measure();
            }
        }
        self.pieces.retain(|piece| !piece.items.is_empty());
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
        if len > 2 * LEN {
            let tail = self.pieces[at].items_mut().split_off(len / 2);
            self.pieces[at].measure();
            self.pieces.insert(at + 1, Piece::new(tail));
        } else if len < LEN / 2 && at + 1 < self.pieces.len() {
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
pub fn shifted(pos: usize, by: isize) -> usize {
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

    /// An edit whose items take out whole pieces, rewrite the ones it
    /// reaches and only shift the ones after it.
    #[test_case(10  ; "one_piece")]
    #[test_case(500 ; "many_pieces")]
    fn remap_moves_and_drops_like_a_sorted_list(count: usize) {
        const INSERTED: usize = 3;
        let items: Vec<_> = (0..count).map(|at| (at * 10, at * 10 + 5)).collect();
        let mut chunks = Chunks::from_sorted(items.clone());
        let mut plain = items;

        let (start, old_end) = (count * 2 + 3, count * 6 + 3);
        let new_end = start + INSERTED;
        let by = new_end.cast_signed() - old_end.cast_signed();
        let remap = |item: &mut (usize, usize)| {
            if item.0 >= old_end {
                item.shift(by);
                true
            } else {
                item.1 <= start
            }
        };
        chunks.remap(start, old_end, new_end, remap);
        plain.retain_mut(remap);
        assert_eq!(spots(&chunks), plain);

        let (from, to) = (start, new_end + 100);
        let touching: Vec<_> = chunks
            .touching(from, to)
            .map(|(shift, item)| shifted(item.0, shift))
            .collect();
        let expected: Vec<_> = plain
            .iter()
            .filter(|item| item.0 <= to && item.1 >= from)
            .map(|item| item.0)
            .collect();
        assert_eq!(touching, expected);
    }
}
