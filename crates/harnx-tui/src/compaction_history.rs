//! Read-only transcript windows, frozen at the first archive read.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::session::{Session, SessionLogEntry};
use harnx_core::transcript_view::{self, CompactionMarker, TranscriptView};
use harnx_runtime::config::session::replay_nats_entries_into_session;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::tool::ToolDeclaration;
use ratatui_widget_scrolling::ScrollState;

use crate::types::{TranscriptItem, Tui};

mod render;

// Enter runs on the event loop, so bound connection setup and the full log read
// together, like Ctrl+D's mark-read, before returning control to the keyboard.
// Match runtime READ_TIMEOUT: sequential per-entry reads can take seconds on a healthy connection.
const ARCHIVE_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct ArchiveBrowser {
    entries: Vec<(u64, SessionLogEntry)>,
    declarations: HashMap<String, ToolDeclaration>,
    cluster: String,
    pub views: Vec<ArchiveView>,
}

pub(super) struct ArchiveView {
    pub before: u64,
    pub marker: Option<CompactionMarker>,
    pub items: Vec<TranscriptItem>,
    pub focus: Option<usize>,
    pub scroll: ScrollState,
    pub scroll_to_focus: bool,
}

impl ArchiveBrowser {
    fn open_focused_compaction(&mut self) -> Result<()> {
        let Some(seq) = self.views.last().and_then(ArchiveView::focused_compaction) else {
            return Ok(());
        };
        self.push(seq)
    }

    fn push(&mut self, before: u64) -> Result<()> {
        let view = transcript_view::before_compaction(&self.entries, before, None)?;
        let items = transcript_items(&view, &self.declarations, &self.cluster)?;
        let mut scroll = ScrollState::new();
        scroll.follow = false;
        self.views.push(ArchiveView {
            before,
            marker: view.compaction,
            focus: items.iter().position(TranscriptItem::is_navigable),
            items,
            scroll,
            scroll_to_focus: true,
        });
        Ok(())
    }
}

fn transcript_items(
    view: &TranscriptView,
    declarations: &HashMap<String, ToolDeclaration>,
    cluster: &str,
) -> Result<Vec<TranscriptItem>> {
    let entries = view
        .entries
        .iter()
        .map(|(seq, entry)| Ok((usize::try_from(*seq)?, entry.clone())))
        .collect::<Result<Vec<_>>>()?;
    let session =
        replay_nats_entries_into_session(&entries, "archived history", Session::default(), false)?;
    let mut items = crate::lifecycle::build_transcript_with_compaction_for_cluster(
        &[],
        &session.messages,
        view.compaction
            .as_ref()
            .map(|marker| marker.summary.as_str()),
        declarations,
        Some(cluster),
    );
    items.extend(
        session
            .replay_warnings
            .into_iter()
            .map(TranscriptItem::ErrorText),
    );
    Ok(items)
}

impl Tui {
    pub(super) async fn open_focused_compaction(&mut self) -> bool {
        let summary = match self
            .app
            .transcript_focus
            .and_then(|i| self.app.transcript.get(i))
        {
            Some(TranscriptItem::CompactionMarker { summary_text, .. }) => summary_text.clone(),
            _ => return false,
        };
        if let Err(error) = self.load_compaction_archive(&summary).await {
            self.show_archive_error(&error);
        }
        true
    }

    fn show_archive_error(&mut self, error: &anyhow::Error) {
        let text = format!("Failed to load earlier history: {error:#}");
        self.app.detail_view_scroll = ScrollState::new();
        self.app.detail_view_scroll.follow = false;
        self.app.detail_view_open = true;
        self.app.detail_view_entry = Some(TranscriptItem::ErrorText(text.clone()));
        self.app.doc_view = None;
        self.app.doc_history.clear();
        self.app.detail_view_title = Some("Earlier history unavailable".into());
        self.app.detail_view_text = Some(text);
    }

    async fn load_compaction_archive(&mut self, summary: &str) -> Result<()> {
        // Drop the config guard before the broker read. Browsing never activates a session.
        let cfg = self.config.read().clone();
        let session = cfg.session.as_ref().context("no active session")?;
        let (agent, cluster) = cfg
            .remote_agent
            .clone()
            .or_else(|| {
                cfg.agent.as_ref().map(|agent| {
                    (
                        agent.name().to_owned(),
                        cfg.default_cluster_key().to_owned(),
                    )
                })
            })
            .context("no session agent")?;
        let entries = tokio::time::timeout(ARCHIVE_READ_TIMEOUT, async {
            let jetstream = cfg.nats_jetstream(&cluster).await?;
            NatsSessionLog::for_agent(jetstream, &agent, session.id())
                .load_events_async()
                .await
        })
        .await
        .with_context(|| {
            format!("earlier history read timed out after {ARCHIVE_READ_TIMEOUT:?}")
        })??;
        let marker = transcript_view::default_view(&entries)?
            .compaction
            .context("no compaction in session log")?;
        anyhow::ensure!(
            marker.summary == summary,
            "session history changed; reopen the transcript"
        );
        let (declarations, _) =
            cfg.tool_declarations_for_use_tools(Some("*"), cfg.active_package().as_deref());
        let mut browser = ArchiveBrowser {
            entries,
            declarations: declarations
                .into_iter()
                .map(|decl| (decl.name.clone(), decl))
                .collect(),
            cluster,
            views: Vec::new(),
        };
        browser.push(marker.compaction_seq)?;
        self.app.archive_browser = Some(browser);
        Ok(())
    }

    pub(super) fn handle_archive_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(browser) = self.app.archive_browser.as_mut() else {
            return Ok(());
        };
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => {
                browser.views.pop();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if let Err(error) = browser.open_focused_compaction() {
                    self.show_archive_error(&error);
                    return Ok(());
                }
            }
            _ => {
                if let Some(view) = browser.views.last_mut() {
                    view.handle_key(key);
                }
            }
        }
        if browser.views.is_empty() {
            self.app.archive_browser = None;
        }
        Ok(())
    }
}

impl ArchiveView {
    fn focused_compaction(&self) -> Option<u64> {
        match self.items.get(self.focus?)? {
            TranscriptItem::CompactionMarker { .. } => {
                self.marker.as_ref().map(|marker| marker.compaction_seq)
            }
            _ => None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) => self.move_focus(false),
            (KeyCode::Down, KeyModifiers::NONE) => self.move_focus(true),
            _ => self.handle_scroll_key(key),
        }
    }

    fn handle_scroll_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::PageUp, KeyModifiers::NONE) => self.scroll_page(false),
            (KeyCode::PageDown, KeyModifiers::NONE) => self.scroll_page(true),
            (
                KeyCode::Home | KeyCode::Char('g' | '<'),
                KeyModifiers::NONE | KeyModifiers::SHIFT,
            ) => {
                self.scroll.scroll_to_top();
            }
            (KeyCode::End | KeyCode::Char('G' | '>'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.scroll.scroll_to_bottom();
            }
            _ => {} // No input, mutation, link activation or prompt submission in historical views.
        }
    }

    fn scroll_page(&mut self, forward: bool) {
        for _ in 0..10 {
            if forward {
                self.scroll.scroll_down();
            } else {
                self.scroll.scroll_up();
            }
        }
    }

    fn move_focus(&mut self, forward: bool) {
        let current = self.focus.unwrap_or(0);
        let next = if forward {
            (current + 1..self.items.len()).find(|&i| self.items[i].is_navigable())
        } else {
            (0..current).rev().find(|&i| self.items[i].is_navigable())
        };
        self.focus = next.or(self.focus);
        self.scroll_to_focus = self.focus.is_some();
    }
}
