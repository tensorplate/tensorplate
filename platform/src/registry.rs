// SPDX-License-Identifier: Apache-2.0
//
// The platform registry: the loaded support rows, the separate
// roadmap-target catalog, and the query API that resolves a detected machine
// to an exact row or a lower-priority family compatibility envelope.
//
// Two rules shape everything here. The registry **fails closed on load**:
// one invalid row means no registry at all, because a partially-loaded
// registry would silently answer "unsupported" for rows that were merely
// unreadable. And roadmap targets live in a **separate catalog** that
// matching never consults, so a target can never be mistaken for support.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tensorplate_protocol::install_paths;

use crate::capability::PlatformCapability;
use crate::detect::PlatformReport;
use crate::error::PlatformRegistryError;
use crate::identity::{DetectedPlatform, HostIdentity};
use crate::reason::PlatformReason;
use crate::roadmap::RoadmapTarget;
use crate::row::{
    AcceleratorMatchPolicy, CpuVendor, PlatformSupportRow, SupportLevel, ValidationEnvironmentKind,
};

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
    /// The machine matches an Experimental row. Defined and detectable,
    /// but not a supported combination — and deliberately *not* reported
    /// as Planned: the frozen reason for Planned means a row awaiting
    /// hardware validation, which an Experimental integration is not.
    Experimental(&'a PlatformSupportRow),
    /// The machine matches a row's hardware but is running on a machine
    /// shape that row's evidence does not cover.
    ///
    /// `candidate` names the row when exactly one row's hardware matches,
    /// so a caller can say which claim does not transfer here. It is
    /// `None` when several rows share the hardware and differ only by
    /// machine shape: naming one of them would be picking arbitrarily.
    OutsideValidatedEnvironment {
        candidate: Option<&'a PlatformSupportRow>,
    },
    /// The machine matches no row, with the reason it did not.
    Unsupported(PlatformReason),
}

