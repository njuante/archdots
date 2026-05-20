#[path = "common/mod.rs"]
mod common;

use std::sync::atomic::Ordering;

use archdots_core::packages::{AurHelper, PackageDB, PackageError, PkgSource, ProviderHit};

use common::MockRunner;

// ── helpers ────────────────────────────────────────────────────────────────────

fn db(runner: MockRunner) -> PackageDB {
    PackageDB::with_runner(Box::new(runner)).expect("PackageDB construction must succeed")
}

// ── PackageDB: is_installed / lookup ──────────────────────────────────────────

#[test]
fn is_installed_returns_true_for_present_package() {
    let d = db(MockRunner::new().expect_pacman_q("foo 1.0\nbar 2.0\n"));
    assert!(d.is_installed("foo").unwrap());
}

#[test]
fn is_installed_returns_false_for_absent_package() {
    let d = db(MockRunner::new().expect_pacman_q("foo 1.0\nbar 2.0\n"));
    assert!(!d.is_installed("baz").unwrap());
}

#[test]
fn lookup_returns_pkg_for_known_package() {
    let d = db(MockRunner::new()
        .expect_pacman_q("foo 1.0\nbar 2.0\n")
        .expect_pacman_qm(""));
    let pkg = d.lookup("foo").unwrap().expect("foo must be found");
    assert_eq!(pkg.name, "foo");
    assert_eq!(pkg.version, "1.0");
    assert_eq!(pkg.source, PkgSource::Repo);
}

#[test]
fn lookup_returns_none_for_absent_package() {
    let d = db(MockRunner::new()
        .expect_pacman_q("foo 1.0\n")
        .expect_pacman_qm(""));
    assert!(d.lookup("nothere").unwrap().is_none());
}

// ── PackageDB: caching ─────────────────────────────────────────────────────────

#[test]
fn installed_caches_result_and_calls_runner_once() {
    let runner = MockRunner::new().expect_pacman_q("foo 1.0\n");
    let counter = runner.call_counter.clone();
    let d = db(runner);

    d.installed().unwrap();
    d.installed().unwrap();

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "pacman -Q must be called exactly once despite two installed() calls"
    );
}

#[test]
fn aur_packages_caches_and_calls_runner_once() {
    let runner = MockRunner::new()
        .expect_pacman_q("eww 0.6.0-1\n")
        .expect_pacman_qm("eww 0.6.0-1\n");
    let counter = runner.call_counter.clone();
    let d = db(runner);

    d.aur_packages().unwrap();
    d.aur_packages().unwrap();

    // call 1 = pacman -Q (triggered by aur_packages → installed), call 2 = pacman -Qm
    // Second aur_packages() must not add a third call.
    let total = counter.load(Ordering::SeqCst);
    assert!(total <= 2, "expected ≤2 total runner calls, got {total}");
}

// ── PackageDB: AUR source classification ──────────────────────────────────────

#[test]
fn lookup_package_in_aur_returns_aur_source() {
    let d = db(MockRunner::new()
        .expect_pacman_q("eww 0.6.0-1\n")
        .expect_pacman_qm("eww 0.6.0-1\n"));
    let pkg = d.lookup("eww").unwrap().expect("eww must be found");
    assert_eq!(pkg.source, PkgSource::Aur);
}

#[test]
fn lookup_package_only_in_pacman_q_returns_repo_source() {
    let d = db(MockRunner::new()
        .expect_pacman_q("kitty 0.32.0-1\n")
        .expect_pacman_qm(""));
    let pkg = d.lookup("kitty").unwrap().expect("kitty must be found");
    assert_eq!(pkg.source, PkgSource::Repo);
}

// ── PackageDB: AUR helper detection ───────────────────────────────────────────

#[test]
fn detect_aur_helper_paru_installed_returns_paru() {
    let d = db(MockRunner::new().expect_pacman_q("paru 2.0.0-1\n"));
    assert_eq!(d.detect_aur_helper().unwrap(), Some(AurHelper::Paru));
}

#[test]
fn detect_aur_helper_yay_installed_returns_yay() {
    let d = db(MockRunner::new().expect_pacman_q("yay 12.3.0-1\n"));
    assert_eq!(d.detect_aur_helper().unwrap(), Some(AurHelper::Yay));
}

