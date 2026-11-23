use super::*;
#[test]
fn prefix_head_branch_is_created_then_replaced_under_exact_lease() {
    let f = Fixture::new();
    let head = "refs/heads/aq/pr-1";
    let zero = "0".repeat(40);
    let create = publish_to(&f, &f.candidate, (head, &zero), "fast_forward");
    assert_eq!(success(f.service.execute(create))["outcome"], "applied");
    assert_eq!(git(&f.remote, &["rev-parse", head]), f.source);
    // A rebuilt candidate on an unrelated line is not a fast-forward of the old head.
    let rebuilt = f.candidate_from(&f.other);
    let ff = publish_to(&f, &rebuilt, (head, &f.source), "fast_forward");
    assert_eq!(f.service.execute(ff).error.unwrap().code, "target_moved");
    let replace = publish_to(&f, &rebuilt, (head, &f.source), "replace");
    let applied = success(f.service.execute(replace.clone()));
    assert_eq!(applied["outcome"], "applied");
    assert_changed_commits(&applied, [&f.source, &f.other]);
    assert_eq!(git(&f.remote, &["rev-parse", head]), f.other);
    let spawns = f.service.repositories.runner.metrics().spawns;
    assert_eq!(applied, success(f.service.execute(replace)));
    assert_eq!(f.service.repositories.runner.metrics().spawns, spawns);
    // Replacement presence is exact identity, never containment.
    let again = publish_to(&f, &rebuilt, (head, &f.source), "replace");
    assert_eq!(
        success(f.service.execute(again))["outcome"],
        "already_present"
    );
    let stale = publish_to(&f, &f.candidate, (head, &f.source), "replace");
    let moved = success(f.service.execute(stale));
    assert_rejected_move(&moved, &f.other);
    assert_eq!(git(&f.remote, &["rev-parse", head]), f.other);
    let back = publish_to(&f, &f.candidate, (head, &f.other), "replace");
    assert_eq!(success(f.service.execute(back))["outcome"], "applied");
    assert_eq!(git(&f.remote, &["rev-parse", head]), f.source);
    assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.base);
}

#[test]
fn replace_is_refused_for_exact_targets_and_unscoped_branches_before_git() {
    let f = Fixture::new();
    let before = f.service.repositories.runner.metrics().spawns;
    for (target, update) in [
        ("refs/heads/main", "replace"),
        ("refs/heads/new", "replace"),
        ("refs/heads/aqx/pr-1", "fast_forward"),
        ("refs/heads/aq", "fast_forward"),
    ] {
        let publish = publish_to(&f, &f.candidate, (target, &f.base), update);
        assert_eq!(
            f.service.execute(publish).error.unwrap().code,
            "unsupported_feature",
            "{target} {update}"
        );
    }
    let mut unknown = publish_to(&f, &f.candidate, ("refs/heads/aq/pr-1", &f.base), "force");
    unknown.operation_id = Some(uuid::Uuid::new_v4().to_string());
    assert!(!f.service.execute(unknown).ok);
    assert_eq!(f.service.repositories.runner.metrics().spawns, before);
    assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.base);
}

#[test]
fn target_prefixes_must_be_central_branch_namespaces() {
    let temp = tempfile::tempdir().unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    for (mode, prefix) in [
        ("central", "refs/heads/"),
        ("central", "refs/heads/aq"),
        ("central", "refs/tags/aq/"),
        ("central", "refs/heads/a..q/"),
        ("worker", "refs/heads/aq/"),
    ] {
        let repo = uuid::Uuid::new_v4().to_string();
        let error = f_error(service.execute(request(
            &repo,
            "repo.register",
            json!({"identity":"fixture","mode":mode,"allowed_target_prefixes":[prefix]}),
        )));
        assert_eq!(error, "invalid_request", "{mode} {prefix}");
    }
    let repo = uuid::Uuid::new_v4().to_string();
    let shown = success(service.execute(request(
        &repo,
        "repo.register",
        json!({"identity":"fixture","mode":"central","allowed_target_prefixes":["refs/heads/aq/"]}),
    )));
    assert_eq!(shown["allowed_target_prefixes"], json!(["refs/heads/aq/"]));
}

