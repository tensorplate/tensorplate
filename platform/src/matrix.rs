// SPDX-License-Identifier: Apache-2.0
//
// The support matrix, projected from the registry.
//
// Release notes used to state supported platforms in hand-written prose,
// which drifts: the rows are what `doctor` matches against and what deploy
// admission enforces, so a sentence in a release note that disagrees with
// them is a promise the software will not keep. This renders that section
// from the same rows, so the two cannot disagree.
//
// The projection is deliberately lossy in one direction only. It never
// invents a claim the rows do not make: Planned rows are listed as planned
// and nothing more, Experimental rows render in their own section outside
// the supported set, and roadmap targets — which are not rows and are
// never matched — render in a separate non-support appendix that no count
// includes.

use std::fmt::Write as _;

use crate::registry::PlatformRegistry;
use crate::roadmap::RoadmapTarget;
use crate::row::{PlatformSupportRow, SupportLevel};

/// Render the support matrix as deterministic Markdown.
///
/// Ordering is the registry's own (row id, then target id), so the output
/// is diffable: a row changing support level produces a one-line diff
/// rather than a reshuffle.
#[must_use]
pub fn render_support_matrix(registry: &PlatformRegistry) -> String {
    let mut out = String::new();
    out.push_str("# Platform support matrix\n\n");
    out.push_str(
        "Generated from `config/platform/`. Do not edit by hand — regenerate\n\
         with `UPDATE_GOLDEN=1 cargo test -p tensorplate-platform --test support_matrix`.\n\n",
    );

    let by_level = |level: SupportLevel| {
        registry
            .rows()
            .filter(move |row| row.support_level() == level)
            .collect::<Vec<_>>()
    };

    let production = by_level(SupportLevel::Production);
    let preview = by_level(SupportLevel::Preview);
    let experimental = by_level(SupportLevel::Experimental);
    let planned = by_level(SupportLevel::Planned);

    out.push_str("## Supported combinations\n\n");
    let _ = writeln!(
        out,
        "{} supported combination(s): {} Production, {} Preview.\n",
        production.len() + preview.len(),
        production.len(),
        preview.len()
    );
    write_supported_section(&mut out, "Production", &production);
    write_supported_section(&mut out, "Preview", &preview);

    // Experimental is a frozen schema value with no rows in this release.
    // Rendering it unconditionally means the value has defined output the
    // day a row first uses it, rather than an unreviewed format appearing
    // in a release note.
    out.push_str("## Experimental\n\n");
    out.push_str(
        "Experimental rows are **not** supported combinations. They are\n\
         listed for visibility only; no support claim attaches to them.\n\n",
    );
    if experimental.is_empty() {
        out.push_str("_No experimental rows in this release._\n\n");
    } else {
        write_row_table(&mut out, &experimental, true);
    }

    out.push_str("## Planned\n\n");
    out.push_str(
        "Planned rows are defined but not validated. They carry no evidence\n\
         and are excluded from supported combinations.\n\n",
    );
    if planned.is_empty() {
        out.push_str("_No planned rows in this release._\n\n");
    } else {
        write_row_table(&mut out, &planned, false);
    }

    write_roadmap_section(&mut out, &registry.roadmap_targets().collect::<Vec<_>>());
    out
}

fn write_supported_section(out: &mut String, label: &str, rows: &[&PlatformSupportRow]) {
    let _ = writeln!(out, "### {label}\n");
    if rows.is_empty() {
        let _ = writeln!(out, "_No {} rows in this release._\n", label.to_lowercase());
        return;
    }
    write_row_table(out, rows, true);
}

/// One table of rows. `with_model_classes` is off for Planned rows, which
/// carry no model-class claim — an empty column would read as "no models
/// supported" rather than "no claim made".
fn write_row_table(out: &mut String, rows: &[&PlatformSupportRow], with_model_classes: bool) {
    if with_model_classes {
        out.push_str("| Row | OS | CPU | Accelerator | Validated on | Model classes |\n");
        out.push_str("| --- | --- | --- | --- | --- | --- |\n");
    } else {
        out.push_str("| Row | OS | CPU | Accelerator | Validated on |\n");
        out.push_str("| --- | --- | --- | --- | --- |\n");
    }
    for row in rows {
        let os = match &row.os().image_identity {
            Some(image) => format!("{} {} ({image})", row.os().name, row.os().version),
            None => format!("{} {}", row.os().name, row.os().version),
        };
        let vendors = row
            .cpu()
            .vendors
            .iter()
            .map(|vendor| vendor.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let cpu = format!("{} ({vendors})", row.cpu().architecture.as_str());
        let accelerator = row
            .accelerator()
            .map_or_else(|| "none".to_string(), |a| a.sku.clone());
        let environment = validated_on(row);
        if with_model_classes {
            let _ = writeln!(
                out,
                "| `{}` | {os} | {cpu} | {accelerator} | {environment} | {} |",
                row.row_id(),
                model_classes(row)
            );
        } else {
            let _ = writeln!(
                out,
                "| `{}` | {os} | {cpu} | {accelerator} | {environment} |",
                row.row_id()
            );
        }
    }
    out.push('\n');
}

/// The environment a row's claim is scoped to.
///
/// This is part of the claim, not context: support does not transfer
/// across machine shapes, so a row scoped to `g2-standard-8` supports that
/// shape and no other. Omitting it would let the table read as support for
/// any host with the shown OS, CPU, and accelerator, which is broader than
/// what the registry enforces and broader than the evidence behind it. An
/// abbreviated suffix in a row id is not a support boundary a reader can
/// be expected to decode.
fn validated_on(row: &PlatformSupportRow) -> String {
    match &row.validation_environment().machine_type {
        Some(machine_type) => format!("`{machine_type}` only"),
        None => row.validation_environment().identity.clone(),
    }
}

fn model_classes(row: &PlatformSupportRow) -> String {
    if row.model_class_rows().is_empty() {
        return "—".to_string();
    }
    row.model_class_rows()
        .iter()
        .map(|pointer| {
            format!(
                "`{}` ({})",
                pointer.model_class_row,
                pointer.support_level.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

/// Roadmap targets are not rows: they are never matched against a machine
/// and never count as support. They render last, in their own section, so
/// no reader can mistake an intention for a claim.
fn write_roadmap_section(out: &mut String, targets: &[&RoadmapTarget]) {
    out.push_str("## Roadmap targets (not supported)\n\n");
    out.push_str(
        "These are **not** platform support rows. They are future targets\n\
         that are not exact enough to be rows, are never matched against a\n\
         machine, and count toward no support total.\n\n",
    );
    if targets.is_empty() {
        out.push_str("_No roadmap targets in this release._\n");
        return;
    }
    out.push_str("| Target | Intended release | Blocking dependency |\n");
    out.push_str("| --- | --- | --- |\n");
    for target in targets {
        let _ = writeln!(
            out,
            "| `{}` — {} | {} | {} |",
            target.target_id(),
            target.target(),
            target.intended_release(),
            target.blocking_dependency()
        );
    }
}
