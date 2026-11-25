//! Target-specific rejection classification: exact-target matching, sanitization,
//! additive compatibility and non-redispatch.
use super::*;
use aq_git_core::publication::rejection::{CLASSIFICATION_CAPABILITY, RejectionClass};

const TAB: &str = "\t";

/// Replaces `push` with canned porcelain stdout/stderr (`@REF@` stands for the
/// real `<candidate>:<target>` refspec) and counts push invocations.
fn canned_push(f: &mut Fixture, stdout: &str, stderr: &str) -> std::path::PathBuf {
    let dir = f.temp.path().join("canned");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("out"), stdout).unwrap();
    std::fs::write(dir.join("err"), stderr).unwrap();
    let wrapper = dir.join("git");
    let script = format!(
        r#"#!/bin/sh
for arg in "$@"; do
    if [ "$arg" = push ]; then
        for last; do :; done
        echo x >> '{d}/pushes'
        sed "s|@REF@|$last|g" '{d}/out'
        cat '{d}/err' >&2
        exit 1
    fi
done
exec /usr/bin/git "$@"
"#,
        d = dir.display()
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    f.service.repositories.runner = GitRunner::new(wrapper);
    dir.join("pushes")
}
fn pushes(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |s| s.lines().count())
}
fn line(flag: &str, refspec: &str, summary: &str) -> String {
    format!("{flag}{TAB}{refspec}{TAB}{summary}\n")
}
fn rejected_class(stdout: &str) -> Value {
    let mut f = Fixture::new();
    canned_push(&mut f, stdout, "");
    let result = success(f.service.execute(f.publish()));
    assert_eq!(result["outcome"], "rejected");
    assert_eq!(result["reason"], "remote_rejected");
    result["rejection_class"].clone()
}
fn state_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            state_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn exact_target_reasons_map_to_the_closed_set() {
    for (summary, class) in [
        ("[rejected] (non-fast-forward)", "non_fast_forward"),
        ("[rejected] (stale info)", "stale_lease"),
        ("[remote rejected] (hook declined)", "hook_declined"),
        (
            "[remote rejected] (pre-receive hook declined)",
            "hook_declined",
        ),
    ] {
        assert_eq!(
            rejected_class(&line("!", "@REF@", summary)),
            class,
            "{summary}"
        );
    }
}

#[test]
fn unrecognized_or_malformed_reasons_stay_unknown() {
    for summary in [
        "[remote rejected] (protected branch hook declined)",
        "[remote rejected] (update hook declined)",
        "[rejected] (fetch first)",
        "[rejected] (non-fast-forward) extra",
        "[rejected] (Non-Fast-Forward)",
        " [rejected] (stale info)",
        "[rejected] (stale info)\r",
        "[remote rejected] (pre-receive hook declined: secret-text)",
    ] {
        assert_eq!(
            rejected_class(&line("!", "@REF@", summary)),
            "unknown",
            "{summary:?}"
        );
    }
    // An extra tab-separated field makes the acknowledgement malformed.
    let extra = format!("!{TAB}@REF@{TAB}[rejected] (stale info){TAB}more\n");
    assert_eq!(rejected_class(&extra), "unknown");
}

#[test]
fn unrelated_refs_never_establish_the_target_cause() {
    let other = "1111111111111111111111111111111111111111:refs/heads/other";
    let stdout = format!(
        "{}{}",
        line("!", other, "[rejected] (non-fast-forward)"),
        line("!", "@REF@", "[remote rejected] (some new reason)")
    );
    assert_eq!(rejected_class(&stdout), "unknown");
    // The unrelated line may even come after the exact one.
    let stdout = format!(
        "{}{}",
        line("!", "@REF@", "[rejected] (stale info)"),
        line("!", other, "[remote rejected] (hook declined)")
    );
    assert_eq!(rejected_class(&stdout), "stale_lease");
}

#[test]
fn missing_ambiguous_or_mismatched_acknowledgements_are_indeterminate_without_class() {
    let other = "1111111111111111111111111111111111111111:refs/heads/other";
    let cases = [
        // Only an unrelated ref is acknowledged.
        line("!", other, "[rejected] (non-fast-forward)"),
        String::new(),
        // Two lines for the exact target are ambiguous even when they agree.
        format!(
            "{}{}",
            line("!", "@REF@", "[rejected] (stale info)"),
            line("!", "@REF@", "[rejected] (stale info)")
        ),
        // Missing summary, wrong flag width, wrong commit for the target.
        format!("!{TAB}@REF@\n"),
        line("!!", "@REF@", "[rejected] (stale info)"),
        line(
            "!",
            "2222222222222222222222222222222222222222:refs/heads/main",
            "[rejected] (stale info)",
        ),
    ];
    for stdout in cases {
        let mut f = Fixture::new();
        let log = canned_push(&mut f, &stdout, "");
        let publish = f.publish();
        let error = f.service.execute(publish.clone()).error.unwrap();
        assert_eq!(error.code, "operation_indeterminate", "{stdout:?}");
        assert!(!error.message.contains("rejection_class"));
        // Replay never redispatches to obtain a classification.
        let replay = f.service.execute(publish).error.unwrap();
        assert_eq!(replay.code, "operation_indeterminate");
        assert_eq!(pushes(&log), 1, "{stdout:?}");
    }
}