fn f_error(response: Response) -> String {
    response.error.expect("refusal").code
}

#[test]
fn lost_replace_acknowledgement_reconciles_only_on_exact_head() {
    let mut f = Fixture::new();
    let head = "refs/heads/aq/pr-2";
    let create = publish_to(&f, &f.candidate, (head, &"0".repeat(40)), "fast_forward");
    assert_eq!(success(f.service.execute(create))["outcome"], "applied");
    let rebuilt = f.candidate_from(&f.other);
    f.suppress_push_output();
    let replace = publish_to(&f, &rebuilt, (head, &f.source), "replace");
    let id = replace.operation_id.clone().unwrap();
    assert_eq!(
        f.service.execute(replace).error.unwrap().code,
        "operation_indeterminate"
    );
    assert_eq!(git(&f.remote, &["rev-parse", head]), f.other);
    // A descendant contains the candidate, but a replacement requires exact identity.
    let tree = format!("{}^{{tree}}", f.other);
    let descendant = git(
        &f.remote,
        &["commit-tree", &tree, "-p", &f.other, "-m", "later"],
    );
    git(&f.remote, &["update-ref", head, &descendant]);
    let reconcile = |f: &Fixture| {
        let target = f.service.root.operation(&id).unwrap().unwrap();
        success(f.service.execute(request(
            &f.repo,
            "operation.reconcile",
            json!({"target_operation_id":id,"expected_generation":target.generation}),
        )))
    };
    assert_eq!(reconcile(&f)["outcome"], "indeterminate");
    git(&f.remote, &["update-ref", head, &f.other]);
    let result = reconcile(&f);
    assert_eq!(result["resolution"]["outcome"], "already_present");
    assert_eq!(result["resolution"]["observed_target"], oid(&f.other));
}

pub(super) fn remote_has(f: &Fixture, name: &str) -> bool {
    !git(&f.remote, &["for-each-ref", name]).is_empty()
}
fn pool_has(f: &Fixture, name: &str) -> bool {
    !git(
        &f.service.repositories.pool_path(&f.repo),
        &["for-each-ref", name],
    )
    .is_empty()
}

#[test]
fn ref_delete_removes_prefix_branch_locally_and_on_remote_idempotently() {
    let f = Fixture::new();
    let head = "refs/heads/aq/task-1";
    publish_head(&f, head);
    git(
        &f.service.repositories.pool_path(&f.repo),
        &["update-ref", head, &f.source],
    );
    let delete = delete_ref(&f, head, Some(&f.source), true);
    let done = success(f.service.execute(delete.clone()));
    assert_delete_outcomes(&done, "deleted");
    assert_eq!(done["old"], oid(&f.source));
    assert!(!remote_has(&f, head) && !pool_has(&f, head));
    // Sibling branches and the retained candidate are untouched.
    assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.base);
    // Replay of the same operation returns the recorded result without Git.
    let spawns = f.service.repositories.runner.metrics().spawns;
    assert_eq!(done, success(f.service.execute(delete)));
    assert_eq!(f.service.repositories.runner.metrics().spawns, spawns);
    // A new operation on an already-deleted branch is success, with or without a lease.
    for old in [Some(f.source.as_str()), None] {
        let again = success(f.service.execute(delete_ref(&f, head, old, true)));
        assert_delete_outcomes(&again, "already_absent");
    }
}

#[test]
fn ref_delete_without_remote_leaves_remote_and_null_lease_deletes_observed_head() {
    let f = Fixture::new();
    let head = "refs/heads/aq/task-2";
    publish_head(&f, head);
    git(
        &f.service.repositories.pool_path(&f.repo),
        &["update-ref", head, &f.source],
    );
    let local = success(f.service.execute(delete_ref(&f, head, None, false)));
    assert_eq!(local["outcome"], "deleted");
    assert_eq!(local["remote"]["outcome"], "not_requested");
    assert!(remote_has(&f, head) && !pool_has(&f, head));
    let remote = success(f.service.execute(delete_ref(&f, head, None, true)));
    assert_eq!(remote["outcome"], "deleted");
    assert_eq!(remote["local"]["outcome"], "already_absent");
    assert!(!remote_has(&f, head));
}