impl RowMatch<'_> {
    /// The row this machine resolved to, where one was identified.
    ///
    /// For [`Self::OutsideValidatedEnvironment`] this is the row whose
    /// hardware matches but whose claim does not reach this machine — it
    /// is returned so a caller can name that claim, **not** because the
    /// machine matches it. `None` for an unsupported machine, and for an
    /// environment miss where several rows were equally close.
    #[must_use]
    pub fn row(&self) -> Option<&PlatformSupportRow> {
        match self {
            Self::Supported(row) | Self::PlannedNotValidated(row) | Self::Experimental(row) => {
                Some(row)
            }
            Self::OutsideValidatedEnvironment { candidate } => *candidate,
            Self::Unsupported(_) => None,
        }
    }

    /// The typed reason this machine is not a supported combination, where
    /// the frozen vocabulary has a value for it.
    ///
    /// `None` covers both a supported machine and the two outcomes the
    /// vocabulary cannot express — an Experimental row and a machine shape
    /// outside a row's validated environment. Callers must consult the
    /// variant, not just this, before concluding a machine is supported;
    /// [`Self::is_supported`] is the safe predicate.
    #[must_use]
    pub fn reason(&self) -> Option<PlatformReason> {
        match self {
            Self::Supported(_)
            | Self::Experimental(_)
            | Self::OutsideValidatedEnvironment { .. } => None,
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

fn matched_row(row: &PlatformSupportRow) -> RowMatch<'_> {
    match row.support_level() {
        SupportLevel::Planned => RowMatch::PlannedNotValidated(row),
        SupportLevel::Experimental => RowMatch::Experimental(row),
        SupportLevel::Production | SupportLevel::Preview => RowMatch::Supported(row),
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
    // Bit order is this module's own reason priority, broadest dimension
    // first: an operator whose machine differs in several dimensions is
    // told about the architecture before the accelerator, because the
    // broader fact explains more. This is deliberately NOT the order
    // `PlatformReason::ALL` happens to list, which is a listing, not a
    // priority.
    const ARCHITECTURE: u8 = 1 << 0;
    const VENDOR: u8 = 1 << 1;
    const OS: u8 = 1 << 2;
    const ACCELERATOR: u8 = 1 << 3;
    /// Hardware matches but the machine shape is outside the row's
    /// evidence. This bit is never consulted by [`Self::reason`] — an
    /// environment-only miss has no frozen reason and is reported through
    /// [`RowMatch::OutsideValidatedEnvironment`] instead, which
    /// `resolve` checks *before* the nearest-miss fold because naming the
    /// row whose claim does not reach this machine is more actionable
    /// than naming a dimension of some other row.
    const ENVIRONMENT: u8 = 1 << 4;

    fn between(row: &PlatformSupportRow, detected: &DetectedPlatform) -> Self {
        let mut bits = 0;
        if detected.host.architecture.known() != Some(row.cpu().architecture) {
            bits |= Self::ARCHITECTURE;
        }
        if !detected
            .host
            .vendor
            .known()
            .is_some_and(|vendor| row.cpu().covers_vendor(vendor))
        {
            bits |= Self::VENDOR;
        }
        if !os_matches(row, &detected.host) {
            bits |= Self::OS;
        }
        if !accelerator_matches(row, detected) {
            bits |= Self::ACCELERATOR;
        }
        if !environment_matches(row, &detected.host) {
            bits |= Self::ENVIRONMENT;
        }
        Self(bits)
    }

    /// Whether the only thing that differs is the machine shape.
    fn is_environment_only(self) -> bool {
        self.0 == Self::ENVIRONMENT
    }

    /// The same mismatch judged on host identity alone.
    ///
    /// A host profile says nothing about what accelerator is fitted, so
    /// the accelerator dimension must not contribute to a host-level
    /// answer: reporting `unsupported_accelerator_sku` to a machine whose
    /// accelerator has not been looked at yet would be a claim nothing
    /// supports.
    fn host_only(self) -> Self {
        Self(self.0 & !Self::ACCELERATOR)
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

    /// The reason for this mismatch, in this module's broadest-first
    /// dimension priority (see the bit definitions above) — not the order
    /// `PlatformReason::ALL` lists.
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
            // An environment-only mismatch has no frozen reason; the
            // caller reports it through the `RowMatch` variant instead.
            None
        }
    }
}

/// Whether two rows have equal resolution priority and can match one machine.
///
/// Exact and family rows deliberately overlap: exact wins. Two exact rows or
/// two family rows at the same priority must remain disjoint, or resolution
/// would depend on row-id order.
fn rows_are_ambiguous(left: &PlatformSupportRow, right: &PlatformSupportRow) -> bool {
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
    // Environments overlap only where some host machine shape satisfies
    // both. Derived from the same [`AcceptedShapes`] description matching
    // uses, so the two cannot drift: a physical row and a shape-scoped
    // cloud row accept disjoint sets of hosts, and rejecting that pair as
    // ambiguous would block a legitimate environment-separated pair.
    if !accepted_shapes(left).overlaps(accepted_shapes(right)) {
        return false;
    }
    match (left.accelerator(), right.accelerator()) {
        (Some(a), Some(b)) => match (a.match_policy, b.match_policy) {
            (AcceleratorMatchPolicy::Exact, AcceleratorMatchPolicy::Exact) => a.sku == b.sku,
            (AcceleratorMatchPolicy::Family, AcceleratorMatchPolicy::Family) => {
                a.family == b.family
            }
            (AcceleratorMatchPolicy::Exact, AcceleratorMatchPolicy::Family)
            | (AcceleratorMatchPolicy::Family, AcceleratorMatchPolicy::Exact) => false,
        },
        (None, None) => true,
        _ => false,
    }
}

/// The platform profile a detected host selects.
///
/// Host identity narrows to a set, never to one row: rows sharing an OS
/// and CPU profile differ only by accelerator. Callers that need one row
/// supply accelerator identity and use [`PlatformRegistry::resolve`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileSelection<'a> {
    /// Rows consistent with this host, ordered by row id. Never empty.
    Candidates(Vec<&'a PlatformSupportRow>),
    /// The host differs from its nearest rows only in machine shape: its
    /// architecture, vendor, and OS are ones this release validates, but
    /// no row's evidence covers the shape it is running on.
    ///
    /// Separate from [`Self::NoMatch`] because the frozen reason
    /// vocabulary has no value for it, and reusing one would tell an
    /// operator something untrue about their OS or CPU.
    OutsideValidatedEnvironment,
    /// No row is consistent with this host, and why — judged on host
    /// dimensions only.
    NoMatch(PlatformReason),
}

impl ProfileSelection<'_> {
    /// The candidate rows, empty when the host matched none.
    #[must_use]
    pub fn candidates(&self) -> &[&PlatformSupportRow] {
        match self {
            Self::Candidates(rows) => rows,
            Self::NoMatch(_) | Self::OutsideValidatedEnvironment => &[],
        }
    }

    /// The reason this host matched no row, if it matched none.
    #[must_use]
    pub fn no_match_reason(&self) -> Option<PlatformReason> {
        match self {
            Self::Candidates(_) | Self::OutsideValidatedEnvironment => None,
            Self::NoMatch(reason) => Some(*reason),
        }
    }
}

