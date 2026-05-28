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

/// The inner rect left for the chat, plus the rect granted to each carved
/// split. A split that clamped out to nothing simply does not appear, so
/// callers never have to handle an invalid rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitLayout {
    pub inner: Rect,
    rects: [Option<Rect>; 4],
}

impl SplitLayout {
    pub fn rect(&self, split: Split) -> Option<Rect> {
        Split::ALL
            .iter()
            .position(|s| *s == split)
            .and_then(|i| self.rects[i])
    }
}

/// Carve every requested split off `area`, clamped so the interior keeps at
/// least [`MIN_CHAT_COLS`] x [`MIN_CHAT_ROWS`]. [`Split::ALL`] fixes the order
/// (columns before rows) so geometry stays the same no matter what opened first.
pub fn carve(area: Rect, reqs: &[SplitReq]) -> SplitLayout {
    let mut inner = area;
    let mut rects = [None; 4];

    for (i, dir) in Split::ALL.iter().enumerate() {
        let Some(req) = reqs.iter().find(|r| r.split == *dir) else {
            continue;
        };
        let Some(edge) = dir.edge() else {
            continue;
        };

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
        for dir in Split::ALL {
            assert!(layout.rect(dir).is_none(), "{EXPECT_NO_RECT}");
        }
    }

    #[test_case(Split::Above ; "above_reserves_top_rows")]
    #[test_case(Split::Below ; "below_reserves_bottom_rows")]
    fn vertical_split_reserves_rows(dir: Split) {
        let layout = carve(AREA, &[req(dir, 10)]);
        let band = layout.rect(dir).expect("band must exist");
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
        let band = layout.rect(dir).expect("band must exist");
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
        let band = layout.rect(Split::Below).expect("band must exist");
        assert_eq!(layout.inner.height, MIN_CHAT_ROWS);
        assert_eq!(band.height, AREA.height - MIN_CHAT_ROWS);
    }

    #[test]
    fn oversize_column_request_clamps_to_chat_minimum() {
        let layout = carve(AREA, &[req(Split::Left, 1000)]);
        let band = layout.rect(Split::Left).expect("band must exist");
        assert_eq!(layout.inner.width, MIN_CHAT_COLS);
        assert_eq!(band.width, AREA.width - MIN_CHAT_COLS);
    }

    #[test]
    fn zero_extent_request_yields_no_rect() {
        let layout = carve(AREA, &[req(Split::Below, 0)]);
        assert!(layout.rect(Split::Below).is_none(), "{EXPECT_NO_RECT}");
        assert_eq!(layout.inner, AREA, "{EXPECT_INNER}");
    }

    #[test]
    fn request_with_no_room_clamps_to_none() {
        let tight = Rect::new(0, 0, MIN_CHAT_COLS, 40);
        let layout = carve(tight, &[req(Split::Left, 10)]);
        assert!(layout.rect(Split::Left).is_none(), "{EXPECT_NO_RECT}");
        assert_eq!(layout.inner, tight, "{EXPECT_INNER}");
    }

    #[test]
    fn coexisting_left_and_below_shrink_both_axes() {
        let left = carve(AREA, &[req(Split::Left, 20), req(Split::Below, 10)]);
        assert_eq!(left.inner.width, AREA.width - 20);
        assert_eq!(left.inner.height, AREA.height - 10);
        let l = left.rect(Split::Left).expect("left band");
        let b = left.rect(Split::Below).expect("below band");
        assert_eq!(l.width, 20);
        assert_eq!(b.height, 10);
        assert!(
            l.x + l.width <= left.inner.x || b.y >= left.inner.y + left.inner.height,
            "{EXPECT_DISJOINT}",
        );
    }

    #[test]
    fn open_order_does_not_change_geometry() {
        let a = carve(AREA, &[req(Split::Left, 20), req(Split::Below, 10)]);
        let b = carve(AREA, &[req(Split::Below, 10), req(Split::Left, 20)]);
        assert_eq!(
            a, b,
            "carve order is fixed by Split::ALL, not request order"
        );
    }
}
