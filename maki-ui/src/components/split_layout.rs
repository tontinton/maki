use maki_lua::{Axis, Split};
use ratatui::layout::Rect;

/// Minimum interior columns the chat keeps after every column split is carved.
const MIN_CHAT_COLS: u16 = 20;
/// The one floor that protects the chat from vanishing: `carve` clamps row
/// splits against it, and the app clamps the bottom panel against it too, so no
/// feature can shrink the chat below this no matter how it grows.
pub const MIN_CHAT_ROWS: u16 = 2;

/// A split that wants `extent` cells along its edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitReq {
    pub split: Split,
    pub extent: u16,
}

/// The inner rect left for the chat, plus one rect per request in the order
/// they were passed to [`carve`]. A request that clamped out to nothing is
/// `None`, so callers never handle an invalid rect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitLayout {
    pub inner: Rect,
    rects: Vec<Option<Rect>>,
}

impl SplitLayout {
    /// One rect per request, parallel to the slice given to [`carve`].
    pub fn rects(&self) -> &[Option<Rect>] {
        &self.rects
    }
}

/// Carve every request off `area`. Edges are laid out in [`Split::ALL`] order
/// (columns before rows) so geometry is stable no matter what opened first;
/// requests sharing an edge stack in the order they are given, each clamping
/// against the shrinking interior. That clamp keeps at least
/// [`MIN_CHAT_COLS`] x [`MIN_CHAT_ROWS`], so the chat never vanishes.
pub fn carve(area: Rect, reqs: &[SplitReq]) -> SplitLayout {
    let mut inner = area;
    let mut rects = vec![None; reqs.len()];

    for dir in Split::ALL {
        let Some(edge) = dir.edge() else {
            continue;
        };
        for (i, req) in reqs.iter().enumerate() {
            if req.split != dir {
                continue;
            }
            let (available, min) = match edge.axis {
                Axis::Vertical => (inner.height, MIN_CHAT_ROWS),
                Axis::Horizontal => (inner.width, MIN_CHAT_COLS),
            };
            let band = req.extent.min(available.saturating_sub(min));
            if band == 0 {
                continue;
            }
            let (granted, rest) = split_edge(inner, edge.axis, edge.at_start, band);
            rects[i] = Some(granted);
            inner = rest;
        }
    }

    SplitLayout { inner, rects }
}

