use anyhow::{Context, Result};
use harnx_client::Model;
use harnx_core::{message::MessageRole, require_nextest, session::SessionLogEntry};
use harnx_runtime::config::{
    reload_session_from_disk,
    session::{self, Session},
    session_lock::SessionLock,
    Config, GlobalConfig,
};
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[test]
fn test_lock_serializes_two_threads() -> Result<()> {
    require_nextest();

    let fixture = TestFixture::new("serialize-two-threads")?;
    let session_path = fixture.session_path.clone();

    let start_barrier = Arc::new(Barrier::new(2));
    let release_thread1_tx = Arc::new(mpsc::channel::<()>().0);
    let release_thread1_rx = mpsc::channel::<()>().1;
    drop(release_thread1_tx);
    drop(release_thread1_rx);

    let (thread1_started_tx, thread1_started_rx) = mpsc::channel();
    let (release_thread1_tx, release_thread1_rx) = mpsc::channel();

    let thread1_path = session_path.clone();
    let thread1_barrier = Arc::clone(&start_barrier);
    let thread1 = thread::spawn(move || -> Result<()> {
        let _lock = SessionLock::acquire(&thread1_path)?;
        append_text_entry(&thread1_path, "thread-1")?;
        thread1_started_tx.send(())?;
        thread1_barrier.wait();
        release_thread1_rx.recv()?;
        Ok(())
    });

    thread1_started_rx
        .recv()
        .context("thread 1 failed before signaling ready")?;

    let thread2_path = session_path.clone();
    let thread2_barrier = Arc::clone(&start_barrier);
    let thread2 = thread::spawn(move || -> Result<Duration> {
        thread2_barrier.wait();
        let start = Instant::now();
        let _lock = SessionLock::acquire(&thread2_path)?;
        let blocked_for = start.elapsed();
        append_text_entry(&thread2_path, "thread-2")?;
        Ok(blocked_for)
    });

    thread::sleep(Duration::from_millis(150));
    release_thread1_tx.send(())?;

    thread1.join().expect("thread 1 should not panic")?;
    let blocked_for = thread2.join().expect("thread 2 should not panic")?;

    assert!(blocked_for >= Duration::from_millis(125));

    let entries = read_entries(&fixture.session_path)?;
    assert_eq!(message_texts(&entries), vec!["thread-1", "thread-2"]);
    assert_eq!(entries.len(), 3, "header + 2 messages expected");

    Ok(())
}

#[test]
fn test_stale_free_sequence_numbers() -> Result<()> {
    require_nextest();

    let fixture = TestFixture::new("stale-free-sequences")?;
    let session_path = fixture.session_path.clone();

    let start_barrier = Arc::new(Barrier::new(2));
    let (thread1_done_writing_tx, thread1_done_writing_rx) = mpsc::channel();
    let (release_thread1_tx, release_thread1_rx) = mpsc::channel();

    let thread1_path = session_path.clone();
    let thread1_barrier = Arc::clone(&start_barrier);
    let thread1 = thread::spawn(move || -> Result<()> {
        let _lock = SessionLock::acquire(&thread1_path)?;
        for marker in ["a-1", "a-2", "a-3"] {
            append_text_entry(&thread1_path, marker)?;
        }
        thread1_done_writing_tx.send(())?;
        thread1_barrier.wait();
        release_thread1_rx.recv()?;
        Ok(())
    });

    thread1_done_writing_rx
        .recv()
        .context("thread 1 failed before finishing initial appends")?;

    let thread2_path = session_path.clone();
    let thread2_barrier = Arc::clone(&start_barrier);
    let thread2 = thread::spawn(move || -> Result<()> {
        thread2_barrier.wait();
        let _lock = SessionLock::acquire(&thread2_path)?;
        for marker in ["b-1", "b-2"] {
            append_text_entry(&thread2_path, marker)?;
        }
        Ok(())
    });

    thread::sleep(Duration::from_millis(150));
    release_thread1_tx.send(())?;

    thread1.join().expect("thread 1 should not panic")?;
    thread2.join().expect("thread 2 should not panic")?;

    let entries = read_entries(&fixture.session_path)?;
    assert_eq!(entries.len(), 6, "header + 5 messages expected");

    let seqs: Vec<usize> =
        serde_yaml::Deserializer::from_str(&fs::read_to_string(&fixture.session_path)?)
            .enumerate()
            .map(|(seq, _)| seq)
            .collect();
    assert_eq!(seqs, vec![0, 1, 2, 3, 4, 5]);

    let texts = message_texts(&entries);
    assert_eq!(texts, vec!["a-1", "a-2", "a-3", "b-1", "b-2"]);

    Ok(())
}