/// The loaded platform registry.
///
/// Obtain one with [`PlatformRegistry::load_installed`] (the installed
/// registry, which is what consumers use), [`PlatformRegistry::load`]
/// (an explicit directory), or [`PlatformRegistry::from_documents`] (from
/// in-memory documents, for tests and for consumers that ship the registry
/// differently). All three fail closed.
#[derive(Clone, Debug)]
pub struct PlatformRegistry {
    rows: BTreeMap<String, PlatformSupportRow>,
    roadmap_targets: BTreeMap<String, RoadmapTarget>,
}

impl PlatformRegistry {
    /// Load the registry from the location the packages install it to.
    ///
    /// This is the entry point every consumer uses. The agent, the CLI,
    /// and the observability service each answer platform questions from
    /// the same rows because they all resolve the registry through here
    /// rather than each naming a path of its own.
    ///
    /// # Errors
    ///
    /// As [`Self::load`] — including [`PlatformRegistryError::Unreadable`]
    /// when the registry is not installed, or is installed but not
    /// readable by this process.
    pub fn load_installed() -> Result<Self, PlatformRegistryError> {
        let directory = install_paths::platform_registry_dir().map_err(|detail| {
            PlatformRegistryError::Unreadable {
                path: install_paths::PLATFORM_REGISTRY_DIR_ENV.to_string(),
                detail,
            }
        })?;
        Self::load(&directory)
    }

