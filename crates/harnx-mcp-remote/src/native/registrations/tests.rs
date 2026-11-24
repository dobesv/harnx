//! Broker-backed lifecycle regressions with explicit race barriers.

use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::oneshot;

mod fixture;
mod publication;
mod refresh;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) enum Point {
    PublicationChecked,
    CredentialDeleted,
    CatalogSnapshot,
    RefreshSnapshot,
}

type Pause = (oneshot::Sender<()>, oneshot::Receiver<()>);

#[derive(Default)]
pub(super) struct Hooks {
    pauses: Mutex<HashMap<Point, VecDeque<Pause>>>,
}

impl Hooks {
    fn arm(&self, point: Point) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, wait) = oneshot::channel();
        let (resume, suspended) = oneshot::channel();
        self.pauses
            .lock()
            .unwrap()
            .entry(point)
            .or_default()
            .push_back((reached, suspended));
        (wait, resume)
    }

    pub(super) async fn pause(&self, point: Point) {
        let pause = self
            .pauses
            .lock()
            .unwrap()
            .get_mut(&point)
            .and_then(VecDeque::pop_front);
        if let Some((reached, resume)) = pause {
            let _ = reached.send(());
            resume.await.expect("resume the paused writer");
        }
    }
}

async fn reached(wait: oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .expect("writer reached the barrier")
        .expect("writer is alive");
}
