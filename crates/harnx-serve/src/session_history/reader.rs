use super::{
    params::{Cursor, Request, Window},
    projection::{HistoryPage, Projection},
    HistoryError,
};
use crate::session_actor::{SessionKey, SessionRegistry};
use harnx_core::session::{Session, SessionLogEntry};
use harnx_runtime::{config::Config, nats_session_log::NatsSessionLog};
use std::sync::Arc;

type Entries = Vec<(u64, SessionLogEntry)>;

struct Reader {
    log: NatsSessionLog,
    base: Session,
    scope: (String, String, String),
    created_at: String,
}

pub(super) async fn read(
    config: &Config,
    registry: &SessionRegistry,
    key: &SessionKey,
    request: Request,
) -> Result<HistoryPage, HistoryError> {
    let reader = Reader::open(config, key).await?;
    let (cursor, entries) = reader.cursor(request).await?;
    let cached = registry
        .history_cache
        .lock()
        .expect("history cache poisoned")
        .get(&cursor.window);
    let projection = match cached {
        Some(projection) => return projection.page(&cursor).map_err(super::invalid),
        None => reader.project(&cursor.window, entries).await?,
    };
    let page = projection.page(&cursor).map_err(super::invalid)?;
    registry
        .history_cache
        .lock()
        .expect("history cache poisoned")
        .insert(cursor.window, projection);
    Ok(page)
}

impl Reader {
    async fn open(config: &Config, key: &SessionKey) -> Result<Self, HistoryError> {
        crate::ensure_frontend_nats_owner(key.cluster()).await?;
        let jetstream = crate::serve_nats_jetstream(config, key.cluster()).await?;
        let store =
            harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
                .await?;
        let record = store
            .get(&key.storage_key())
            .await?
            .ok_or(HistoryError::Missing)?;
        Ok(Self {
            log: NatsSessionLog::new(jetstream, key.storage_key()),
            base: record.metadata.base_session(),
            scope: (
                key.cluster().into(),
                key.agent().into(),
                key.session.clone(),
            ),
            created_at: record.metadata.created_at.to_rfc3339(),
        })
    }

    async fn cursor(&self, request: Request) -> Result<(Cursor, Option<Entries>), HistoryError> {
        match request {
            Request::Continue(cursor) => {
                if cursor.window.scope != self.scope || cursor.window.created_at != self.created_at
                {
                    return Err(HistoryError::Invalid(
                        "history cursor belongs to another session".into(),
                    ));
                }
                Ok((cursor, None))
            }
            Request::Initial { selector, limit } => {
                let entries = self.log.load_events_async().await?;
                let window = Window {
                    version: 1,
                    scope: self.scope.clone(),
                    created_at: self.created_at.clone(),
                    as_of_seq: entries.last().map_or(0, |(seq, _)| *seq),
                    selector,
                };
                Ok((
                    Cursor {
                        window,
                        offset: 0,
                        limit,
                    },
                    Some(entries),
                ))
            }
        }
    }

    async fn project(
        &self,
        window: &Window,
        entries: Option<Entries>,
    ) -> Result<Arc<Projection>, HistoryError> {
        let entries = match entries {
            Some(entries) => entries,
            None => self.log.load_events_async().await?,
        };
        Ok(Arc::new(
            Projection::build(&entries, &self.base, window).map_err(super::invalid)?,
        ))
    }
}
