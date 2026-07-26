// SPDX-License-Identifier: Apache-2.0
//
// The platform registry: the loaded set of exact support rows, the
// separate roadmap-target catalog, and the query API that resolves a
// detected machine to a row.
//
// Two rules shape everything here. The registry **fails closed on load**:
// one invalid row means no registry at all, because a partially-loaded
// registry would silently answer "unsupported" for rows that were merely
// unreadable. And roadmap targets live in a **separate catalog** that
// matching never consults, so a target can never be mistaken for support.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::PlatformRegistryError;
use crate::identity::{DetectedPlatform, HostIdentity};
use crate::reason::PlatformReason;
use crate::roadmap::RoadmapTarget;
use crate::row::{PlatformSupportRow, SupportLevel};

/// The outcome of resolving a detected machine against the registry.
///
/// Deliberately not a `Result`: "this is a Planned row" is neither success
/// nor failure, and collapsing it into either loses the distinction
/// between hardware nobody has validated and hardware nobody has heard of.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowMatch<'a> {
    /// The machine matches a row that carries a support claim.
    Supported(&'a PlatformSupportRow),
    /// The machine matches a Planned row exactly: defined, but carrying no
    /// validation evidence. Reported with
    /// [`PlatformReason::RowPlannedNotValidated`].
    PlannedNotValidated(&'a PlatformSupportRow),
    /// The machine matches no row, with the reason it did not.
    Unsupported(PlatformReason),
}

impl RowMatch<'_> {
    /// The matched row, if any. `None` for an unsupported machine.
    #[must_use]
    pub fn row(&self) -> Option<&PlatformSupportRow> {
        match self {
            Self::Supported(row) | Self::PlannedNotValidated(row) => Some(row),
            Self::Unsupported(_) => None,
        }
    }

    /// The typed reason this machine is not a supported combination.
    /// `None` only when the machine matched a row carrying a claim.
    #[must_use]
    pub fn reason(&self) -> Option<PlatformReason> {
        match self {
            Self::Supported(_) => None,
            Self::PlannedNotValidated(_) => Some(PlatformReason::RowPlannedNotValidated),
            Self::Unsupported(reason) => Some(*reason),
        }
    }

    /// Whether deployment may proceed on this machine.
    #[must_use]
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported(_))
    }
}

/// The identity a row matches on. Two rows sharing one would make
/// resolution ambiguous, so the registry rejects that at load time rather
/// than picking a winner at query time.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RowIdentityKey {
    architecture: &'static str,
    os_name: String,
    os_version: String,
    image_identity: Option<String>,
    accelerator_sku: Option<String>,
}

impl RowIdentityKey {
    fn of(row: &PlatformSupportRow) -> Self {
        Self {
            architecture: row.cpu().architecture.as_str(),
            os_name: row.os().name.clone(),
            os_version: row.os().version.clone(),
            image_identity: row.os().image_identity.clone(),
            accelerator_sku: row.accelerator().map(|a| a.sku.clone()),
        }
    }
}

/// The loaded platform registry.
///
/// Obtain one with [`PlatformRegistry::load`] (from the committed registry
/// directory) or [`PlatformRegistry::from_documents`] (from in-memory
/// documents, for tests and for consumers that ship the registry
/// differently). Both fail closed.
#[derive(Clone, Debug)]
pub struct PlatformRegistry {
    rows: BTreeMap<String, PlatformSupportRow>,
    roadmap_targets: BTreeMap<String, RoadmapTarget>,
}

impl PlatformRegistry {
    /// Load the registry from a directory containing `rows/` and
    /// `roadmap_targets/` subdirectories of JSON documents.
    ///
    /// # Errors
    ///
    /// Fails if any document is unreadable or invalid, if two rows share a
    /// row id or a matchable identity, or if a roadmap target collides with
    /// a row id. One bad document means no registry: a half-loaded
    /// registry would report supported platforms as unsupported.
    pub fn load(directory: &Path) -> Result<Self, PlatformRegistryError> {
        let rows = read_json_documents(&directory.join("rows"))?;
        let targets = read_json_documents(&directory.join("roadmap_targets"))?;
        Self::from_documents(
            rows.iter().map(|(p, b)| (p.as_path(), b.as_str())),
            targets.iter().map(|(p, b)| (p.as_path(), b.as_str())),
        )
    }

