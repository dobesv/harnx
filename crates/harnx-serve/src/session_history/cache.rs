use super::{params::Window, projection::Projection};
use std::{collections::VecDeque, sync::Arc};

const MAX_WINDOWS: usize = 16;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct HistoryCache {
    windows: VecDeque<(Window, Arc<Projection>, usize)>,
    bytes: usize,
}

impl HistoryCache {
    pub(super) fn get(&mut self, key: &Window) -> Option<Arc<Projection>> {
        let index = self
            .windows
            .iter()
            .position(|(window, _, _)| window == key)?;
        let item = self.windows.remove(index)?;
        let projection = Arc::clone(&item.1);
        self.windows.push_back(item);
        Some(projection)
    }

    pub(super) fn insert(&mut self, key: Window, projection: Arc<Projection>) {
        if let Some(index) = self
            .windows
            .iter()
            .position(|(window, _, _)| window == &key)
        {
            self.bytes -= self
                .windows
                .remove(index)
                .expect("existing history window")
                .2;
        }
        let bytes = projection.byte_size();
        if bytes > MAX_BYTES {
            return;
        }
        while self.windows.len() >= MAX_WINDOWS || self.bytes + bytes > MAX_BYTES {
            if let Some((_, _, removed)) = self.windows.pop_front() {
                self.bytes -= removed;
            }
        }
        self.bytes += bytes;
        self.windows.push_back((key, projection, bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_history::params::Selector;
    use harnx_core::{
        message::{MessageContent, MessageRole},
        session::{Session, SessionLogEntry},
    };

    fn projection(size: usize) -> (Window, Arc<Projection>) {
        let entries = vec![(
            1,
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("x".repeat(size)),
                timestamp: None,
                fence_token: None,
            },
        )];
        let window = Window {
            version: 1,
            scope: ("local".into(), "agent".into(), "session".into()),
            created_at: "test".into(),
            as_of_seq: 1,
            selector: Selector::Range { from: 1, to: 1 },
        };
        let projection = Projection::build(&entries, &Session::default(), &window).unwrap();
        (window, Arc::new(projection))
    }

    #[test]
    fn byte_budget_evicts_windows_before_count_limit() {
        let (key, projection) = projection(MAX_BYTES / 2 + 1);
        let mut other = key.clone();
        other.created_at = "other".into();
        let mut cache = HistoryCache::default();
        cache.insert(key.clone(), Arc::clone(&projection));
        cache.insert(other.clone(), projection);
        assert!(cache.get(&key).is_none());
        assert!(cache.get(&other).is_some());
        assert!(cache.bytes <= MAX_BYTES);
    }

    #[test]
    fn oversized_windows_bypass_cache() {
        let (key, projection) = projection(MAX_BYTES + 1);
        let mut cache = HistoryCache::default();
        cache.insert(key.clone(), projection);
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.bytes, 0);
    }

    fn filled_cache() -> (HistoryCache, Vec<Window>, Arc<Projection>) {
        let (key, projection) = projection(32);
        let keys: Vec<_> = (0..MAX_WINDOWS)
            .map(|index| Window {
                created_at: index.to_string(),
                ..key.clone()
            })
            .collect();
        let mut cache = HistoryCache::default();
        for key in &keys {
            cache.insert(key.clone(), Arc::clone(&projection));
        }
        (cache, keys, projection)
    }

    #[test]
    fn get_promotes_a_window_before_eviction() {
        let (mut cache, keys, projection) = filled_cache();
        let bytes = cache.bytes;
        cache.get(&keys[0]).expect("oldest window cached");
        let extra = Window {
            created_at: "extra".into(),
            ..keys[0].clone()
        };
        cache.insert(extra, projection);
        assert!(cache.get(&keys[0]).is_some());
        assert!(cache.get(&keys[1]).is_none());
        assert_eq!(cache.bytes, bytes);
    }

    #[test]
    fn reinserting_a_window_promotes_it_and_replaces_its_byte_cost() {
        let (mut cache, keys, original) = filled_cache();
        let (_, replacement) = projection(64);
        let bytes = cache.bytes - original.byte_size() + replacement.byte_size();
        cache.insert(keys[0].clone(), Arc::clone(&replacement));
        let extra = Window {
            created_at: "extra".into(),
            ..keys[0].clone()
        };
        cache.insert(extra, original);
        assert!(Arc::ptr_eq(
            &replacement,
            &cache.get(&keys[0]).expect("reinserted window retained")
        ));
        assert!(cache.get(&keys[1]).is_none());
        assert_eq!(cache.bytes, bytes);
    }
}
