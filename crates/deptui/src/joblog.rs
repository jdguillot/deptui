//! The job-log window: the entry type, which entries the pane shows,
//! and how a visual selection maps to columns.
//!
//! Key handling (`app.rs`) and painting (`ui.rs`) both operate on this
//! window, and they used to carry private copies of its rules — the
//! host filter existed twice, and the char-selection slice existed
//! twice (once for highlighting, once for yanking). Each rule lives
//! here exactly once, so what the pane shows, what search walks, what
//! the highlight covers, and what `y` copies can never disagree.

use crate::host::LogKind;

/// One line of the log buffer.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub text: String,
    pub is_err: bool,
    /// Which host's deploy produced this line, if any. `None` is for
    /// app-level status messages (reachability sweeps, toggle flips,
    /// banner strings, etc.). Used by the job-log pane to colour-tag
    /// each line with its origin host.
    pub host: Option<String>,
    /// Typed classification, set by the producer. The renderer styles
    /// from this instead of re-parsing the text.
    pub kind: LogKind,
}

/// Whether the visual selection in the job log is character-level or line-level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualMode {
    /// `v` — cursor tracks (line, col); partial-line selection is possible.
    Char,
    /// `V` — whole lines only; col component is ignored.
    Line,
}

/// Active visual selection in the job log pane. Indices are in terms of the
/// *filtered* log (same index space as [`filtered_indices`]).
#[derive(Debug, Clone)]
pub struct VisualSel {
    pub mode: VisualMode,
    /// The end the user *started* the selection from. Fixed until selection ends.
    pub anchor: (usize, usize), // (filtered_line_idx, char_col)
    /// The end the user is currently moving. Drives `j`/`k`/`h`/`l`.
    pub cursor: (usize, usize),
}

impl VisualSel {
    /// Returns the normalised range `(start, end)` where start ≤ end.
    /// Both elements are `(filtered_line_idx, char_col)`.
    pub fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        let (al, ac) = self.anchor;
        let (cl, cc) = self.cursor;
        if al < cl || (al == cl && ac <= cc) {
            ((al, ac), (cl, cc))
        } else {
            ((cl, cc), (al, ac))
        }
    }
}

/// Indices into `log` that the job-log pane currently shows: entries
/// for the marked hosts (or the selected host when no marks are set),
/// plus every untagged entry. Untagged lines are app-level messages —
/// key hints, cancellations, agent acks — and this pane is the only
/// place they can appear, so hiding them meant the user never saw
/// them at all (the "press `a` and nothing happens" bug).
pub fn filtered_indices(log: &[LogEntry], marked: &[String], selected: Option<&str>) -> Vec<usize> {
    let active: std::collections::HashSet<&str> = if marked.is_empty() {
        selected.into_iter().collect()
    } else {
        marked.iter().map(|s| s.as_str()).collect()
    };
    log.iter()
        .enumerate()
        .filter_map(|(i, e)| match e.host.as_deref() {
            Some(h) => active.contains(h).then_some(i),
            None => Some(i),
        })
        .collect()
}

/// Column bounds `[start, end)` of a char-mode selection on one line.
///
/// `line_idx`, `start_line`, and `end_line` are filtered-line indices
/// (the caller has already normalised the selection); `char_count` is
/// the line's length in chars. Both the renderer's highlight split and
/// the yank slice go through this, so what looks selected is exactly
/// what `y` copies.
pub fn char_selection_bounds(
    char_count: usize,
    line_idx: usize,
    start_line: usize,
    start_col: usize,
    end_line: usize,
    end_col: usize,
) -> (usize, usize) {
    if start_line == end_line {
        // Single-line selection.
        (start_col.min(char_count), (end_col + 1).min(char_count))
    } else if line_idx == start_line {
        (start_col.min(char_count), char_count)
    } else if line_idx == end_line {
        (0, (end_col + 1).min(char_count))
    } else {
        (0, char_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(host: Option<&str>) -> LogEntry {
        LogEntry {
            text: "x".into(),
            is_err: false,
            host: host.map(str::to_string),
            kind: LogKind::Plain,
        }
    }

    #[test]
    fn filter_shows_selected_host_plus_untagged_lines() {
        let log = vec![entry(Some("a")), entry(None), entry(Some("b"))];
        assert_eq!(filtered_indices(&log, &[], Some("a")), vec![0, 1]);
        assert_eq!(filtered_indices(&log, &[], Some("b")), vec![1, 2]);
        // No selection at all: app-level messages still show.
        assert_eq!(filtered_indices(&log, &[], None), vec![1]);
    }

    #[test]
    fn filter_prefers_marked_hosts_over_the_selection() {
        let log = vec![entry(Some("a")), entry(Some("b")), entry(Some("c"))];
        let marked = vec!["a".to_string(), "c".to_string()];
        assert_eq!(filtered_indices(&log, &marked, Some("b")), vec![0, 2]);
    }

    #[test]
    fn filter_keeps_untagged_lines_under_marks_too() {
        let log = vec![entry(Some("a")), entry(None), entry(Some("b"))];
        let marked = vec!["b".to_string()];
        assert_eq!(filtered_indices(&log, &marked, Some("a")), vec![1, 2]);
    }

    #[test]
    fn char_bounds_single_line() {
        assert_eq!(char_selection_bounds(10, 3, 3, 2, 3, 5), (2, 6));
        // End clamped to the line length.
        assert_eq!(char_selection_bounds(4, 3, 3, 2, 3, 9), (2, 4));
    }

    #[test]
    fn char_bounds_multi_line() {
        // First line: from start_col to the end.
        assert_eq!(char_selection_bounds(10, 1, 1, 4, 3, 2), (4, 10));
        // Middle line: whole line.
        assert_eq!(char_selection_bounds(10, 2, 1, 4, 3, 2), (0, 10));
        // Last line: from 0 through end_col inclusive.
        assert_eq!(char_selection_bounds(10, 3, 1, 4, 3, 2), (0, 3));
    }
}