    /// Build a registry from already-read documents, reporting the source
    /// path of whichever document fails.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn from_documents<'a, R, T>(rows: R, targets: T) -> Result<Self, PlatformRegistryError>
    where
        R: IntoIterator<Item = (&'a Path, &'a str)>,
        T: IntoIterator<Item = (&'a Path, &'a str)>,
    {
        let mut loaded_rows = BTreeMap::new();
        let mut identities: BTreeMap<RowIdentityKey, String> = BTreeMap::new();
        for (path, body) in rows {
            let row = PlatformSupportRow::from_json(body)
                .map_err(|source| PlatformRegistryError::in_document(path, source))?;
            let key = RowIdentityKey::of(&row);
            if let Some(existing) = identities.get(&key) {
                return Err(PlatformRegistryError::AmbiguousRegistry {
                    detail: format!(
                        "rows `{existing}` and `{}` match the same platform identity",
                        row.row_id()
                    ),
                });
            }
            identities.insert(key, row.row_id().to_string());
            if let Some(existing) = loaded_rows.insert(row.row_id().to_string(), row) {
                return Err(PlatformRegistryError::AmbiguousRegistry {
                    detail: format!("row id `{}` is declared twice", existing.row_id()),
                });
            }
        }

        let mut loaded_targets = BTreeMap::new();
        for (path, body) in targets {
            let target = RoadmapTarget::from_json(body)
                .map_err(|source| PlatformRegistryError::in_document(path, source))?;
            if loaded_rows.contains_key(target.target_id()) {
                return Err(PlatformRegistryError::AmbiguousRegistry {
                    detail: format!(
                        "roadmap target `{}` collides with a support row id",
                        target.target_id()
                    ),
                });
            }
            if loaded_targets
                .insert(target.target_id().to_string(), target)
                .is_some()
            {
                return Err(PlatformRegistryError::AmbiguousRegistry {
                    detail: "a roadmap target id is declared twice".to_string(),
                });
            }
        }

        Ok(Self {
            rows: loaded_rows,
            roadmap_targets: loaded_targets,
        })
    }

    /// Every loaded row, ordered by row id.
    pub fn rows(&self) -> impl Iterator<Item = &PlatformSupportRow> {
        self.rows.values()
    }

    /// Look a row up by its exact id.
    #[must_use]
    pub fn row(&self, row_id: &str) -> Option<&PlatformSupportRow> {
        self.rows.get(row_id)
    }

    /// Rows that count as supported combinations, ordered by row id.
    /// Planned rows are excluded, as are Experimental rows.
    pub fn supported_rows(&self) -> impl Iterator<Item = &PlatformSupportRow> {
        self.rows
            .values()
            .filter(|row| row.is_supported_combination())
    }

    /// Every roadmap target, ordered by target id. These are never
    /// candidates for matching — the query API cannot reach this catalog.
    pub fn roadmap_targets(&self) -> impl Iterator<Item = &RoadmapTarget> {
        self.roadmap_targets.values()
    }

    /// Look a roadmap target up by its exact id.
    #[must_use]
    pub fn roadmap_target(&self, target_id: &str) -> Option<&RoadmapTarget> {
        self.roadmap_targets.get(target_id)
    }

    /// Rows consistent with a host identity alone, ordered by row id.
    ///
    /// Host identity cannot pick a single row — several rows share an OS
    /// and CPU and differ only by accelerator — so this deliberately
    /// returns the candidate set. Use [`Self::resolve`] once accelerator
    /// identity is known.
    #[must_use]
    pub fn candidates(&self, host: &HostIdentity) -> Vec<&PlatformSupportRow> {
        self.rows
            .values()
            .filter(|row| host_matches(row, host))
            .collect()
    }