/// Split `area` into `(band, rest)`. The band takes `extent` cells from the
/// start (top/left) or end (bottom/right) edge along `axis`; `rest` keeps the
/// remainder.
fn split_edge(area: Rect, axis: Axis, at_start: bool, extent: u16) -> (Rect, Rect) {
    match axis {
        Axis::Vertical => {
            if at_start {
                let band = Rect::new(area.x, area.y, area.width, extent);
                let rest = Rect::new(area.x, area.y + extent, area.width, area.height - extent);
                (band, rest)
            } else {
                let rest = Rect::new(area.x, area.y, area.width, area.height - extent);
                let band = Rect::new(area.x, area.y + area.height - extent, area.width, extent);
                (band, rest)
            }
        }
        Axis::Horizontal => {
            if at_start {
                let band = Rect::new(area.x, area.y, extent, area.height);
                let rest = Rect::new(area.x + extent, area.y, area.width - extent, area.height);
                (band, rest)
            } else {
                let rest = Rect::new(area.x, area.y, area.width - extent, area.height);
                let band = Rect::new(area.x + area.width - extent, area.y, extent, area.height);
                (band, rest)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 40,
    };

    const EXPECT_INNER: &str = "inner must equal area when nothing is carved";
    const EXPECT_NO_RECT: &str = "a direction with no request must have no rect";
    const EXPECT_CONSERVED: &str = "band + inner must conserve the carved axis";
    const EXPECT_DISJOINT: &str = "carved bands must not overlap the inner rect";

    fn req(split: Split, extent: u16) -> SplitReq {
        SplitReq { split, extent }
    }

    #[test]
    fn empty_reqs_leave_inner_equal_to_area() {
        let layout = carve(AREA, &[]);
        assert_eq!(layout.inner, AREA, "{EXPECT_INNER}");
        assert!(layout.rects().is_empty());
    }

    #[test_case(Split::Above ; "above_reserves_top_rows")]
    #[test_case(Split::Below ; "below_reserves_bottom_rows")]
    fn vertical_split_reserves_rows(dir: Split) {
        let layout = carve(AREA, &[req(dir, 10)]);
        let band = layout.rects()[0].expect("band must exist");
        assert_eq!(band.height, 10);
        assert_eq!(band.width, AREA.width);
        assert_eq!(layout.inner.height, AREA.height - 10);
        assert_eq!(
            band.height + layout.inner.height,
            AREA.height,
            "{EXPECT_CONSERVED}"
        );
        let at_top = band.y == AREA.y;
        assert_eq!(at_top, dir == Split::Above, "{EXPECT_DISJOINT}");
    }

    #[test_case(Split::Left ; "left_reserves_columns")]
    #[test_case(Split::Right ; "right_reserves_columns")]
    fn horizontal_split_reserves_columns(dir: Split) {
        let layout = carve(AREA, &[req(dir, 30)]);
        let band = layout.rects()[0].expect("band must exist");
        assert_eq!(band.width, 30);
        assert_eq!(band.height, AREA.height);
        assert_eq!(layout.inner.width, AREA.width - 30);
        assert_eq!(
            band.width + layout.inner.width,
            AREA.width,
            "{EXPECT_CONSERVED}"
        );
        let at_left = band.x == AREA.x;
        assert_eq!(at_left, dir == Split::Left, "{EXPECT_DISJOINT}");
    }

    #[test]
    fn oversize_row_request_clamps_to_chat_minimum() {
        let layout = carve(AREA, &[req(Split::Below, 1000)]);
        let band = layout.rects()[0].expect("band must exist");
        assert_eq!(layout.inner.height, MIN_CHAT_ROWS);
        assert_eq!(band.height, AREA.height - MIN_CHAT_ROWS);
    }

    #[test]
    fn oversize_column_request_clamps_to_chat_minimum() {
        let layout = carve(AREA, &[req(Split::Left, 1000)]);
        let band = layout.rects()[0].expect("band must exist");
        assert_eq!(layout.inner.width, MIN_CHAT_COLS);
        assert_eq!(band.width, AREA.width - MIN_CHAT_COLS);
    }

    #[test]
    fn zero_extent_request_yields_no_rect() {
        let layout = carve(AREA, &[req(Split::Below, 0)]);
        assert!(layout.rects()[0].is_none(), "{EXPECT_NO_RECT}");
        assert_eq!(layout.inner, AREA, "{EXPECT_INNER}");
    }

    #[test]
    fn request_with_no_room_clamps_to_none() {
        let tight = Rect::new(0, 0, MIN_CHAT_COLS, 40);
        let layout = carve(tight, &[req(Split::Left, 10)]);
        assert!(layout.rects()[0].is_none(), "{EXPECT_NO_RECT}");
        assert_eq!(layout.inner, tight, "{EXPECT_INNER}");
    }

    #[test]
    fn coexisting_left_and_below_shrink_both_axes() {
        let layout = carve(AREA, &[req(Split::Left, 20), req(Split::Below, 10)]);
        assert_eq!(layout.inner.width, AREA.width - 20);
        assert_eq!(layout.inner.height, AREA.height - 10);
        let l = layout.rects()[0].expect("left band");
        let b = layout.rects()[1].expect("below band");
        assert_eq!(l.width, 20);
        assert_eq!(b.height, 10);
        assert!(
            l.x + l.width <= layout.inner.x || b.y >= layout.inner.y + layout.inner.height,
            "{EXPECT_DISJOINT}",
        );
    }

    /// Two requests on one edge stack: the first takes the screen edge, the
    /// second sits just inside it, and both extents are reserved.
    #[test]
    fn same_edge_requests_stack_in_request_order() {
        let layout = carve(AREA, &[req(Split::Below, 4), req(Split::Below, 6)]);
        let first = layout.rects()[0].expect("first band");
        let second = layout.rects()[1].expect("second band");
        assert_eq!(first.height, 4);
        assert_eq!(second.height, 6);
        assert_eq!(first.y + first.height, AREA.height, "first hugs the bottom");
        assert_eq!(
            second.y + second.height,
            first.y,
            "second sits above the first"
        );
        assert_eq!(layout.inner.height, AREA.height - 10);
    }

    /// Edge order is fixed by `Split::ALL`, so request order only changes which
    /// rect slot a band lands in, never the geometry.
    #[test]
    fn edge_order_is_independent_of_request_order() {
        let a = carve(AREA, &[req(Split::Left, 20), req(Split::Below, 10)]);
        let b = carve(AREA, &[req(Split::Below, 10), req(Split::Left, 20)]);
        assert_eq!(a.inner, b.inner);
        assert_eq!(
            a.rects()[0],
            b.rects()[1],
            "left band lands the same either way"
        );
        assert_eq!(
            a.rects()[1],
            b.rects()[0],
            "below band lands the same either way"
        );
    }
}
