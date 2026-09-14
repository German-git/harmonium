//! Browser panel state with pure cursor, paging and selection transitions.
//!
//! All transitions keep the invariants that `cursor` and `scroll_offset`
//! stay inside the entry list bounds and that the cursor is always visible
//! inside the `[scroll_offset, scroll_offset + viewport_height)` window.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::filesystem::{EntryKind, FileEntry, is_supported_audio};

/// Viewport assumed until the renderer measures the real panel height.
pub const DEFAULT_VIEWPORT_HEIGHT: u16 = 12;

/// Reserved rows of breathing room between the cursor and the edge it is
/// approaching, so the user always sees at least that many entry rows beyond
/// the highlight while the list still has more content in that direction.
///
/// Applied to both list panels. The value mirrors the requested UX: two rows
/// before the visible boundary.
pub const SCROLL_CONTEXT_ROWS: usize = 2;

/// Direction of the last cursor move, used to keep the scroll window anchored
/// to the edge being approached instead of always hugging one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    /// Cursor moved toward the head of the list.
    Up,
    /// Cursor moved toward the tail of the list.
    Down,
}

/// State of the file browser panel.
#[derive(Debug)]
pub struct BrowserState {
    /// Directory the browser opened with at startup.
    pub start_dir: PathBuf,
    /// Directory currently listed.
    pub current_dir: PathBuf,
    /// Sorted listing of `current_dir`.
    pub entries: Vec<FileEntry>,
    /// Highlighted row, always a valid index while entries exist.
    ///
    /// Private: mutating it directly would bypass the re-anchoring that keeps
    /// the cursor inside the entry bounds and inside the visible window. Go
    /// through [`Self::set_cursor`], which enforces both, or the directional
    /// helpers (`move_up`/`move_down`/`page_*`).
    cursor: usize,
    /// First visible row index so scrolling stays smooth across renders.
    ///
    /// Private for the same reason as [`Self::cursor`]: it is derived from the
    /// cursor and the viewport by the scroll helpers, never set by hand.
    scroll_offset: usize,
    /// Visible row count refreshed by the explicit frame tick.
    pub viewport_height: u16,
    /// Stack of `(parent directory, folder name)` frames recorded while
    /// descending, so ascending always lands the cursor on the folder that was
    /// just left, at every level of nesting.
    pub focus_stack: Vec<(PathBuf, String)>,
}

impl BrowserState {
    /// Create an empty browser pointing at `start_dir`.
    ///
    /// The contents are loaded by [`crate::state::AppState::change_browser_dir`]
    /// once the application can touch the filesystem.
    pub fn new(start_dir: PathBuf) -> Self {
        Self {
            start_dir,
            current_dir: PathBuf::from("."),
            entries: Vec::new(),
            cursor: 0,
            scroll_offset: 0,
            viewport_height: DEFAULT_VIEWPORT_HEIGHT,
            focus_stack: Vec::new(),
        }
    }

    /// Highlighted row index.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// First visible row index.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Swap in a fresh directory listing and reset navigation to the top.
    ///
    /// The focus stack is NOT consumed here: it survives the listing of the
    /// directory being entered and is only applied when the caller ascends
    /// back up (see [`BrowserState::pop_focus`]).
    pub fn replace_contents(&mut self, dir: PathBuf, entries: Vec<FileEntry>) {
        self.current_dir = dir;
        self.entries = entries;
        self.cursor = 0;
        self.scroll_offset = 0;
    }

    /// Remember where we are (parent + folder) before descending into a
    /// subdirectory, so ascending can restore the cursor predictably.
    pub fn push_focus(&mut self, parent_dir: PathBuf, folder_name: String) {
        self.focus_stack.push((parent_dir, folder_name));
    }

    /// Move the cursor onto the folder we just ascended from, clearing that
    /// frame. No-op when the caller is not actually returning to the recorded
    /// parent (e.g. a jump) or the entry is no longer present.
    pub fn pop_focus(&mut self, parent_dir: &Path) {
        let Some((recorded_parent, folder)) = self.focus_stack.pop() else {
            return;
        };
        if recorded_parent != parent_dir {
            // We are not returning to the recorded parent: keep the frame so
            // pressing up again can still restore it.
            self.focus_stack.push((recorded_parent, folder));
            return;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.name == folder) {
            self.set_cursor(index);
        }
    }