#[test]
fn test_save_does_not_overwrite_concurrent_appends() -> Result<()> {
    require_nextest();

    let fixture = Arc::new(TestFixture::new("save-visible-after-lock-release")?);

    let (save_completed_tx, save_completed_rx) = mpsc::channel();
    let (release_lock_tx, release_lock_rx) = mpsc::channel();

    let thread_a_fixture = Arc::clone(&fixture);
    let thread_a = thread::spawn(move || -> Result<()> {
        let lock = SessionLock::acquire(&thread_a_fixture.session_path)?;
        let mut session = thread_a_fixture.make_session()?;
        session.push_message_for_test(MessageRole::User, "saved-by-a".to_string());
        session::save(
            &mut session,
            &thread_a_fixture.session_name,
            &thread_a_fixture.session_path,
            false,
            Some(&lock),
        )?;
        save_completed_tx.send(())?;
        release_lock_rx.recv()?;
        Ok(())
    });

    save_completed_rx
        .recv()
        .context("save thread failed before signaling save completion")?;

    let thread_b_path = fixture.session_path.clone();
    let thread_b = thread::spawn(move || -> Result<Vec<SessionLogEntry>> {
        let _lock = SessionLock::acquire(&thread_b_path)?;
        append_text_entry(&thread_b_path, "thread-b")?;
        read_entries(&thread_b_path)
    });

    thread::sleep(Duration::from_millis(150));
    release_lock_tx.send(())?;

    thread_a.join().expect("thread A should not panic")?;
    let entries_seen_by_b = thread_b.join().expect("thread B should not panic")?;

    assert_eq!(
        message_texts(&entries_seen_by_b),
        vec!["saved-by-a", "thread-b"]
    );

    let final_entries = read_entries(&fixture.session_path)?;
    assert_eq!(
        message_texts(&final_entries),
        vec!["saved-by-a", "thread-b"]
    );
    assert_eq!(final_entries.len(), 3, "header + 2 messages expected");

    Ok(())
}

#[test]
fn test_reload_on_acquire() -> Result<()> {
    require_nextest();

    let fixture = TestFixture::new("reload-on-acquire")?;
    write_manual_session(
        &fixture.session_path,
        &fixture.session_name,
        &["one", "two", "three"],
    )?;

    let mut config = fixture.base_config();
    config.use_session(Some(&fixture.session_name))?;
    if let Some(session) = config.session.as_mut() {
        session.log_entry_count = 0;
    }
    let global_config: GlobalConfig = Arc::new(RwLock::new(config));

    let _lock = SessionLock::acquire(&fixture.session_path)?;
    reload_session_from_disk(&global_config)?;

    let guard = global_config.read();
    let session = guard.session.as_ref().expect("session should be loaded");
    assert_eq!(session.log_entry_count, 4, "header + 3 messages expected");

    Ok(())
}

#[test]
fn test_crash_safety_via_drop() -> Result<()> {
    require_nextest();

    let fixture = TestFixture::new("crash-safety-drop")?;

    {
        let lock = SessionLock::acquire(&fixture.session_path)?;
        drop(lock);
    }

    let second = SessionLock::try_acquire(&fixture.session_path)?;
    assert!(
        second.is_some(),
        "lock should be acquirable after guard drop"
    );

    Ok(())
}

struct TestFixture {
    _temp_dir: TempDir,
    session_name: String,
    session_path: PathBuf,
    sessions_dir: PathBuf,
}

impl TestFixture {
    fn new(session_name: &str) -> Result<Self> {
        let temp_dir = tempfile::tempdir()?;
        let sessions_dir = temp_dir.path().join("sessions");
        fs::create_dir_all(&sessions_dir)?;
        let session_path = sessions_dir.join(format!("{session_name}.yaml"));
        write_manual_session(&session_path, session_name, &[])?;
        Ok(Self {
            _temp_dir: temp_dir,
            session_name: session_name.to_string(),
            session_path,
            sessions_dir,
        })
    }

    fn base_config(&self) -> Config {
        let mut config = Config {
            sessions_dir_override: Some(self.sessions_dir.clone()),
            ..Default::default()
        };
        config
            .clients
            .push(harnx_client::ClientConfig::OpenAICompatibleConfig(
                harnx_core::provider_config::openai_compatible::OpenAICompatibleConfig {
                    name: "test".to_string(),
                    api_base: None,
                    api_key: None,
                    models: vec![],
                    patches: None,
                    extra: None,
                    system_prompt_prefix: None,
                    package: None,
                },
            ));
        config.model = Model::new("test", "model");
        config.model_id = "test:model".to_string();
        config
    }

    fn make_session(&self) -> Result<Session> {
        let mut session = session::new(&self.base_config(), &self.session_name, None)?;
        session.set_sessions_dir(self.sessions_dir.clone());
        Ok(session)
    }
}

fn append_text_entry(session_path: &Path, text: &str) -> Result<()> {
    let mut entries = read_entries(session_path)?;
    entries.push(message_entry(text));
    write_entries(session_path, &entries)
}

fn message_entry(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: harnx_core::message::MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

fn read_entries(session_path: &Path) -> Result<Vec<SessionLogEntry>> {
    let content = fs::read_to_string(session_path)?;
    serde_yaml::Deserializer::from_str(&content)
        .map(SessionLogEntry::deserialize)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn write_entries(session_path: &Path, entries: &[SessionLogEntry]) -> Result<()> {
    let docs = entries
        .iter()
        .map(serde_yaml::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    fs::write(session_path, docs.join("---\n"))?;
    Ok(())
}

fn message_texts(entries: &[SessionLogEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            SessionLogEntry::Message { content, .. } => Some(content.to_text()),
            _ => None,
        })
        .collect()
}

fn write_manual_session(session_path: &Path, session_name: &str, texts: &[&str]) -> Result<()> {
    let header = SessionLogEntry::Header {
        model_id: "test:model".to_string(),
        temperature: None,
        top_p: None,
        use_tools: None,
        save_session: Some(true),
        compress_threshold: None,
        agent_name: None,
        session_id: Some(session_name.to_string()),
        working_dir: None,
        git_branch: None,
        git_remote: None,
        terminal_session_id: None,
        agent_variables: IndexMap::new(),
        agent_instructions: "test instructions".to_string(),
        model_fallbacks: vec![],
        compaction_agent: None,
    };

    let mut entries = vec![header];
    entries.extend(texts.iter().map(|text| message_entry(text)));
    write_entries(session_path, &entries)
}
