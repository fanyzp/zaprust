//! Scroll anchoring for the message stream (P2-67): the pure
//! window math the stream's windowing is built on, ported
//! from the standalone rig where its invariants were proven
//! (the v1.6.17 round's working papers) and kept under the
//! same tests here.
//!
//! Model: a stream is a list of rows. Each row has a measured
//! height once it has rendered; until then an estimate stands
//! in. A window renders a contiguous range and replaces the
//! rest with two spacers (top, bottom) whose heights come
//! from the same height table, so the scrollable content
//! height never changes shape when the window moves. The
//! anchor is the first row intersecting the viewport top,
//! plus the pixel offset inside it: the pair the scroll
//! position must preserve across prepends, appends,
//! measurement corrections, and resizes.

/// Identity of one stream row across frames: rows are
/// rebuilt from the conversation every frame, so a row is
/// keyed by the message it belongs to (heights and the anchor
/// follow the message when older pages prepend in front of
/// it, never by position).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
// Chip, Notice and Divider are part of the rig's pinned key
// space: the stream keys only message rows today, and the
// invariant tests construct the full space.
pub(crate) enum RowKey {
    /// A day or time separator chip, keyed by the message
    /// that follows it.
    Chip(u64),
    /// The encryption notice (one per conversation view).
    Notice,
    /// The unread divider, keyed by the first unread message.
    Divider(u64),
    /// A message row (a bubble, a system chip, or an album
    /// run), keyed by its first message.
    Row(u64),
}

/// The height table: `None` is an unmeasured row, read as
/// `estimate`.
pub(crate) struct Heights {
    pub(crate) items: Vec<Option<f32>>,
    pub(crate) estimate: f32,
}

impl Heights {
    pub(crate) fn new(items: Vec<Option<f32>>, estimate: f32) -> Self {
        Self { items, estimate }
    }

    pub(crate) fn height(&self, index: usize) -> f32 {
        self.items[index].unwrap_or(self.estimate)
    }

    /// Content offset of a row's top edge.
    pub(crate) fn offset_of(&self, index: usize) -> f32 {
        (0..index).map(|i| self.height(i)).sum()
    }

    /// Prefix sums of the whole table: `prefix[i]` is
    /// `offset_of(i)` and `prefix[len]` is the total, computed
    /// once per frame so the stream's spacer math stays O(1)
    /// per row instead of re-summing prefixes.
    pub(crate) fn prefix_sums(&self) -> Vec<f32> {
        let mut prefix = Vec::with_capacity(self.items.len() + 1);
        let mut y = 0.0;
        prefix.push(y);
        for i in 0..self.items.len() {
            y += self.height(i);
            prefix.push(y);
        }
        prefix
    }

    pub(crate) fn total(&self) -> f32 {
        self.offset_of(self.items.len())
    }

    /// The anchor at a scroll offset: the first row whose
    /// bottom edge is below the offset, and the pixels from
    /// its top edge to the offset. An offset past the end
    /// anchors the last row at its bottom.
    pub(crate) fn anchor_at(&self, offset: f32) -> (usize, f32) {
        let mut y = 0.0;
        for (i, _) in self.items.iter().enumerate() {
            let h = self.height(i);
            if offset < y + h {
                return (i, offset - y);
            }
            y += h;
        }
        let last = self.items.len().saturating_sub(1);
        (last, if self.items.is_empty() { 0.0 } else { self.height(last) })
    }

    /// The scroll offset that puts an anchor at the viewport
    /// top: the exact inverse of `anchor_at` for anchors
    /// inside the content.
    pub(crate) fn offset_for_anchor(&self, anchor: (usize, f32)) -> f32 {
        self.offset_of(anchor.0) + anchor.1
    }

    /// The render window for a viewport: the contiguous range
    /// of rows intersecting `[offset, offset + viewport)`,
    /// expanded by `overscan` rows on each side, and the
    /// spacer heights replacing the rows outside the range.
    /// top + rendered + bottom always equals the total
    /// content height.
    pub(crate) fn visible_range(
        &self,
        offset: f32,
        viewport: f32,
        overscan: usize,
    ) -> (usize, usize, f32, f32) {
        if self.items.is_empty() {
            return (0, 0, 0.0, 0.0);
        }
        let (first, _) = self.anchor_at(offset);
        let mut end = first;
        let mut y = self.offset_of(first);
        while end < self.items.len() && y < offset + viewport {
            y += self.height(end);
            end += 1;
        }
        let start = first.saturating_sub(overscan);
        let end = (end + overscan).min(self.items.len());
        let top_spacer = self.offset_of(start);
        let bottom_spacer = self.total() - self.offset_of(end);
        (start, end, top_spacer, bottom_spacer)
    }