    /// Resolve a fully detected machine to exactly one row, or to the
    /// typed reason it matches none.
    ///
    /// Checks run in the order that yields the most specific reason: a
    /// partitioned accelerator is rejected before its SKU is considered,
    /// and CPU/OS mismatches are reported before accelerator mismatches,
    /// so an operator is told the first thing that is actually wrong.
    ///
    /// [`PlatformReason::UnsupportedCpuArch`] is reachable here only if a
    /// future release ships rows for a subset of the architectures the
    /// type can express. Today every architecture the type can express has
    /// rows, so an unrecognised architecture fails earlier, in the
    /// host probe that could not name it — which is the correct place: a
    /// machine whose architecture cannot be identified is not a machine
    /// whose architecture is unsupported.
    #[must_use]
    pub fn resolve(&self, detected: &DetectedPlatform) -> RowMatch<'_> {
        if detected
            .accelerator
            .as_ref()
            .is_some_and(|accelerator| accelerator.partitioned)
        {
            return RowMatch::Unsupported(PlatformReason::MigModeEnabled);
        }

        if !self
            .rows
            .values()
            .any(|row| row.cpu().architecture == detected.host.architecture)
        {
            return RowMatch::Unsupported(PlatformReason::UnsupportedCpuArch);
        }
        if !self.rows.values().any(|row| {
            row.cpu().architecture == detected.host.architecture
                && row.cpu().covers_vendor(detected.host.vendor)
        }) {
            return RowMatch::Unsupported(PlatformReason::UnsupportedCpuVendor);
        }

        let host_candidates = self.candidates(&detected.host);
        if host_candidates.is_empty() {
            return RowMatch::Unsupported(PlatformReason::UnsupportedOsVersion);
        }

        let matched = host_candidates
            .into_iter()
            .find(|row| accelerator_matches(row, detected));
        match matched {
            Some(row) if row.support_level() == SupportLevel::Planned => {
                RowMatch::PlannedNotValidated(row)
            }
            Some(row) => RowMatch::Supported(row),
            // The host is recognised but its accelerator configuration is
            // not: either an unknown SKU, or no accelerator where every
            // row for this host declares one.
            None => RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorSku),
        }
    }
}

fn host_matches(row: &PlatformSupportRow, host: &HostIdentity) -> bool {
    row.cpu().architecture == host.architecture
        && row.cpu().covers_vendor(host.vendor)
        && row.os().name == host.os_name
        && row.os().version == host.os_version
        // A row that names an image identity requires it; a row that does
        // not is indifferent to whatever the host reports.
        && row
            .os()
            .image_identity
            .as_ref()
            .map_or(true, |required| {
                host.image_identity.as_ref() == Some(required)
            })
}

fn accelerator_matches(row: &PlatformSupportRow, detected: &DetectedPlatform) -> bool {
    match (row.accelerator(), detected.accelerator.as_ref()) {
        (Some(row_accelerator), Some(observed)) => row_accelerator.sku == observed.sku,
        (None, None) => true,
        _ => false,
    }
}

fn read_json_documents(directory: &Path) -> Result<Vec<(PathBuf, String)>, PlatformRegistryError> {
    let entries =
        std::fs::read_dir(directory).map_err(|source| PlatformRegistryError::Unreadable {
            path: directory.display().to_string(),
            detail: source.to_string(),
        })?;
    let mut documents = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| PlatformRegistryError::Unreadable {
                path: directory.display().to_string(),
                detail: source.to_string(),
            })?
            .path();
        let is_json = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        if !is_json {
            continue;
        }
        let body =
            std::fs::read_to_string(&path).map_err(|source| PlatformRegistryError::Unreadable {
                path: path.display().to_string(),
                detail: source.to_string(),
            })?;
        documents.push((path, body));
    }
    documents.sort();
    Ok(documents)
}