#[test]
fn ref_delete_refuses_exact_targets_and_refs_outside_prefixes_before_git() {
    let f = Fixture::new();
    let before = f.service.repositories.runner.metrics().spawns;
    for name in [
        "refs/heads/main",
        "refs/heads/new",
        "refs/heads/source",
        "refs/heads/aqx/task",
        "refs/tags/aq/task",
        "refs/aq-git/transfers/x",
    ] {
        for remote in [true, false] {
            let error = f
                .service
                .execute(delete_ref(&f, name, Some(&f.base), remote))
                .error
                .unwrap();
            assert_eq!(error.code, "unsupported_feature", "{name}");
        }
    }
    assert_eq!(f.service.repositories.runner.metrics().spawns, before);
    assert_eq!(git(&f.remote, &["rev-parse", "main"]), f.base);
    assert_eq!(git(&f.remote, &["rev-parse", "source"]), f.source);
}

#[test]
fn ref_delete_lease_mismatch_is_refused_without_effect() {
    let f = Fixture::new();
    let head = "refs/heads/aq/task-3";
    publish_head(&f, head);
    let pool = f.service.repositories.pool_path(&f.repo);
    git(&pool, &["update-ref", head, &f.source]);
    // Wrong remote and local lease.
    let wrong = f
        .service
        .execute(delete_ref(&f, head, Some(&f.other), true))
        .error
        .unwrap();
    assert_eq!(wrong.code, "target_moved");
    assert!(remote_has(&f, head) && pool_has(&f, head));
    // Local head differs from the lease although the remote matches: nothing is deleted.
    git(&pool, &["update-ref", head, &f.other]);
    let local = f
        .service
        .execute(delete_ref(&f, head, Some(&f.source), true))
        .error
        .unwrap();
    assert_mismatch_side(&local, "local");
    assert!(remote_has(&f, head) && pool_has(&f, head));
    // Remote moved by another actor.
    git(&pool, &["update-ref", head, &f.source]);
    git(&f.remote, &["update-ref", head, &f.other]);
    let remote = f
        .service
        .execute(delete_ref(&f, head, Some(&f.source), true))
        .error
        .unwrap();
    assert_mismatch_side(&remote, "remote");
    assert_eq!(git(&f.remote, &["rev-parse", head]), f.other);
    assert_eq!(git(&pool, &["rev-parse", head]), f.source);
}

#[test]
fn ref_delete_requires_central_binding_with_remote() {
    let temp = tempfile::tempdir().unwrap();
    let service = RepositoryService::open(temp.path().join("state")).unwrap();
    let repo = uuid::Uuid::new_v4().to_string();
    success(service.execute(request(
        &repo,
        "repo.register",
        json!({"identity":"fixture","mode":"central","allowed_target_prefixes":["refs/heads/aq/"]}),
    )));
    let delete = request(
        &repo,
        "ref.delete",
        json!({"ref":"refs/heads/aq/x","expected_old":null,"remote":true}),
    );
    assert_eq!(
        service.execute(delete).error.unwrap().code,
        "unsupported_feature"
    );
    let local = request(
        &repo,
        "ref.delete",
        json!({"ref":"refs/heads/aq/x","expected_old":null,"remote":false}),
    );
    assert_eq!(success(service.execute(local))["outcome"], "already_absent");
}
fn assert_changed_commits(outcome: &Value, [old, new]: [&str; 2]) {
    assert_eq!(outcome["old"], oid(old));
    assert_eq!(outcome["new"], oid(new));
}
fn assert_rejected_move(outcome: &Value, observed: &str) {
    assert_eq!(outcome["outcome"], "rejected");
    assert_eq!(outcome["reason"], "target_moved");
    assert_eq!(outcome["observed_target"], oid(observed));
}
fn assert_delete_outcomes(outcome: &Value, expected: &str) {
    assert_eq!(outcome["outcome"], expected);
    assert_eq!(outcome["remote"]["outcome"], expected);
    assert_eq!(outcome["local"]["outcome"], expected);
}
fn assert_mismatch_side(error: &aq_git_protocol::ProtocolError, side: &str) {
    assert_eq!(error.code, "target_moved");
    assert_eq!(error.details["side"], side);
}