    /// Load the registry from a directory containing `rows/` and
    /// `roadmap_targets/` subdirectories of JSON documents.
    ///
    /// Prefer [`Self::load_installed`] in consumers; this takes a path for
    /// tests and for staged trees.
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
                .find(|existing| rows_are_ambiguous(existing, &row))
            {
                return Err(PlatformRegistryError::AmbiguousRegistry {
                    detail: format!(
                        "rows `{}` and `{}` can both match one machine at the same priority",
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
    ///
    /// Machine shape is part of host identity. A host reporting a shape no
    /// row declares can yield no candidates at all, and a host reporting a
    /// cloud shape never yields a row validated on physical hardware —
    /// see [`AcceptedShapes`] for what each row's environment admits.
    #[must_use]
    pub fn candidates(&self, host: &HostIdentity) -> Vec<&PlatformSupportRow> {
        self.rows
            .values()
            .filter(|row| host_matches(row, host))
            .collect()
    }

    /// Select the platform profile for a detected host: the rows it could
    /// be, or the typed reason it could be none of them.
    ///
    /// This is the host-level answer, and it is deliberately a *set*.
    /// Several rows share an OS and CPU profile and differ only by
    /// accelerator, so narrowing to one requires accelerator identity and
    /// is [`Self::resolve`]'s job. Returning a set rather than guessing a
    /// representative is what stops a host-level view from implying a
    /// single-row match that has not been established.
    ///
    /// A host matching nothing gets a typed reason drawn from the nearest
    /// row — the one it fails in the fewest host dimensions — with the
    /// accelerator dimension excluded, since nothing has looked at an
    /// accelerator yet.
    #[must_use]
    pub fn select_profile(&self, host: &HostIdentity) -> ProfileSelection<'_> {
        let candidates = self.candidates(host);
        if !candidates.is_empty() {
            return ProfileSelection::Candidates(candidates);
        }

        let detected = DetectedPlatform::host_only(host.clone());
        let mismatches: Vec<Mismatch> = self
            .rows
            .values()
            .map(|row| Mismatch::between(row, &detected).host_only())
            .filter(|mismatch| mismatch.count() > 0)
            .collect();
        let Some(nearest) = mismatches.iter().map(|m| m.count()).min() else {
            // Unreachable with a loaded registry, which always holds at
            // least one row, but answering with the shape rather than a
            // panic keeps the failure legible if that ever changes.
            return ProfileSelection::NoMatch(PlatformReason::UnsupportedOsVersion);
        };
        // Ties fold together so the answer comes from dimension priority
        // rather than from whichever row iteration reached first.
        let folded = mismatches
            .iter()
            .filter(|m| m.count() == nearest)
            .fold(Mismatch(0), |acc, m| acc.union(*m));
        // An environment-only miss has no frozen reason, and borrowing one
        // would be a false statement: a host one machine shape away from a
        // row has a perfectly supported OS, and telling its operator the OS
        // version is unsupported sends them to reinstall the wrong thing.
        // It gets its own outcome, mirroring `RowMatch`.
        folded.reason().map_or(
            ProfileSelection::OutsideValidatedEnvironment,
            ProfileSelection::NoMatch,
        )
    }

    /// Resolve a fully detected machine to exactly one row, or to the
    /// typed reason it matches none.
    ///
    /// A partitioned accelerator is rejected outright, before any row is
    /// considered: a partitioned instance of a supported SKU is not a
    /// degraded version of that row, it is a configuration this release
    /// does not serve on.
    ///
    /// A host reporting more than one accelerator is rejected the same
    /// way and for the same reason. Every committed row is single-device,
    /// so two of a supported card is a topology no row's evidence was
    /// collected on -- not a better version of the row that claims one.
    /// Partitioning is checked first: on a host that is both partitioned
    /// and multi-device, MIG is the more specific and more actionable
    /// answer.
    ///
    /// A machine whose hardware matches a row but whose machine shape is
    /// outside that row's validated environment resolves to
    /// [`RowMatch::OutsideValidatedEnvironment`], never to `Supported`:
    /// evidence recorded on one machine shape does not transfer to
    /// another, so an accelerator in an unvalidated chassis must not
    /// inherit a cloud row's claim.
    ///
    /// Otherwise the answer comes from the *nearest* row — the one the
    /// machine fails in the fewest dimensions — so a machine one CPU
    /// vendor away from a row is told about the vendor rather than about
    /// its accelerator. Ties resolve in this module's own dimension
    /// priority — architecture, then vendor, then OS, then accelerator:
    /// broadest first, because the broader fact explains more — so the
    /// answer never depends on registry file order. That order is
    /// deliberately not the one `PlatformReason::ALL` happens to list,
    /// which is a listing rather than a priority.
    ///
    /// The environment dimension sits outside that ranking: a row whose
    /// hardware matches and whose *only* difference is machine shape is
    /// reported before any nearest-miss reason, because naming the row
    /// whose claim does not reach this machine says more than naming a
    /// dimension of some unrelated row.
    ///
    /// Trigger *semantics* for the reasons — when `doctor` shows which,
    /// and how — are frozen elsewhere; this is the registry's own
    /// mechanical answer. Two limits of the frozen vocabulary show up
    /// here: it has no value for "no accelerator where the row requires
    /// one", and none for "the OS name is unknown", so both report the
    /// nearest available truth
    /// ([`PlatformReason::UnsupportedAcceleratorSku`] and
    /// [`PlatformReason::UnsupportedOsVersion`] respectively). A third
    /// such gap was closed rather than absorbed:
    /// [`PlatformReason::UnsupportedAcceleratorTopology`] exists because
    /// reporting a device count as a wrong SKU would have been the same
    /// kind of nearest-available-truth, on a fact an operator can act on.
    #[must_use]
    pub fn resolve(&self, detected: &DetectedPlatform) -> RowMatch<'_> {
        if detected
            .accelerator
            .as_ref()
            .is_some_and(|accelerator| accelerator.partitioned)
        {
            return RowMatch::Unsupported(PlatformReason::MigModeEnabled);
        }

        if detected
            .accelerator
            .as_ref()
            .is_some_and(|accelerator| accelerator.device_count != 1)
        {
            return RowMatch::Unsupported(PlatformReason::UnsupportedAcceleratorTopology);
        }

        let mut nearest_count = u32::MAX;
        let mut nearest = Mismatch::default();
        let mut exact_outside_environment: Vec<&PlatformSupportRow> = Vec::new();
        let mut family_outside_environment: Vec<&PlatformSupportRow> = Vec::new();
        let mut family_match: Option<&PlatformSupportRow> = None;
        for row in self.rows.values() {
            let mismatch = Mismatch::between(row, detected);
            if mismatch.is_environment_only() {
                if row.accelerator().is_some_and(|accelerator| {
                    accelerator.match_policy == AcceleratorMatchPolicy::Family
                }) {
                    family_outside_environment.push(row);
                } else {
                    exact_outside_environment.push(row);
                }
            }
            if mismatch.count() == 0 {
                if row.accelerator().is_some_and(|accelerator| {
                    accelerator.match_policy == AcceleratorMatchPolicy::Family
                }) {
                    family_match = Some(row);
                    continue;
                }
                return matched_row(row);
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

        if let Some(row) = family_match {
            return matched_row(row);
        }

        // Hardware matched a row but the machine shape is outside its
        // evidence: report the row rather than an unrelated reason, so a
        // caller can say precisely which claim does not transfer here.
        let outside_environment = if exact_outside_environment.is_empty() {
            family_outside_environment
        } else {
            exact_outside_environment
        };
        if !outside_environment.is_empty() {
            return RowMatch::OutsideValidatedEnvironment {
                // Exactly one row's hardware matches: name it. Several,
                // and any choice would be arbitrary.
                candidate: match outside_environment.as_slice() {
                    [only] => Some(only),
                    _ => None,
                },
            };
        }
        // `reason()` is `None` only for a mismatch of nothing or of
        // environment alone, and both returned above — so this fallback is
        // unreachable rather than a live fail-open path. It exists because
        // the type system cannot say that.
        debug_assert!(
            nearest.reason().is_some(),
            "every surviving mismatch has a reason"
        );
        RowMatch::Unsupported(
            nearest
                .reason()
                .unwrap_or(PlatformReason::UnsupportedAcceleratorSku),
        )
    }

    /// Resolve a platform observation into its row-bounded memory capability.
    ///
    /// A capability exists only for a supported row with a matching
    /// accelerator observation. Planned, experimental, outside-environment,
    /// and unsupported outcomes never publish an admission limit. A family
    /// row applies the same conservative detected-and-row-bounded ceiling as
    /// an exact row.
    #[must_use]
    pub fn resolved_capability(&self, report: &PlatformReport) -> Option<PlatformCapability> {
        let detected = report.detected_platform();
        let RowMatch::Supported(row) = self.resolve(&detected) else {
            return None;
        };
        let observed = report.accelerator.as_ref()?;
        let declared = row.accelerator()?;
        debug_assert!(accelerator_matches(row, &detected));
        debug_assert_eq!(observed.memory_profile, declared.memory_profile);
        Some(PlatformCapability::bounded(
            row.row_id(),
            declared.memory_profile,
            observed.memory_bytes,
            declared.memory_bytes,
        ))
    }

    /// The memory ceiling for a machine admitted against `row` without
    /// matching that row's validated environment.
    ///
    /// The same conservative bound a validated match gets: the smaller of
    /// what this machine reports and what the row budgets. The row's
    /// *environment* evidence does not transfer to an uncharacterised
    /// chassis, which is why such a machine is never `Supported` — but the
    /// memory budget is a property of the accelerator, and that does.
    ///
    /// Publishing no ceiling here would be the unsafe reading of "not
    /// validated": it would leave the machine admitted with the configured
    /// limit alone, so an unvalidated host would be bounded *less* than a
    /// validated one.
    ///
    /// No assertion that the observed and declared memory profiles agree.
    /// [`Self::resolved_capability`] can make that claim because it has an
    /// exact match in hand; the candidate here may have matched by family,
    /// where they legitimately differ.
    #[must_use]
    pub fn capability_outside_environment(
        &self,
        report: &PlatformReport,
        row: &PlatformSupportRow,
    ) -> Option<PlatformCapability> {
        let observed = report.accelerator.as_ref()?;
        let declared = row.accelerator()?;
        Some(PlatformCapability::bounded(
            row.row_id(),
            declared.memory_profile,
            observed.memory_bytes,
            declared.memory_bytes,
        ))
    }
}

fn host_matches(row: &PlatformSupportRow, host: &HostIdentity) -> bool {
    host.architecture.known() == Some(row.cpu().architecture)
        && host
            .vendor
            .known()
            .is_some_and(|vendor| row.cpu().covers_vendor(vendor))
        && os_matches(row, host)
        && environment_matches(row, host)
}

/// Which host machine shapes a row's validated environment accepts.
///
/// The single description both the matcher and the load-time overlap check
/// are derived from. Keeping one source is the point: these two are
/// documented as exact duals — the overlap check is the negation of
/// "resolution is unambiguous" — and a change to one that misses the other
/// either rejects legitimate rows at load or admits a genuinely ambiguous
/// pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcceptedShapes<'a> {
    /// Exactly this machine shape, and nothing else.
    Only(&'a str),
    /// Only a host that reports no machine shape. A row validated on
    /// physical hardware makes no claim about a cloud instance: its
    /// evidence was recorded in a chassis whose thermals, firmware, and
    /// power delivery are the operator's, none of which transfer to a
    /// hypervisor.
    Unshaped,
    /// Any host, shaped or not. How the schema expresses a deliberately
    /// chassis-independent claim: `cloud_instance` with no machine type.
    Any,
}

impl AcceptedShapes<'_> {
    /// Whether some host satisfies both.
    fn overlaps(self, other: Self) -> bool {
        match (self, other) {
            // A shape-scoped row and an unshaped-only row share no host:
            // one requires a machine shape, the other requires none.
            (Self::Only(_), Self::Unshaped) | (Self::Unshaped, Self::Only(_)) => false,
            (Self::Only(a), Self::Only(b)) => a == b,
            _ => true,
        }
    }

    fn admits(self, machine_type: Option<&str>) -> bool {
        match self {
            Self::Any => true,
            Self::Only(required) => machine_type == Some(required),
            Self::Unshaped => machine_type.is_none(),
        }
    }
}

fn accepted_shapes(row: &PlatformSupportRow) -> AcceptedShapes<'_> {
    let environment = row.validation_environment();
    match (
        environment.machine_type.as_deref(),
        environment.kind == ValidationEnvironmentKind::Physical,
    ) {
        (Some(required), _) => AcceptedShapes::Only(required),
        (None, true) => AcceptedShapes::Unshaped,
        (None, false) => AcceptedShapes::Any,
    }
}

fn environment_matches(row: &PlatformSupportRow, host: &HostIdentity) -> bool {
    accepted_shapes(row).admits(host.machine_type.as_deref())
}

fn os_matches(row: &PlatformSupportRow, host: &HostIdentity) -> bool {
    row.os().name == host.os_name
        && row.os().version == host.os_version
        // A row that names an image identity requires it; a row that does
        // not is indifferent to whatever the host reports, which is why
        // `rows_are_ambiguous` treats an absent one as overlapping.
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
        (Some(row_accelerator), Some(observed)) => match row_accelerator.match_policy {
            AcceleratorMatchPolicy::Exact => row_accelerator.sku == observed.sku,
            AcceleratorMatchPolicy::Family => {
                row_accelerator.family == "Apple M-series"
                    && detected.host.vendor.known() == Some(CpuVendor::Apple)
                    && is_apple_m_series_sku(&observed.sku)
            }
        },
        (None, None) => true,
        _ => false,
    }
}

/// Whether the exact brand string is one of Apple's M-series spellings.
///
/// Detection deliberately preserves the system-reported SKU verbatim. Family
/// matching therefore recognizes only the documented base, Pro, Max, and
/// Ultra forms and rejects near-miss prose rather than normalizing it into
/// support.
fn is_apple_m_series_sku(sku: &str) -> bool {
    let Some(rest) = sku.strip_prefix("Apple M") else {
        return false;
    };
    let generation_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if generation_len == 0 || rest.starts_with('0') {
        return false;
    }
    matches!(&rest[generation_len..], "" | " Pro" | " Max" | " Ultra")
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
