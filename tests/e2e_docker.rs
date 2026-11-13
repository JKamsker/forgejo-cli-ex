//! Docker E2E tests: one `#[ignore]`d test per server target.
//!
//! ```sh
//! FJ_EX_E2E=1 cargo test --locked --test e2e_docker e2e_forgejo_17_docker -- --ignored --nocapture
//! ```
//!
//! The target table lives in `tests/e2e/harness/targets.rs`; see
//! `tests/e2e/README.md` for all options.

mod e2e;

macro_rules! e2e_target {
    ($test:ident, $id:literal) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        #[ignore = "needs Docker; run with FJ_EX_E2E=1 and --ignored"]
        async fn $test() -> eyre::Result<()> {
            e2e::run_target($id).await
        }
    };
}

e2e_target!(e2e_forgejo_15_docker, "forgejo-15");
e2e_target!(e2e_forgejo_16_docker, "forgejo-16");
e2e_target!(e2e_forgejo_17_docker, "forgejo-17");
e2e_target!(e2e_gitea_1_25_docker, "gitea-1-25");
e2e_target!(e2e_gitea_1_26_docker, "gitea-1-26");
e2e_target!(e2e_gitea_1_27_docker, "gitea-1-27");

/// Every target in the table has a test above and a well-formed definition.
#[test]
fn target_table_is_consistent() {
    use e2e::harness::targets::{test_name, Family, TARGETS};
    let source = include_str!("e2e_docker.rs");
    let mut ids = std::collections::HashSet::new();
    for t in TARGETS {
        assert!(ids.insert(t.id), "duplicate target id {}", t.id);
        let line = format!("e2e_target!({}, \"{}\");", test_name(t.id), t.id);
        assert!(
            source.lines().any(|l| l.trim() == line),
            "target {} needs `{line}` in tests/e2e_docker.rs",
            t.id
        );
        assert!(
            t.image.ends_with(&format!(":{}", t.version)),
            "{}: image tag must pin the version",
            t.id
        );
        let expected_dir = match t.family {
            Family::Forgejo => ".forgejo",
            Family::Gitea => ".gitea",
        };
        assert_eq!(t.workflow_dir, expected_dir, "{}: workflow dir", t.id);
        assert!(
            t.runner_image
                .rsplit(':')
                .next()
                .is_some_and(|tag| tag.chars().next().is_some_and(|c| c.is_ascii_digit())),
            "{}: runner image must be pinned to a version",
            t.id
        );
    }
    // Every e2e_target! line names a target from the table.
    for l in source.lines().map(str::trim) {
        if let Some(rest) = l.strip_prefix("e2e_target!(") {
            let id = rest.split('"').nth(1).unwrap_or_default();
            assert!(
                TARGETS.iter().any(|t| t.id == id),
                "`{l}` names no target in TARGETS"
            );
        }
    }
    // CI matrix: each `target:` is a known id and is followed by its own `test_name:`.
    let ci = include_str!("../.github/workflows/ci.yml");
    let lines: Vec<&str> = ci.lines().map(str::trim).collect();
    let mut matrix = std::collections::HashMap::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(id) = l.strip_prefix("target: ") else {
            continue;
        };
        let id = id.trim();
        assert!(
            TARGETS.iter().any(|t| t.id == id),
            "CI matrix target {id:?} is not in TARGETS"
        );
        let name = lines[i + 1..]
            .iter()
            .take_while(|l| !l.starts_with("- "))
            .find_map(|l| l.strip_prefix("test_name: "))
            .map(str::trim);
        assert_eq!(
            name,
            Some(test_name(id).as_str()),
            "CI matrix entry for {id} must run its own test"
        );
        assert!(
            matrix.insert(id.to_string(), ()).is_none(),
            "duplicate CI matrix target {id}"
        );
    }
    for t in TARGETS {
        assert!(
            matrix.contains_key(t.id),
            "target {} is missing from the CI e2e matrix",
            t.id
        );
    }
}
