// SPDX-License-Identifier: Apache-2.0
//
// The support matrix is a projection of the registry, not a document.
//
// The golden file is the format contract: release notes ship this text, so
// a change to it is a change to what the project publicly claims and should
// be visible in review as a diff rather than discovered after a release.
//
// Regenerate with:
//   UPDATE_GOLDEN=1 cargo test -p tensorplate-platform --test support_matrix

#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use tensorplate_platform::{render_support_matrix, PlatformRegistry, SupportLevel};

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn committed_registry() -> PlatformRegistry {
    PlatformRegistry::load(&repo_path("config/platform")).expect("registry loads")
}

/// Compare against a golden file, rewriting it when `UPDATE_GOLDEN` is set.
fn assert_golden(path: &Path, rendered: &str) {
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, rendered).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{} is missing ({e}); regenerate with UPDATE_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        rendered,
        expected,
        "{} is stale — regenerate with `UPDATE_GOLDEN=1 cargo test -p tensorplate-platform --test support_matrix`",
        path.display()
    );
}

#[test]
fn the_published_matrix_matches_the_registry() {
    // The committed document is what ships in release notes. If it drifts
    // from the rows, the release claims support the software does not
    // enforce.
    assert_golden(
        &repo_path("docs/release/support-matrix.md"),
        &render_support_matrix(&committed_registry()),
    );
}

#[test]
fn planned_rows_are_listed_without_being_counted_as_supported() {
    let registry = committed_registry();
    let rendered = render_support_matrix(&registry);
    let supported = registry.supported_rows().count();

    assert!(
        rendered.contains(&format!("{supported} supported combination(s)")),
        "the headline count comes from the registry, not the prose"
    );
    for row in registry.rows() {
        assert!(
            rendered.contains(row.row_id()),
            "row `{}` must appear somewhere in the matrix",
            row.row_id()
        );
    }

    // Every Planned row is under the Planned heading, and none of them is
    // above it among the supported combinations.
    let planned_at = rendered.find("## Planned").expect("a Planned section");
    for row in registry
        .rows()
        .filter(|row| row.support_level() == SupportLevel::Planned)
    {
        let at = rendered
            .find(row.row_id())
            .expect("planned row is rendered");
        assert!(
            at > planned_at,
            "planned row `{}` must not appear among supported combinations",
            row.row_id()
        );
    }
}

#[test]
fn roadmap_targets_render_outside_support_and_count_toward_nothing() {
    let registry = committed_registry();
    let rendered = render_support_matrix(&registry);

    let roadmap_at = rendered
        .find("## Roadmap targets (not supported)")
        .expect("a roadmap section");
    let supported_at = rendered
        .find("## Supported combinations")
        .expect("a supported section");
    assert!(roadmap_at > supported_at);

    for target in registry.roadmap_targets() {
        let at = rendered
            .find(target.target_id())
            .unwrap_or_else(|| panic!("roadmap target `{}` is rendered", target.target_id()));
        assert!(
            at > roadmap_at,
            "roadmap target `{}` must render only in the non-support section",
            target.target_id()
        );
    }

    // The headline count is Production + Preview only.
    let supported = registry.supported_rows().count();
    assert!(rendered.contains(&format!("{supported} supported combination(s)")));
    assert_eq!(
        supported + registry.roadmap_targets().count(),
        supported + 4,
        "four roadmap targets exist and none of them is a supported combination"
    );
}