    /// Place the cursor at `index` and re-anchor the scroll window so the
    /// highlighted row is always visible.
    ///
    /// Everything that relocates the cursor away from the normal up/down
    /// movement (restoring a folder name after ascending, jumping from a
    /// search result) must go through this so the documented invariant — the
    /// cursor is always inside `[scroll_offset, scroll_offset + viewport)` —
    /// holds even when the target index is far past the current viewport.
    pub fn set_cursor(&mut self, index: usize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            self.scroll_offset = 0;
            return;
        }
        self.cursor = index.min(self.entries.len() - 1);
        // Re-anchor rather than enforcing a direction: the target may be above
        // or below the current window, so chase whichever edge it approaches.
        let visible = usize::from(self.viewport_height.max(1));
        self.scroll_offset = scroll_offset_for_direction(
            self.scroll_offset,
            self.cursor,
            self.entries.len(),
            visible,
            SCROLL_CONTEXT_ROWS,
            ScrollDirection::Down,
        );
        // `scroll_offset_for_direction(Down)` never recedes, so after a large
        // jump upward the raw safety net already clamps it; recompute against
        // the current offset to honor the Up contraction when needed.
        self.scroll_offset = scroll_offset_for_direction(
            self.scroll_offset,
            self.cursor,
            self.entries.len(),
            visible,
            SCROLL_CONTEXT_ROWS,
            ScrollDirection::Up,
        );
    }

    /// Store the measured visible row count, never below one.
    pub fn set_viewport_height(&mut self, height: u16) {
        self.viewport_height = height.max(1);
    }

    /// Move the cursor up, clamped at the top boundary.
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.enforce_scroll(ScrollDirection::Up);
    }

    /// Move the cursor down, clamped at the last entry.
    pub fn move_down(&mut self) {
        if !self.entries.is_empty() {
            self.cursor = (self.cursor + 1).min(self.entries.len() - 1);
        }
        self.enforce_scroll(ScrollDirection::Down);
    }

    /// Jump to the first entry.
    pub fn goto_top(&mut self) {
        self.cursor = 0;
        self.enforce_scroll(ScrollDirection::Up);
    }

    /// Jump to the last entry, or stay put on an empty directory.
    pub fn goto_bottom(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
        self.enforce_scroll(ScrollDirection::Down);
    }

    /// Page up by one full viewport minus one overlap row.
    pub fn page_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(self.page_step());
        self.enforce_scroll(ScrollDirection::Up);
    }

    /// Page down by one full viewport minus one overlap row.
    pub fn page_down(&mut self) {
        if !self.entries.is_empty() {
            let target = self.cursor + self.page_step();
            self.cursor = target.min(self.entries.len() - 1);
        }
        self.enforce_scroll(ScrollDirection::Down);
    }

    /// Resolve the directory to open when activating the cursor entry.
    ///
    /// Only directory entries qualify. Symbolic links return their path for
    /// explicit worker-side resolution; scans spawned later still ignore
    /// inner links.
    pub fn enter_selected(&self) -> Result<PathBuf> {
        let Some(entry) = self.entries.get(self.cursor) else {
            return Err(crate::error::HarmoniumError::io(
                self.current_dir.clone(),
                std::io::Error::new(ErrorKind::NotFound, "browser cursor points at no entry"),
            ));
        };

        match entry.kind {
            EntryKind::File => Err(crate::error::HarmoniumError::io(
                entry.path.clone(),
                std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "regular files cannot be entered, use add instead",
                ),
            )),
            EntryKind::Dir => Ok(entry.path.clone()),
            EntryKind::Symlink => Ok(entry.path.clone()),
        }
    }

    /// Parent of the current directory, or none when already at a root or
    /// relative placeholder where climbing makes no sense.
    pub fn go_parent(&self) -> Option<PathBuf> {
        let parent = self.current_dir.parent()?.to_path_buf();
        (parent != self.current_dir && !parent.as_os_str().is_empty()).then_some(parent)
    }

    /// Toggle the mark on the cursor entry inside the shared mark set.
    ///
    /// No-op on an empty directory. Marked paths may belong to files or
    /// directories, directories are expanded recursively when added.
    pub fn toggle_mark(&self, selected: &mut HashSet<PathBuf>) {
        if let Some(entry) = self.entries.get(self.cursor)
            && !selected.remove(&entry.path)
        {
            selected.insert(entry.path.clone());
        }
    }

    /// Consume the selection for queueing: every marked entry in current
    /// display order, falling back to the cursor entry when nothing is
    /// marked. Unsupported regular files are dropped because they could not
    /// be played anyway, while directories pass through for recursive scans.
    ///
    /// Marks are cleared as part of taking them, which prevents accidental
    /// duplicate adds and matches the consumption implied by the name.
    pub fn take_marked_or_cursor(&self, selected: &mut HashSet<PathBuf>) -> Vec<FileEntry> {
        let taken = if selected.is_empty() {
            self.cursor_add_candidates()
        } else {
            self.entries
                .iter()
                .filter(|entry| selected.contains(&entry.path))
                .filter(|entry| match entry.kind {
                    EntryKind::File => is_supported_audio(&entry.path),
                    EntryKind::Dir | EntryKind::Symlink => true,
                })
                .cloned()
                .collect()
        };

        selected.clear();
        taken
    }

    /// Cursor based fallback used when no marks exist.
    fn cursor_add_candidates(&self) -> Vec<FileEntry> {
        match self.entries.get(self.cursor) {
            Some(entry) => match entry.kind {
                EntryKind::Dir | EntryKind::Symlink => vec![entry.clone()],
                EntryKind::File if is_supported_audio(&entry.path) => vec![entry.clone()],
                EntryKind::File => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    /// Rows per paging action: a full viewport minus one overlap row, never
    /// below one so degenerate viewports still move somewhere.
    fn page_step(&self) -> usize {
        self.viewport_height.saturating_sub(1).max(1) as usize
    }

    /// Re-anchor the scroll window so the cursor stays visible with
    /// [`SCROLL_CONTEXT_ROWS`] of breathing room toward the edge being
    /// approached, while more entries exist in that direction.
    ///
    /// The visible window is the half-open range
    /// `[scroll_offset, scroll_offset + viewport_height)`.
    fn enforce_scroll(&mut self, direction: ScrollDirection) {
        let visible = usize::from(self.viewport_height.max(1));
        self.scroll_offset = scroll_offset_for_direction(
            self.scroll_offset,
            self.cursor,
            self.entries.len(),
            visible,
            SCROLL_CONTEXT_ROWS,
            direction,
        );
    }
}

/// First visible item index for a list with the given cursor, keeping the
/// highlighted row inside the viewport and reserving [`SCROLL_CONTEXT_ROWS`]
/// of breathing room on the edge being approached.
///
/// The scroll window chases the edge the cursor is approaching, starting from
/// the previous offset, so scrolling feels symmetric instead of always
/// hugging one side:
///
/// - `Down`: the window advances so the cursor rests `context` rows above the
///   bottom edge while the list has more content below, revealing the next
///   rows as the user moves toward the tail. It never recedes.
/// - `Up`: the window recedes so the cursor rests `context` rows below the
///   top edge while content remains above, letting the cursor climb toward
///   the head instead of sticking to the low bottom anchor. It never
///   advances.
///
/// In both directions the cursor is kept inside `[offset, offset + viewport)`
/// and the result is clamped to `[0, len - viewport]`. Empty lists return zero.
pub fn scroll_offset_for_direction(
    prev_offset: usize,
    cursor: usize,
    len: usize,
    viewport: usize,
    context: usize,
    direction: ScrollDirection,
) -> usize {
    if len == 0 {
        return 0;
    }
    let visible = viewport.max(1);
    let max_offset = len.saturating_sub(visible);
    let mut offset = prev_offset.min(max_offset);

    // Clamp the breathing room so a tiny panel never yields a negative
    // offset and a huge context never exceeds the visible area.
    let context = context.min(visible.saturating_sub(1));

    match direction {
        // Chase the bottom edge: advance the window (never recede) so the
        // cursor keeps `context` rows of the next entries visible below it.
        ScrollDirection::Down => {
            offset = offset.max(cursor.saturating_add(context + 1).saturating_sub(visible));
        }
        // Chase the top edge: recede the window (never advance) so the
        // cursor keeps `context` rows of the previous entries visible above.
        ScrollDirection::Up => {
            offset = offset.min(cursor.saturating_sub(context));
        }
    }

    clamp_scroll_offset(offset, cursor, len, visible)
}

/// Clamp a scroll window to the list bounds while keeping its cursor visible.
///
/// This is the non-directional normalization used when geometry changes. The
/// directional helper above adds its anchoring policy first, then delegates to
/// this function for the shared safety invariant.
pub(crate) fn clamp_scroll_offset(
    prev_offset: usize,
    cursor: usize,
    len: usize,
    viewport: usize,
) -> usize {
    if len == 0 {
        return 0;
    }
    let visible = viewport.max(1);
    let max_offset = len.saturating_sub(visible);
    let mut offset = prev_offset.min(max_offset);

    // Safety net: the cursor must always stay inside the window.
    if cursor < offset {
        offset = cursor;
    }
    if cursor >= offset.saturating_add(visible) {
        offset = cursor.saturating_add(1).saturating_sub(visible);
    }

    offset.min(max_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry::new(name, PathBuf::from("/fixture").join(name), kind)
    }

    /// Browser with ten plain files and a fixed comfortable viewport.
    fn populated_browser() -> BrowserState {
        let mut state = BrowserState::new(PathBuf::from("/fixture"));
        state.replace_contents(
            PathBuf::from("/fixture"),
            (0..10)
                .map(|index| entry(&format!("f{index}"), EntryKind::File))
                .collect(),
        );
        state.set_viewport_height(5);
        state
    }

    #[test]
    fn cursor_movement_respects_both_boundaries() {
        let mut state = populated_browser();

        for _ in 0..15 {
            state.move_up();
        }
        assert_eq!(state.cursor, 0);

        for _ in 0..25 {
            state.move_down();
        }
        assert_eq!(state.cursor, 9);
    }

    #[test]
    fn shared_scroll_window_handles_empty_single_exact_and_boundary_lists() {
        assert_eq!(clamp_scroll_offset(9, 0, 0, 4), 0);
        assert_eq!(clamp_scroll_offset(9, 0, 1, 4), 0);
        assert_eq!(clamp_scroll_offset(9, 0, 4, 4), 0);
        assert_eq!(clamp_scroll_offset(0, 0, 5, 4), 0);
        assert_eq!(clamp_scroll_offset(0, 4, 5, 4), 1);
        assert_eq!(clamp_scroll_offset(0, usize::MAX, 5, 4), 1);
        assert_eq!(clamp_scroll_offset(usize::MAX, 0, 5, 4), 0);
    }

    #[test]
    fn set_cursor_keeps_the_highlight_inside_the_viewport() {
        // The regression: after ascending, the cursor lands on the folder
        // that was just left by name. If that folder sits past the viewport
        // (e.g. index 8 in a 5-row window), a direct `cursor = index` left the
        // highlight below `scroll_offset + viewport`, invisible until the next
        // keypress. `set_cursor` must re-anchor the scroll window.
        let mut state = populated_browser();

        state.set_cursor(8);
        assert_eq!(state.cursor, 8);
        let visible = usize::from(state.viewport_height);
        assert!(
            state.cursor >= state.scroll_offset && state.cursor < state.scroll_offset + visible,
            "cursor {} must be inside [{}, {})",
            state.cursor,
            state.scroll_offset,
            state.scroll_offset + visible
        );
    }

    #[test]
    fn set_cursor_on_empty_list_keeps_both_anchors_at_zero() {
        let mut state = BrowserState::new(PathBuf::from("/empty"));
        state.set_cursor(3);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn pop_focus_restores_the_folder_with_a_visible_cursor() {
        let mut state = populated_browser();
        // Simulate descending then ascending to a folder at the tail.
        state.replace_contents(
            PathBuf::from("/parent"),
            (0..10)
                .map(|index| entry(&format!("folder{index}"), EntryKind::Dir))
                .collect(),
        );
        state.set_viewport_height(5);
        state.push_focus(PathBuf::from("/parent"), "folder7".to_string());

        state.pop_focus(&PathBuf::from("/parent"));
        let visible = usize::from(state.viewport_height);
        assert_eq!(state.cursor, 7);
        assert!(
            state.cursor >= state.scroll_offset && state.cursor < state.scroll_offset + visible,
            "cursor {} must stay visible after pop_focus",
            state.cursor
        );
    }

    #[test]
    fn empty_directories_keep_every_movement_a_safe_no_op() {
        let mut state = BrowserState::new(PathBuf::from("/empty"));

        state.move_up();
        state.move_down();
        state.goto_bottom();
        state.page_down();
        state.page_up();

        assert_eq!(state.cursor, 0);

        let mut marks = HashSet::new();
        state.toggle_mark(&mut marks);
        assert!(marks.is_empty());
        assert!(state.take_marked_or_cursor(&mut marks).is_empty());
    }

    #[test]
    fn goto_jumps_to_both_extremes() {
        let mut state = populated_browser();

        state.goto_bottom();
        assert_eq!(state.cursor, 9);

        state.goto_top();
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn paging_moves_by_viewport_minus_one_overlap_row() {
        let mut state = populated_browser();

        state.page_down();
        assert_eq!(state.cursor, 4);
        state.page_down();
        assert_eq!(state.cursor, 8);
        state.page_down();
        // Clamped at the last entry regardless of overshoot
        assert_eq!(state.cursor, 9);

        state.page_up();
        assert_eq!(state.cursor, 5);
        state.page_up();
        state.page_up();
        state.page_up();
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn degenerate_viewport_heights_still_page_one_row() {
        for height in [0_u16, 1] {
            let mut state = populated_browser();
            state.set_viewport_height(height);
            assert_eq!(state.viewport_height, 1);

            state.goto_top();
            state.page_down();
            assert_eq!(state.cursor, 1, "viewport {height} must step one row");

            state.page_up();
            assert_eq!(state.cursor, 0, "viewport {height} must step one row");
        }
    }

    #[test]
    fn scrolling_window_keeps_the_cursor_visible_half_open() {
        let mut state = populated_browser();

        state.goto_bottom();
        // Window is [offset, offset + 5), cursor 9 forces offset 5
        assert_eq!(state.scroll_offset, 5);

        state.goto_top();
        assert_eq!(state.scroll_offset, 0);

        state.move_down();
        state.page_down();
        state.page_down();
        assert_eq!(state.cursor, 9);
        assert!(state.scroll_offset + 5 > state.cursor);
    }

    #[test]
    fn parent_navigation_stops_at_the_filesystem_root() {
        let mut state = BrowserState::new(PathBuf::from("/etc"));
        state.replace_contents(PathBuf::from("/"), Vec::new());

        assert_eq!(state.go_parent(), None);

        let mut nested = BrowserState::new(PathBuf::from("/"));
        nested.replace_contents(PathBuf::from("/music/flac"), Vec::new());

        assert_eq!(nested.go_parent(), Some(PathBuf::from("/music")));
    }

    #[test]
    fn entering_a_plain_directory_reports_its_path_without_io() {
        let mut state = BrowserState::new(PathBuf::from("/fixture"));
        state.replace_contents(
            PathBuf::from("/fixture"),
            vec![entry("Album", EntryKind::Dir)],
        );

        let opened = state.enter_selected().expect("directory enters");

        assert_eq!(opened, PathBuf::from("/fixture/Album"));
    }

    #[test]
    fn entering_files_or_empty_rows_is_rejected() {
        let mut state = BrowserState::new(PathBuf::from("/fixture"));
        state.replace_contents(
            PathBuf::from("/fixture"),
            vec![entry("song.mp3", EntryKind::File)],
        );
        assert!(state.enter_selected().is_err());

        let mut empty = BrowserState::new(PathBuf::from("/fixture"));
        empty.replace_contents(PathBuf::from("/fixture"), Vec::new());
        assert!(empty.enter_selected().is_err());
    }

    #[test]
    fn entering_a_symlinked_directory_returns_its_path_for_worker_resolution() {
        let root = crate::test_support::unique_temp_dir("enter-link");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).expect("dir");
        let link = root.join("link");
        if std::os::unix::fs::symlink(&sub, &link).is_err() {
            eprintln!("skipping symlink enter test: links unavailable here");
            return;
        }

        let mut state = BrowserState::new(root.to_path_buf());
        state.replace_contents(
            root.to_path_buf(),
            vec![FileEntry::new("link", link.clone(), EntryKind::Symlink)],
        );

        let opened = state.enter_selected().expect("symlink to dir enters");
        assert_eq!(opened, link);
    }

    #[test]
    fn toggling_marks_flips_membership_for_the_cursor_entry() {
        let state = populated_browser();
        let mut marks = HashSet::new();

        state.toggle_mark(&mut marks);
        assert_eq!(marks.len(), 1);

        state.toggle_mark(&mut marks);
        assert!(marks.is_empty());
    }

    #[test]
    fn down_scroll_reserves_two_rows_below_the_cursor_while_content_remains() {
        // Twenty entries in a five row viewport, moving Down. The cursor
        // keeps two rows of breathing room below it: offset = cursor - (viewport - 1 - context).
        assert_eq!(
            scroll_offset_for_direction(0, 0, 20, 5, 2, ScrollDirection::Down),
            0
        );
        assert_eq!(
            scroll_offset_for_direction(0, 2, 20, 5, 2, ScrollDirection::Down),
            0
        );
        assert_eq!(
            scroll_offset_for_direction(0, 3, 20, 5, 2, ScrollDirection::Down),
            1
        );
        assert_eq!(
            scroll_offset_for_direction(0, 5, 20, 5, 2, ScrollDirection::Down),
            3
        );
        assert_eq!(
            scroll_offset_for_direction(0, 17, 20, 5, 2, ScrollDirection::Down),
            15
        );
    }

    #[test]
    fn down_scroll_pins_to_the_tail_when_content_is_exhausted() {
        // Cursor at the very last row has nothing below it, so the window
        // clamps at the list tail instead of pushing past it.
        assert_eq!(
            scroll_offset_for_direction(0, 19, 20, 5, 2, ScrollDirection::Down),
            15
        );
        assert_eq!(
            scroll_offset_for_direction(0, 20, 20, 5, 2, ScrollDirection::Down),
            15
        );
    }

    #[test]
    fn up_scroll_climbs_toward_the_head_revealing_rows_above() {
        // Climbing from the bottom (cursor 19, offset 12) upward: the window
        // recedes once the cursor is within `context` rows of the top edge, so
        // the highlight climbs instead of sticking to the bottom anchor.
        assert_eq!(
            scroll_offset_for_direction(12, 18, 20, 8, 2, ScrollDirection::Up),
            12
        );
        assert_eq!(
            scroll_offset_for_direction(12, 15, 20, 8, 2, ScrollDirection::Up),
            12
        );
        assert_eq!(
            scroll_offset_for_direction(12, 14, 20, 8, 2, ScrollDirection::Up),
            12
        );
        // cursor 13 => cursor - context = 11, below the current offset: recede.
        assert_eq!(
            scroll_offset_for_direction(12, 13, 20, 8, 2, ScrollDirection::Up),
            11
        );
        assert_eq!(
            scroll_offset_for_direction(11, 12, 20, 8, 2, ScrollDirection::Up),
            10
        );
    }

    #[test]
    fn up_scroll_floors_at_zero_near_the_head() {
        // Near the head the offset bottoms out at zero, letting the cursor
        // rise through the visible top while no more rows exist above it.
        assert_eq!(
            scroll_offset_for_direction(5, 0, 9, 5, 2, ScrollDirection::Up),
            0
        );
        assert_eq!(
            scroll_offset_for_direction(5, 1, 9, 5, 2, ScrollDirection::Up),
            0
        );
        assert_eq!(
            scroll_offset_for_direction(5, 2, 9, 5, 2, ScrollDirection::Up),
            0
        );
        assert_eq!(
            scroll_offset_for_direction(5, 3, 9, 5, 2, ScrollDirection::Up),
            1
        );
    }

    #[test]
    fn scroll_direction_never_advances_against_the_move_direction() {
        // Down never recedes, Up never advances, over a long run.
        let mut offset = 0usize;
        for cursor in 0..20 {
            offset = scroll_offset_for_direction(offset, cursor, 20, 8, 2, ScrollDirection::Down);
        }
        assert_eq!(offset, 12, "down only advances toward the tail");

        let mut offset = 12usize;
        for cursor in (0..20).rev() {
            offset = scroll_offset_for_direction(offset, cursor, 20, 8, 2, ScrollDirection::Up);
        }
        assert_eq!(offset, 0, "up only recedes toward the head");
    }

    #[test]
    fn scroll_direction_anchors_symmetric_on_the_same_cursor() {
        // The same cursor yields a higher window start when moving down (air
        // below) and a lower one when moving up (air above), which is the
        // whole point of the directional rule.
        let down = scroll_offset_for_direction(0, 12, 20, 8, 2, ScrollDirection::Down);
        let up = scroll_offset_for_direction(12, 12, 20, 8, 2, ScrollDirection::Up);
        assert_eq!(down, 7); // cursor rests two rows above the bottom edge
        assert_eq!(up, 10); // cursor rests two rows below the top edge
        assert!(down < up, "down must hug the bottom, up must hug the top");
    }

    #[test]
    fn scroll_direction_degrades_gracefully_on_degenerate_inputs() {
        // Empty list returns the head; a tiny viewport still keeps the cursor
        // visible, and a huge context is clamped down to the viewport.
        for direction in [ScrollDirection::Up, ScrollDirection::Down] {
            assert_eq!(scroll_offset_for_direction(0, 0, 0, 5, 2, direction), 0);
            assert_eq!(scroll_offset_for_direction(0, 3, 4, 1, 2, direction), 3);
        }
        assert_eq!(
            scroll_offset_for_direction(0, 6, 20, 3, 99, ScrollDirection::Down),
            6
        );
        assert_eq!(
            scroll_offset_for_direction(0, 6, 20, 3, 99, ScrollDirection::Up),
            4
        );
    }

    #[test]
    fn scroll_direction_never_hides_the_cursor_within_the_viewport() {
        for len in 1..=30 {
            for viewport in 1..=8 {
                for cursor in 0..len {
                    for prev in 0..=len {
                        for direction in [ScrollDirection::Up, ScrollDirection::Down] {
                            let offset = scroll_offset_for_direction(
                                prev, cursor, len, viewport, 2, direction,
                            );
                            assert!(
                                offset <= cursor,
                                "len {len} viewport {viewport} cursor {cursor} prev {prev} {direction:?}: offset {offset} above cursor"
                            );
                            assert!(
                                cursor < offset + viewport,
                                "len {len} viewport {viewport} cursor {cursor} prev {prev} {direction:?}: cursor below window"
                            );
                            assert!(
                                offset + viewport <= len.max(viewport),
                                "len {len} viewport {viewport} cursor {cursor} prev {prev} {direction:?}: window past tail"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn taking_marked_entries_filters_unsupported_and_consumes_marks() {
        let mut state = BrowserState::new(PathBuf::from("/fixture"));
        state.replace_contents(
            PathBuf::from("/fixture"),
            vec![
                entry("Album", EntryKind::Dir),
                entry("keep.mp3", EntryKind::File),
                entry("notes.txt", EntryKind::File),
                entry("linked", EntryKind::Symlink),
            ],
        );
        let all_paths: Vec<PathBuf> = state.entries.iter().map(|e| e.path.clone()).collect();

        let mut marks = HashSet::new();
        marks.insert(all_paths[0].clone()); // directory passes through
        marks.insert(all_paths[1].clone()); // supported audio passes
        marks.insert(all_paths[2].clone()); // unsupported file dropped
        marks.insert(all_paths[3].clone()); // symlink passes for recursive scan

        let taken = state.take_marked_or_cursor(&mut marks);

        let names: Vec<&str> = taken.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["Album", "keep.mp3", "linked"]);
        // Consumption semantics prevent accidental duplicate queues
        assert!(marks.is_empty());
    }

    #[test]
    fn taking_with_no_marks_falls_back_to_the_cursor_entry_only() {
        let mut state = BrowserState::new(PathBuf::from("/fixture"));
        state.replace_contents(
            PathBuf::from("/fixture"),
            vec![
                entry("a.mp3", EntryKind::File),
                entry("b.txt", EntryKind::File),
                entry("C.mp3", EntryKind::File),
            ],
        );
        let mut marks = HashSet::new();

        state.cursor = 2;
        let taken = state.take_marked_or_cursor(&mut marks);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].name, "C.mp3");

        state.cursor = 1;
        let taken = state.take_marked_or_cursor(&mut marks);
        // Unsupported cursor file offers nothing to queue
        assert!(taken.is_empty());
    }
}