#[test]
fn detect_aur_helper_neither_returns_none() {
    let d = db(MockRunner::new().expect_pacman_q("rofi 1.7.5-1\n"));
    assert_eq!(d.detect_aur_helper().unwrap(), None);
}

#[test]
fn detect_aur_helper_both_installed_prefers_paru() {
    let d = db(MockRunner::new().expect_pacman_q("paru 2.0.0-1\nyay 12.3.0-1\n"));
    assert_eq!(d.detect_aur_helper().unwrap(), Some(AurHelper::Paru));
}

// ── PackageDB: provider_of — curated hits ─────────────────────────────────────

#[test]
fn provider_of_curated_binary_returns_curated_hit() {
    // rofi is in the curated table with source=repo; -Qm returns nothing
    let d = db(MockRunner::new().expect_pacman_qm(""));
    if let ProviderHit::Curated { package, source } = d.provider_of("rofi", false).unwrap() {
        assert_eq!(package, "rofi");
        assert_eq!(source, PkgSource::Repo);
    } else {
        panic!("expected ProviderHit::Curated for rofi");
    }
}

#[test]
fn provider_of_unknown_binary_shallow_returns_unknown() {
    let d = db(MockRunner::new());
    assert_eq!(
        d.provider_of("xyz123_not_a_real_binary", false).unwrap(),
        ProviderHit::Unknown
    );
}

// ── PackageDB: provider_of — deep (pacman -F) ─────────────────────────────────

#[test]
fn provider_of_deep_resolves_via_pacman_f() {
    let d = db(MockRunner::new().expect_pacman_qm("").expect_pacman_f(
        "xyz123",
        "extra/xyz123-tools 1.0 [installed]\n    usr/bin/xyz123\n",
        "",
    ));
    if let ProviderHit::PacmanFiles { package, source } = d.provider_of("xyz123", true).unwrap() {
        assert_eq!(package, "xyz123-tools");
        assert_eq!(source, PkgSource::Repo);
    } else {
        panic!("expected ProviderHit::PacmanFiles");
    }
}

#[test]
fn provider_of_deep_files_db_not_synced() {
    let d = db(MockRunner::new().expect_pacman_f("xyz123", "", "error: files database is empty"));
    assert_eq!(
        d.provider_of("xyz123", true).unwrap(),
        ProviderHit::FilesDbNotSynced
    );
}

#[test]
fn provider_of_deep_no_match_returns_unknown() {
    let d = db(MockRunner::new()
        .expect_pacman_qm("")
        .expect_pacman_f("xyz123", "", ""));
    assert_eq!(d.provider_of("xyz123", true).unwrap(), ProviderHit::Unknown);
}

// ── PackageDB: error handling ─────────────────────────────────────────────────

#[test]
fn pacman_q_nonzero_exit_returns_pacman_failed() {
    use archdots_core::packages::CommandOutput;

    let runner = MockRunner::new().expect(
        "pacman",
        &["-Q"],
        CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "database error".to_string(),
        },
    );
    let d = db(runner);
    let err = d.installed().unwrap_err();
    assert!(
        matches!(err, PackageError::PacmanFailed { code: 1, .. }),
        "expected PacmanFailed, got: {err:?}"
    );
}

#[test]
fn pacman_q_malformed_line_returns_unparseable_output() {
    let d = db(MockRunner::new().expect_pacman_q("foo\n")); // no version token
    let err = d.installed().unwrap_err();
    assert!(
        matches!(err, PackageError::UnparseableOutput(ref s) if s == "foo"),
        "expected UnparseableOutput with the offending line, got: {err:?}"
    );
}

#[test]
#[ignore = "requires creating /var/lib/pacman/db.lck or injecting the lock path; \
            marked ignored to avoid coupling production code to test infrastructure"]
fn database_locked_returns_database_locked_error() {
    // To exercise this path: create a temporary lock file and inject its path
    // into PackageDB via a future builder method. Until then, test by reading
    // the production behaviour at the system level on an Arch host.
    unimplemented!()
}

// ── curated table sanity via PackageDB surface ────────────────────────────────

#[test]
fn curated_table_has_at_least_30_entries() {
    let d = db(MockRunner::new());
    assert!(
        d.curated_binary_count() >= 30,
        "expected ≥30 curated entries, got {}",
        d.curated_binary_count()
    );
}