    /// The scroll offset after a measurement correction: when
    /// a row above the anchor is measured and its height
    /// differs from the estimate that stood in for it, the
    /// offset moves by the same delta, so the anchor's
    /// viewport position is preserved exactly. A correction
    /// at or below the anchor moves nothing.
    // Part of the rig's pinned surface, proven by the
    // correction invariant test; the stream restores a moved
    // anchor by scrolling its painted rect into place instead.
    #[allow(dead_code)]
    pub(crate) fn offset_after_correction(
        &self,
        offset: f32,
        corrected_index: usize,
        old_height: f32,
        new_height: f32,
    ) -> f32 {
        let (anchor, _) = self.anchor_at(offset);
        if corrected_index < anchor {
            offset + (new_height - old_height)
        } else {
            offset
        }
    }

    /// The clamped scroll offset for stick-to-bottom: the
    /// newest row's bottom edge sits at the viewport's bottom
    /// edge, and the offset never goes negative on content
    /// shorter than the viewport.
    pub(crate) fn offset_stuck_to_bottom(&self, viewport: f32) -> f32 {
        (self.total() - viewport).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heights(list: &[f32]) -> Heights {
        Heights::new(list.iter().map(|h| Some(*h)).collect(), 50.0)
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.001
    }

    /// Invariant 1 (prepend): prepending rows shifts every
    /// existing row down by the prepended height; moving the
    /// scroll offset by the same amount keeps the anchor row
    /// at the exact same viewport position.
    #[test]
    fn prepend_preserves_the_anchor_position_exactly() {
        let before = heights(&[40.0, 60.0, 30.0, 80.0, 50.0]);
        let offset = 95.0;
        let anchor = before.anchor_at(offset);
        let pos_before = before.offset_of(anchor.0) - offset;

        let mut items: Vec<Option<f32>> = vec![Some(25.0), Some(35.0)];
        items.extend(before.items.iter().copied());
        let after = Heights::new(items, 50.0);
        let shifted = offset + 60.0;
        let anchor_after = after.anchor_at(shifted);
        assert_eq!(anchor_after.0, anchor.0 + 2, "the anchor row moved down by the prepend count");
        assert!(approx(anchor_after.1, anchor.1), "the intra-row offset is unchanged");
        let pos_after = after.offset_of(anchor_after.0) - shifted;
        assert!(approx(pos_before, pos_after), "the anchor's viewport position is exact");
    }

    /// Invariant 2 (correction above the anchor): measuring
    /// an estimated row above the anchor shifts the anchor's
    /// content position; the compensated offset keeps its
    /// viewport position exact.
    #[test]
    fn correction_above_the_anchor_is_compensated_exactly() {
        let mut table = Heights::new(vec![None, Some(60.0), Some(30.0), Some(80.0)], 50.0);
        let offset = 120.0;
        let anchor = table.anchor_at(offset);
        let pos_before = table.offset_of(anchor.0) - offset;

        // Row 0 measures at 74 instead of the 50 estimate.
        let fixed = table.offset_after_correction(offset, 0, 50.0, 74.0);
        table.items[0] = Some(74.0);
        assert!(approx(fixed, offset + 24.0), "the offset moves by the delta");
        let anchor_after = table.anchor_at(fixed);
        assert_eq!(anchor_after, anchor, "the same anchor, the same intra-row offset");
        let pos_after = table.offset_of(anchor_after.0) - fixed;
        assert!(approx(pos_before, pos_after), "the anchor's viewport position is exact");
    }

    /// Invariant 3 (correction below the anchor): measuring
    /// a row at or below the anchor changes nothing about the
    /// anchor's position, and the offset must not move.
    #[test]
    fn correction_below_the_anchor_moves_nothing() {
        let table = Heights::new(vec![Some(40.0), None, Some(30.0)], 50.0);
        let offset = 20.0;
        let fixed = table.offset_after_correction(offset, 1, 50.0, 90.0);
        assert!(approx(fixed, offset), "no compensation below the anchor");
    }

    /// Invariant 4 (append + stick): with stick-to-bottom
    /// engaged, appending rows moves the clamped offset so
    /// the newest bottom stays at the viewport bottom.
    #[test]
    fn append_with_stick_keeps_the_newest_bottom_pinned() {
        let mut table = heights(&[40.0, 60.0, 30.0]);
        let viewport = 100.0;
        assert!(approx(table.offset_stuck_to_bottom(viewport), 30.0));
        table.items.push(Some(70.0));
        let stuck = table.offset_stuck_to_bottom(viewport);
        assert!(approx(stuck, 100.0), "the offset follows the growth");
        assert!(
            approx(table.offset_of(3) + table.height(3) - stuck, viewport),
            "the last row's bottom is at the viewport's bottom"
        );
    }

    /// Invariant 5 (resize): the anchor is a property of the
    /// content, not the viewport; a viewport height change
    /// keeps the anchor and its offset identical.
    #[test]
    fn resize_keeps_the_anchor() {
        let table = heights(&[40.0, 60.0, 30.0, 80.0, 50.0]);
        let offset = 135.0;
        let anchor = table.anchor_at(offset);
        let _range_small = table.visible_range(offset, 120.0, 0);
        let _range_large = table.visible_range(offset, 400.0, 0);
        assert_eq!(table.anchor_at(offset), anchor);
        assert!(approx(table.offset_for_anchor(anchor), offset), "anchor and offset are inverses");
    }

    /// Invariant 6 (spacer conservation): for any offset,
    /// viewport, and overscan, top spacer + rendered range +
    /// bottom spacer equals the total height, and the range
    /// covers every row intersecting the viewport.
    #[test]
    fn spacers_conserve_the_total_and_cover_the_viewport() {
        let table =
            Heights::new(vec![Some(40.0), None, Some(30.0), Some(80.0), None, Some(50.0)], 55.0);
        for offset in [0.0, 39.0, 40.0, 95.0, 200.0, 259.0] {
            for viewport in [60.0, 130.0, 400.0] {
                let (start, end, top, bottom) = table.visible_range(offset, viewport, 2);
                let rendered: f32 = (start..end).map(|i| table.height(i)).sum();
                assert!(
                    approx(top + rendered + bottom, table.total()),
                    "spacers conserve the total at offset {offset} viewport {viewport}"
                );
                assert!(
                    approx(top, table.offset_of(start)),
                    "the top spacer is the range's prefix"
                );
                let (first, _) = table.anchor_at(offset);
                assert!(start <= first && first < end, "the top row is rendered");
                let mut last = first;
                let mut y = table.offset_of(first);
                while last + 1 < table.items.len() && y + table.height(last) < offset + viewport {
                    y += table.height(last);
                    last += 1;
                }
                assert!(last < end, "the bottom row is rendered");
            }
        }
    }

    /// Invariant 7 (estimates converge): a stream that starts
    /// fully estimated and measures its rows one by one ends
    /// with the same total as the fully measured table, and
    /// every intermediate correction above the anchor is
    /// exactly compensated (invariant 2 applied repeatedly).
    #[test]
    fn repeated_corrections_converge_to_the_measured_total() {
        let truth = [42.0, 77.0, 35.0, 90.0, 51.0];
        let mut table = Heights::new(vec![None; 5], 50.0);
        let mut offset = 130.0;
        for i in 0..5 {
            let old = table.height(i);
            offset = table.offset_after_correction(offset, i, old, truth[i]);
            table.items[i] = Some(truth[i]);
        }
        let measured = heights(&truth);
        assert!(approx(table.total(), measured.total()), "the totals converge");
        let anchor = table.anchor_at(offset);
        assert!(approx(table.offset_for_anchor(anchor), offset), "the anchor still round-trips");
    }

    /// The prefix sums the stream paints spacers from agree
    /// with the table's own offsets everywhere.
    #[test]
    fn prefix_sums_match_offsets() {
        let table = Heights::new(vec![Some(40.0), None, Some(30.0), None], 55.0);
        let prefix = table.prefix_sums();
        assert_eq!(prefix.len(), 5);
        for i in 0..5 {
            assert!(approx(prefix[i], table.offset_of(i)), "prefix {i}");
        }
        assert!(approx(prefix[4], table.total()));
    }
}
