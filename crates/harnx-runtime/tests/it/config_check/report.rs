//! The report every check writes into: suppression, strict mode, rendering
//! and the `--allow` rules that drive suppression.

use harnx_runtime::config_check::{AllowRule, CheckSettings, Code, Finding, Report, Severity};

fn settings(strict: bool, allow: Vec<AllowRule>) -> CheckSettings {
    CheckSettings {
        strict,
        allow,
        ..CheckSettings::default()
    }
}

#[test]
fn strict_promotes_warnings() {
    let mut report = Report::default();
    report.push(Finding::warning(Code::EnvVarUnset, "NATS_TOKEN is not set").in_file("x.yaml"));

    let lenient = report.clone().finish(&settings(false, Vec::new()));
    assert_eq!(lenient.exit_code(), 0);
    assert_eq!(lenient.summary.warnings, 1);

    let strict = report.finish(&settings(true, Vec::new()));
    assert_eq!(strict.exit_code(), 1);
    assert_eq!(strict.summary.errors, 1);
    assert_eq!(strict.summary.warnings, 0);
    assert_eq!(strict.findings[0].severity, Severity::Error);
}

#[test]
fn allow_suppresses_by_code_and_glob() {
    let mut report = Report::default();
    report.push(
        Finding::warning(Code::EnvVarUnset, "TOKEN is not set").in_file("nats_servers/prod.yaml"),
    );
    report.push(Finding::warning(Code::EnvVarUnset, "TOKEN is not set").in_file("config.yaml"));
    report.push(
        Finding::warning(Code::CommandNotFound, "npx is not on PATH")
            .in_file("nats_servers/prod.yaml"),
    );
    let allow = vec![AllowRule {
        code: Code::EnvVarUnset,
        file: Some("nats_servers/*".to_string()),
    }];

    let report = report.finish(&settings(false, allow));

    assert_eq!(report.summary.suppressed, 1);
    assert_eq!(report.summary.warnings, 2);
    let kept: Vec<_> = report
        .findings
        .iter()
        .map(|finding| (finding.code, finding.file.clone().unwrap()))
        .collect();
    assert_eq!(
        kept,
        vec![
            (Code::EnvVarUnset, "config.yaml".into()),
            (Code::CommandNotFound, "nats_servers/prod.yaml".into()),
        ]
    );
}

#[test]
fn allow_without_file_suppresses_the_code_everywhere() {
    let mut report = Report::default();
    report.push(Finding::warning(Code::ToolUnverified, "a").in_file("agents/a.md"));
    report.push(Finding::warning(Code::ToolUnverified, "b"));
    let allow = vec![AllowRule {
        code: Code::ToolUnverified,
        file: None,
    }];

    let report = report.finish(&settings(true, allow));

    assert_eq!(report.summary.suppressed, 2);
    assert!(report.findings.is_empty());
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn json_shape() {
    let mut report = Report::default();
    report.push(
        Finding::error(Code::UnknownKey, "unknown key a.b")
            .in_file("config.yaml")
            .at_key("a.b"),
    );
    let report = report.finish(&CheckSettings::default());

    let value: serde_json::Value = serde_json::from_str(&report.render_json()).unwrap();

    let finding = &value["findings"][0];
    assert_eq!(finding["severity"], "error");
    assert_eq!(finding["code"], "unknown-key");
    assert_eq!(finding["message"], "unknown key a.b");
    assert_eq!(finding["file"], "config.yaml");
    assert_eq!(finding["key_path"], "a.b");
    assert!(finding.get("line").is_none(), "absent line is omitted");
    assert_eq!(value["summary"]["errors"], 1);
    assert_eq!(value["summary"]["warnings"], 0);
}

#[test]
fn text_line_format() {
    let mut report = Report::default();
    report.push(
        Finding::error(Code::ParseError, "did not find expected node content")
            .in_file("config.yaml")
            .at_line(3),
    );
    report.push(
        Finding::warning(Code::UnknownKey, "unknown key a.b")
            .in_file("config.yaml")
            .at_key("a.b"),
    );
    report.push(Finding::warning(Code::EmptyConfig, "nothing to check"));
    let report = report.finish(&CheckSettings::default());

    let text = report.render_text();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(
        lines[0],
        "error[parse-error]: config.yaml:3: did not find expected node content"
    );
    assert_eq!(
        lines[1],
        "warning[unknown-key]: config.yaml: unknown key a.b"
    );
    assert_eq!(lines[2], "warning[empty-config]: nothing to check");
    assert!(
        lines.last().unwrap().contains("1 error, 2 warnings"),
        "summary line: {text}"
    );
}

#[test]
fn code_strings_match_serde() {
    for code in [
        Code::UnknownKey,
        Code::ModelNotInCatalog,
        Code::EnvLineInvalid,
    ] {
        let serialized = serde_json::to_value(code).unwrap();
        assert_eq!(serialized, code.as_str());
    }
    assert_eq!(Code::SilentDrop.default_severity(), Severity::Error);
    assert_eq!(
        Code::GlobMatchesNothing.default_severity(),
        Severity::Warning
    );
}

#[test]
fn allow_rule_parses_a_code() {
    let rule: AllowRule = "tool-unverified".parse().unwrap();

    assert_eq!(
        rule,
        AllowRule {
            code: Code::ToolUnverified,
            file: None,
        }
    );
}

#[test]
fn allow_rule_parses_a_code_and_a_glob() {
    let rule: AllowRule = "env-var-unset:nats_servers/*".parse().unwrap();

    assert_eq!(
        rule,
        AllowRule {
            code: Code::EnvVarUnset,
            file: Some("nats_servers/*".to_string()),
        }
    );
}

#[test]
fn allow_rule_rejects_an_unknown_code() {
    let error = "tool-unverifed".parse::<AllowRule>().unwrap_err();

    let message = format!("{error:#}");
    assert!(
        message.contains("unknown finding code 'tool-unverifed'"),
        "{message}"
    );
    assert!(
        message.contains("tool-unverified"),
        "lists the codes: {message}"
    );
}

#[test]
fn allow_rule_rejects_a_bad_glob() {
    let error = "env-var-unset:nats_servers/["
        .parse::<AllowRule>()
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("invalid file glob"),
        "{error:#}"
    );
}

#[test]
fn every_code_is_listed_once() {
    for code in Code::ALL {
        let parsed: AllowRule = code.as_str().parse().unwrap();
        assert_eq!(parsed.code, code);
    }
    let mut names: Vec<&str> = Code::ALL.iter().map(Code::as_str).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), Code::ALL.len());
}
