use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::markdown_render::RenderedEntry;
use crate::types::{RenderEntryState, Tui};

impl Tui {
    pub(crate) fn render_archive_view(&mut self, frame: &mut Frame<'_>, size: Rect) {
        frame.render_widget(Clear, size);
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(size);
        let browser = self.app.archive_browser.as_mut().expect("archive browser");
        let depth = browser.views.len();
        let view = browser.views.last_mut().expect("archive view");
        let entries = view
            .items
            .iter_mut()
            .enumerate()
            .map(|(index, item)| {
                let mut entry = Self::render_entry(
                    item,
                    self.app.show_sequence_numbers,
                    self.app.show_timestamps,
                    self.app.use_utc_timestamps,
                    chunks[0].width,
                    RenderEntryState::new(false, 0),
                    self.code_theme.as_ref(),
                );
                if view.focus == Some(index) {
                    entry.reverse_style();
                }
                entry
            })
            .collect::<Vec<_>>();
        render_entries(view, frame, chunks[0], &entries);
        let earlier_hint = if view.focused_compaction().is_some() {
            "  ENTER/earlier history"
        } else {
            ""
        };
        let footer = format!(
            " Earlier history before compaction {} (level {depth}, read-only)\n ↑↓/browse{earlier_hint}  PgUp/PgDn/scroll  ESC/back",
            view.before,
        );
        frame.render_widget(Paragraph::new(footer), chunks[1]);
    }
}

fn render_entries(
    view: &mut super::ArchiveView,
    frame: &mut Frame<'_>,
    area: Rect,
    entries: &[RenderedEntry],
) {
    // Static windows never follow the live transcript, even after End/PageDown.
    view.scroll.follow = false;
    // Populate this window's height cache before scrolling to a wrapped item.
    view.scroll.render(frame, area, entries, |entry| {
        (entry.total_height as usize, entry.clone())
    });
    view.scroll.position = view.scroll.position.min(view.scroll.last_max_position);
    if view.scroll_to_focus {
        if let Some(focus) = view.focus {
            view.scroll.position = view.scroll.scroll_position_to_show_item(
                focus,
                area.width,
                area.height as usize,
                entries.len(),
            );
        }
        view.scroll_to_focus = false;
        frame.render_widget(Clear, area);
        view.scroll.render(frame, area, entries, |entry| {
            (entry.total_height as usize, entry.clone())
        });
    }
}
