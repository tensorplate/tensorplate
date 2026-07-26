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

/// Which dimensions of a row a detected machine fails to satisfy.
///
/// Reason selection works from this rather than from a sequence of
/// filters: a filter chain reports whichever dimension it happened to
/// narrow on last, which is how a machine one CPU-vendor away from a row
/// ends up being told its accelerator is unsupported.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Mismatch(u8);

impl Mismatch {
    // Bit order is the dimension priority: the lowest set bit names the
    // reason, so ties resolve the same way every time.
    const ARCHITECTURE: u8 = 1 << 0;
    const VENDOR: u8 = 1 << 1;
    const OS: u8 = 1 << 2;
    const ACCELERATOR: u8 = 1 << 3;

    fn between(row: &PlatformSupportRow, detected: &DetectedPlatform) -> Self {
        let mut bits = 0;
        if row.cpu().architecture != detected.host.architecture {
            bits |= Self::ARCHITECTURE;
        }
        if !row.cpu().covers_vendor(detected.host.vendor) {
            bits |= Self::VENDOR;
        }
        if !os_matches(row, &detected.host) {
            bits |= Self::OS;
        }
        if !accelerator_matches(row, detected) {
            bits |= Self::ACCELERATOR;
        }
        Self(bits)
    }

    /// Combine the dimensions of two equally-near rows, so a tie is
    /// resolved by dimension priority rather than by whichever row the
    /// iteration reached first.
    fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// The reason for this mismatch, in the dimension order the frozen
    /// reason vocabulary declares.
    fn reason(self) -> Option<PlatformReason> {
        if self.0 & Self::ARCHITECTURE != 0 {
            Some(PlatformReason::UnsupportedCpuArch)
        } else if self.0 & Self::VENDOR != 0 {
            Some(PlatformReason::UnsupportedCpuVendor)
        } else if self.0 & Self::OS != 0 {
            Some(PlatformReason::UnsupportedOsVersion)
        } else if self.0 & Self::ACCELERATOR != 0 {
            Some(PlatformReason::UnsupportedAcceleratorSku)
        } else {
            None
        }
    }
}