#[test]
fn an_experimental_row_renders_in_its_own_excluded_section() {
    // No Experimental row exists in this release, but the value is frozen
    // in the schema. Rendering it here means the format is reviewed now
    // rather than appearing unreviewed in a release note the day a row
    // first uses it.
    let base = std::fs::read_to_string(repo_path("config/platform/rows/ubuntu2404-x86-cpu.json"))
        .expect("read a committed row");
    let mut document: serde_json::Value = serde_json::from_str(&base).expect("row parses");
    document["row_id"] = serde_json::json!("ubuntu2404-x86-experimental");
    document["support_level"] = serde_json::json!("Experimental");
    // The OS and the environment move together. Bumping only the version
    // would leave the copied environment describing 24.04, so the fixture
    // meant to define Experimental rendering would publish a row whose
    // own two cells disagree — and a future environment-projection
    // regression could hide behind that inconsistency.
    document["os"]["version"] = serde_json::json!("25.04");
    document["validation_environment"]["identity"] =
        serde_json::json!("Any x86_64 Ubuntu 25.04 host");
    let experimental = serde_json::to_string(&document).expect("serialize");

    let rows = std::fs::read_dir(repo_path("config/platform/rows"))
        .expect("read rows")
        .map(|entry| {
            let path = entry.expect("entry").path();
            (path.clone(), std::fs::read_to_string(&path).expect("read"))
        })
        .collect::<Vec<_>>();
    let targets = std::fs::read_dir(repo_path("config/platform/roadmap_targets"))
        .expect("read targets")
        .map(|entry| {
            let path = entry.expect("entry").path();
            (path.clone(), std::fs::read_to_string(&path).expect("read"))
        })
        .collect::<Vec<_>>();

    let synthetic = PathBuf::from("rows/ubuntu2404-x86-experimental.json");
    let registry = PlatformRegistry::from_documents(
        rows.iter()
            .map(|(p, b)| (p.as_path(), b.as_str()))
            .chain(std::iter::once((
                synthetic.as_path(),
                experimental.as_str(),
            ))),
        targets.iter().map(|(p, b)| (p.as_path(), b.as_str())),
    )
    .expect("a registry with one experimental row loads");

    let rendered = render_support_matrix(&registry);
    assert_golden(
        &repo_path("test/platform/support_matrix_experimental.golden.md"),
        &rendered,
    );

    // The claim that matters: it is listed, and it is not supported.
    let experimental_at = rendered.find("## Experimental").expect("a section");
    let at = rendered
        .find("ubuntu2404-x86-experimental")
        .expect("the row is listed");
    assert!(at > experimental_at, "listed under Experimental");
    assert!(
        rendered.contains("Experimental rows are **not** supported combinations"),
        "the section says so in words, not only by placement"
    );
    // Derived, not hard-coded: the documented regeneration step is run
    // exactly when the registry changes, so a literal here would make that
    // step fail on the one change that requires it — rewriting the goldens
    // and then exiting non-zero on an unrelated assertion.
    let baseline = committed_registry().supported_rows().count();
    assert_eq!(
        registry.supported_rows().count(),
        baseline,
        "an experimental row must not change the supported total"
    );
    assert!(rendered.contains(&format!("{baseline} supported combination(s)")));
}

#[test]
fn a_pipe_in_free_prose_cannot_forge_a_table_cell() {
    // `validation_environment.identity` and the accelerator SKU are free
    // prose the schema does not constrain. An unescaped pipe ends the cell
    // early, shifting every later value one column left, so a reader sees
    // a support claim attached to the wrong field.
    let base = std::fs::read_to_string(repo_path("config/platform/rows/ubuntu2404-x86-cpu.json"))
        .expect("read a committed row");
    let mut document: serde_json::Value = serde_json::from_str(&base).expect("row parses");
    document["row_id"] = serde_json::json!("ubuntu2404-x86-hostile");
    document["os"]["version"] = serde_json::json!("25.10");
    document["validation_environment"]["identity"] =
        serde_json::json!("8x H100 | 2TB RAM | Production ready");
    let hostile = serde_json::to_string(&document).expect("serialize");

    let path = PathBuf::from("rows/ubuntu2404-x86-hostile.json");
    let registry =
        PlatformRegistry::from_documents([(path.as_path(), hostile.as_str())], std::iter::empty())
            .expect("the row loads");

    let rendered = render_support_matrix(&registry);
    let row_line = rendered
        .lines()
        .find(|line| line.contains("ubuntu2404-x86-hostile"))
        .expect("the row renders");
    assert_eq!(
        row_line.matches(" | ").count(),
        rendered
            .lines()
            .find(|line| line.starts_with("| Row |"))
            .expect("a header")
            .matches(" | ")
            .count(),
        "the row must have exactly the header's column count: {row_line}"
    );
    assert!(
        row_line.contains(r"8x H100 \| 2TB RAM \| Production ready"),
        "the pipes are escaped, not dropped: {row_line}"
    );
}
