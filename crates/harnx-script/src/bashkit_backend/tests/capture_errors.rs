use super::*;

#[tokio::test]
async fn fatal_command_error_survives_full_stderr_capture() {
    for cap in [0, 3, 9, 32] {
        let result = BashKitBackend
            .run(crate::ScriptRequest {
                script: "printf '123456789' >&2\nprobe\necho after",
                limits: ScriptLimits {
                    max_stderr_bytes: cap,
                    ..Default::default()
                },
                commands: vec![ProbeCommand::new(Behavior::Fatal)],
                fs_mounts: vec![],
                cancel: CancellationToken::new(),
            })
            .await;
        assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.len() <= cap, "{result:?}");
        assert!(!result.stdout.contains("after"), "{result:?}");
        assert_eq!(result.error.as_deref(), Some("fatal inner call"));
        if cap <= 9 {
            assert!(result.stderr_truncated, "{result:?}");
            assert!(result.limit_hit.is_some(), "{result:?}");
        }
    }
}

#[test]
fn returned_parse_error_survives_full_stderr_capture() {
    use super::super::{capture::Capture, outcome::classify};

    for cap in [0, 3, 9] {
        let mut capture = Capture::new(&ScriptLimits {
            max_stderr_bytes: cap,
            ..Default::default()
        });
        capture.append("", "123456789");
        let error = bashkit::Error::parse("invalid command");
        let expected = error.to_string();
        let (status, code, limit) = classify(Err(error), &mut capture);
        assert_eq!((status, code), (ScriptStatus::Error, 2));
        assert_eq!(capture.error.as_deref(), Some(expected.as_str()));
        assert!(capture.stderr.text.len() <= cap);
        assert!(limit.is_some());
    }
}

#[tokio::test]
async fn late_parse_error_is_capped_by_bashkit_before_classification() {
    // This is a dependency boundary, not an Err(Parse) classification path:
    // Once stderr has overflowed, BashKit 0.20.2 returns Ok(exit 2) and caps
    // even its streaming callback. PR3 never receives the syntax diagnostic.
    for cap in [0, 3] {
        let limits = ScriptLimits {
            max_stderr_bytes: cap,
            ..Default::default()
        };
        let observed = Arc::new(Mutex::new(String::new()));
        let chunks = observed.clone();
        let mut bash = BashKitBackend::builder(&limits).build();
        let result = bash
            .exec_streaming(
                "set -e\nprintf '123456789' >&2\nif",
                Box::new(move |_, stderr| chunks.lock().unwrap().push_str(&stderr.text_lossy())),
            )
            .await
            .expect("late parse errors return Ok, not Err(Parse)");
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr_truncated, "cap={cap}: {result:?}");
        assert_eq!(result.stderr.text_lossy(), "123456789"[..cap]);
        assert_eq!(*observed.lock().unwrap(), "123456789"[..cap]);
    }
}