#[test]
fn hostile_stderr_and_reason_text_never_reach_responses_state_or_debug() {
    let secret = format!("pw-{}", uuid::Uuid::new_v4().simple());
    let url = format!("https://deploy:{secret}@example.invalid/org/repo.git");
    let hook = format!("hook-message-{secret}");
    let stderr = format!("remote: {hook}\nremote: pre-receive hook declined\nfatal: {url}\n");
    let reasons = [
        // Classifiable acknowledgement with hostile stderr.
        "[remote rejected] (pre-receive hook declined)".to_string(),
        // Hostile text inside the acknowledgement itself.
        format!("[remote rejected] (pre-receive hook declined) {url} {hook}"),
        format!("[remote rejected] ({hook})"),
    ];
    for reason in reasons {
        let mut f = Fixture::new();
        canned_push(&mut f, &line("!", "@REF@", &reason), &stderr);
        let publish = f.publish();
        let response = f.service.execute(publish.clone());
        let rendered = format!("{response:?}");
        let result = response.result.clone().unwrap();
        let wire = serde_json::to_string(&response).unwrap();
        for text in [&rendered, &wire] {
            assert!(
                !text.contains(&secret) && !text.contains("example.invalid"),
                "{text}"
            );
        }
        // Exposed fields are a fixed bounded set of typed values.
        let mut keys: Vec<_> = result.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["observed_at", "outcome", "reason", "rejection_class"]
        );
        assert!(RejectionClass::ALL.contains(&result["rejection_class"].as_str().unwrap()));
        assert_eq!(
            result["rejection_class"],
            if reason == "[remote rejected] (pre-receive hook declined)" {
                "hook_declined"
            } else {
                "unknown"
            }
        );
        let replayed = format!("{:?}", f.service.execute(publish));
        assert!(!replayed.contains(&secret));
        // Nothing persisted (publication record, operation journal, any state file) holds it.
        let mut files = Vec::new();
        state_files(f.temp.path().join("state").as_path(), &mut files);
        for path in files {
            let bytes = std::fs::read(&path).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains(&secret)
                    && !text.contains("example.invalid")
                    && !text.contains(&hook),
                "{} retained remote text",
                path.display()
            );
        }
    }
}

#[test]
fn replay_preserves_result_bytes_lease_and_retention_without_redispatch() {
    let mut f = Fixture::new();
    let log = canned_push(&mut f, &line("!", "@REF@", "[rejected] (stale info)"), "");
    let publish = f.publish();
    let first = success(f.service.execute(publish.clone()));
    assert_eq!(first["rejection_class"], "stale_lease");
    let again = success(f.service.execute(publish));
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&again).unwrap()
    );
    assert_eq!(pushes(&log), 1);
    // The remote lease value is untouched and no dependency lingers on the candidate.
    assert_eq!(git(&f.remote, &["rev-parse", "refs/heads/main"]), f.base);
    assert!(
        f.service
            .root
            .retention(&f.candidate)
            .unwrap()
            .unwrap()
            .dependencies
            .is_empty()
    );
    // A later, separately identified operation can still publish: the hold was released.
    f.service.repositories.runner = GitRunner::new("/usr/bin/git".into());
    assert_eq!(
        success(f.service.execute(f.publish()))["outcome"],
        "applied"
    );
}

#[test]
fn historical_results_without_the_field_replay_unchanged() {
    let mut f = Fixture::new();
    let log = canned_push(&mut f, &line("!", "@REF@", "[rejected] (stale info)"), "");
    let publish = f.publish();
    success(f.service.execute(publish.clone()));
    // Rewrite every persisted record as an older build would have written it.
    let mut files = Vec::new();
    state_files(f.temp.path().join("state").as_path(), &mut files);
    let mut rewritten = 0;
    for path in files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
    {
        let text = std::fs::read_to_string(path).unwrap();
        if text.contains("rejection_class") {
            let mut value: Value = serde_json::from_str(&text).unwrap();
            strip_class(&mut value);
            std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            rewritten += 1;
        }
    }
    assert!(rewritten >= 1);
    let Fixture { temp, service, .. } = f;
    drop(service);
    let service = reopen(&temp.path().join("state"));
    let replay = success(service.execute(publish));
    assert_eq!(replay["outcome"], "rejected");
    assert_eq!(replay["reason"], "remote_rejected");
    // Absent means "not recorded": nothing is synthesized, and nothing is re-run.
    assert!(replay.get("rejection_class").is_none(), "{replay}");
    assert_eq!(pushes(&log), 1);
}
fn reopen(state: &Path) -> RepositoryService {
    use std::time::{Duration, Instant};
    // A parallel test's Git child can inherit the owner lock until exec closes its copy.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match RepositoryService::open(state) {
            Err(error) if error.code == "state_busy" && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => break result.expect("reopen state after dropping the owner"),
        }
    }
}
fn strip_class(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("rejection_class");
            map.values_mut().for_each(strip_class);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_class),
        _ => {}
    }
}

#[test]
fn capability_is_additive_and_classes_are_the_closed_v1_set() {
    let f = Fixture::new();
    let capabilities = f.service.capabilities();
    assert_eq!(capabilities["protocol_version"], 1);
    assert_eq!(
        capabilities["capability_tokens"],
        json!([CLASSIFICATION_CAPABILITY])
    );
    assert_eq!(
        CLASSIFICATION_CAPABILITY,
        "publication_rejection_classification_v1"
    );
    assert_eq!(
        capabilities["publication"]["rejection_classes"],
        json!([
            "unknown",
            "non_fast_forward",
            "stale_lease",
            "hook_declined"
        ])
    );
    // Pre-existing advertisement members are unchanged.
    assert_eq!(
        capabilities["publication"]["update_modes"],
        json!(["fast_forward", "replace"])
    );
    assert_eq!(RejectionClass::from_summary(b""), RejectionClass::Unknown);
    assert_eq!(
        serde_json::to_value(RejectionClass::NonFastForward).unwrap(),
        "non_fast_forward"
    );
}