/// Whether some machine could match both rows at once.
///
/// This is the exact negation of "resolution is unambiguous", so it is
/// derived from the same predicates matching uses rather than from a
/// separate key: an identity key that omits a dimension (vendors) rejects
/// rows that are genuinely distinguishable, and one that compares a
/// wildcard field verbatim (an absent `image_identity` matches any) misses
/// pairs that really do collide.
fn rows_can_both_match(left: &PlatformSupportRow, right: &PlatformSupportRow) -> bool {
    if left.cpu().architecture != right.cpu().architecture {
        return false;
    }
    if !left
        .cpu()
        .vendors
        .iter()
        .any(|vendor| right.cpu().covers_vendor(*vendor))
    {
        return false;
    }
    if left.os().name != right.os().name || left.os().version != right.os().version {
        return false;
    }
    // An absent image identity matches any, so it overlaps everything;
    // two present ones overlap only when equal.
    match (&left.os().image_identity, &right.os().image_identity) {
        (Some(a), Some(b)) if a != b => return false,
        _ => {}
    }
    match (left.accelerator(), right.accelerator()) {
        (Some(a), Some(b)) => a.sku == b.sku,
        (None, None) => true,
        _ => false,
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
        let mut loaded_rows: BTreeMap<String, PlatformSupportRow> = BTreeMap::new();
        for (path, body) in rows {
            let row = PlatformSupportRow::from_json(body)
                .map_err(|source| PlatformRegistryError::in_document(path, source))?;
            if let Some(overlapping) = loaded_rows
                .values()
                .find(|existing| rows_can_both_match(existing, &row))
            {
                return Err(PlatformRegistryError::AmbiguousRegistry {
                    detail: format!(
                        "rows `{}` and `{}` can both match one machine",
                        overlapping.row_id(),
                        row.row_id()
                    ),
                });
            }
            if loaded_rows.contains_key(row.row_id()) {
                return Err(PlatformRegistryError::AmbiguousRegistry {
                    detail: format!("row id `{}` is declared twice", row.row_id()),
                });
            }
            loaded_rows.insert(row.row_id().to_string(), row);
        }
        if loaded_rows.is_empty() {
            // An empty registry answers "unsupported" for every machine on
            // earth, which is indistinguishable from a registry that failed
            // to load. Refuse it.
            return Err(PlatformRegistryError::AmbiguousRegistry {
                detail: "the registry declares no platform support rows".to_string(),
            });
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
    /// A partitioned accelerator is rejected outright, before any row is
    /// considered: a partitioned instance of a supported SKU is not a
    /// degraded version of that row, it is a configuration this release
    /// does not serve on.
    ///
    /// Otherwise the answer comes from the *nearest* row — the one the
    /// machine fails in the fewest dimensions — so a machine one CPU
    /// vendor away from a row is told about the vendor rather than about
    /// its accelerator. Ties resolve in the fixed dimension order the
    /// frozen reason vocabulary declares, so the answer never depends on
    /// registry file order.
    ///
    /// Trigger *semantics* for the reasons — when `doctor` shows which,
    /// and how — are frozen elsewhere; this is the registry's own
    /// mechanical answer. Two limits of the frozen vocabulary show up
    /// here: it has no value for "no accelerator where the row requires
    /// one", and none for "the OS name is unknown", so both report the
    /// nearest available truth
    /// ([`PlatformReason::UnsupportedAcceleratorSku`] and
    /// [`PlatformReason::UnsupportedOsVersion`] respectively).
    #[must_use]
    pub fn resolve(&self, detected: &DetectedPlatform) -> RowMatch<'_> {
        if detected
            .accelerator
            .as_ref()
            .is_some_and(|accelerator| accelerator.partitioned)
        {
            return RowMatch::Unsupported(PlatformReason::MigModeEnabled);
        }

        let mut nearest_count = u32::MAX;
        let mut nearest = Mismatch::default();
        for row in self.rows.values() {
            let mismatch = Mismatch::between(row, detected);
            if mismatch.count() == 0 {
                return match row.support_level() {
                    SupportLevel::Planned => RowMatch::PlannedNotValidated(row),
                    // Experimental rows are defined but are not supported
                    // combinations, exactly as `is_supported_combination`
                    // reports them; deployment must not proceed on one.
                    SupportLevel::Experimental => {
                        RowMatch::Unsupported(PlatformReason::RowPlannedNotValidated)
                    }
                    SupportLevel::Production | SupportLevel::Preview => RowMatch::Supported(row),
                };
            }
            match mismatch.count().cmp(&nearest_count) {
                std::cmp::Ordering::Less => {
                    nearest_count = mismatch.count();
                    nearest = mismatch;
                }
                // Equally near: fold the dimensions together so the
                // priority order below decides, not iteration order.
                std::cmp::Ordering::Equal => nearest = nearest.union(mismatch),
                std::cmp::Ordering::Greater => {}
            }
        }

        RowMatch::Unsupported(
            nearest
                .reason()
                .unwrap_or(PlatformReason::UnsupportedAcceleratorSku),
        )
    }
}

fn host_matches(row: &PlatformSupportRow, host: &HostIdentity) -> bool {
    row.cpu().architecture == host.architecture
        && row.cpu().covers_vendor(host.vendor)
        && os_matches(row, host)
}

fn os_matches(row: &PlatformSupportRow, host: &HostIdentity) -> bool {
    row.os().name == host.os_name
        && row.os().version == host.os_version
        // A row that names an image identity requires it; a row that does
        // not is indifferent to whatever the host reports, which is why
        // `rows_can_both_match` treats an absent one as overlapping.
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
        // Anything that is not a JSON document is an error, not
        // something to skip: renaming a row to `.json.bak` or dropping it
        // in a subdirectory would otherwise leave a smaller registry that
        // loads cleanly and reports real platforms as unsupported.
        let is_json = path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        if !is_json {
            return Err(PlatformRegistryError::Unreadable {
                path: path.display().to_string(),
                detail: "registry directories contain only JSON documents".to_string(),
            });
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
