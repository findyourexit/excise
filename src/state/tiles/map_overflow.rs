/// Entries the map has no room to draw at this size, summarised for the reader.
///
/// This is a rendering limit, not a model limit: every entry counted here is a
/// real, individually tracked node that a larger pane or a drill would draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapOverflow {
    /// Terminal column where the overflow region starts when one is drawable.
    ///
    /// A zero-sized layout still reports its accounting summary; then this and
    /// `y` identify the logical boundary rather than a paintable cell.
    pub x: u16,
    /// Half-row the overflow region starts at (see `HALF_ROWS_PER_CELL`).
    ///
    /// This is `u32` because a public terminal `Rect` accepts `u16` origins
    /// and extents independently, so its half-row endpoint can exceed `u16`.
    pub y: u32,
    /// How many entries the region stands for.
    pub entries: usize,
    /// Bytes those entries account for, on the same basis as the drawn tiles.
    pub bytes: u128,
    /// Whether the byte total is a lower bound because omitted metadata is incomplete.
    pub uncertain: bool,
}
