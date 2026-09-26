// SPDX-License-Identifier: Apache-2.0
//
// The Compute Engine instance a machine-type record was taken on.
//
// The machine-type record (see [`crate::machine_type_record`]) says what the
// metadata service answered in this kernel boot. It cannot also say which
// instance answered: after a rollback the 0.2.1 agent reads that record,
// accepts only its schema 2 and rejects unknown fields, so the record never
// grows. The instance id lives here instead, in a companion record outside
// the state directory a rollback sets aside. The 0.2.1 agent never reads it.
//
// Two checks rest on it:
//
// - With the metadata service answering, a binding that names another
//   instance fails detection: the disk moved to, or was cloned into, another
//   instance, and this host is not the one its identity was recorded on. So
//   does a binding that names this instance on another machine type than
//   the live answer, whatever boot it is from: the instance was stopped and
//   given a different machine type. Either way reprovisioning is explicit:
//   delete both records and start the agent while the service is
//   reachable. A binding this release cannot parse has no instance or
//   machine type to compare, so it is treated as absent and replaced on
//   that start.
// - With the service unreachable, a binding written in this boot must agree
//   with the machine-type record detection is about to use -- the same
//   machine type, and a digest of exactly those bytes -- or detection fails.
//   A binding from an earlier boot must still name the record's machine
//   type, or the instance was given another one and detection fails as it
//   would online. Without a binding the record alone decides, as it did
//   before the binding existed: an agent upgraded from 0.2.1 under denied
//   egress runs that way until its first start that reaches the service.
//
// The instance id is never logged and never quoted in an error. Evidence
// captured from the agent's journal and from doctor's output is published.
//
// Everything in this module is pure. Reading and writing the file lives in
// [`crate::probe`].

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tensorplate_protocol::install_paths::INSTANCE_BINDING_PATH;
use tensorplate_protocol::serde_shape::is_canonical_identifier;

use crate::detect::{instance_id_from_metadata, HostSources};
use crate::error::{PlatformProbeError, BROKEN_METADATA_ANSWER, GCE_METADATA_SOURCE_NAME};
use crate::machine_type_record::{is_boot_id, MachineTypeRecord};

/// The only binding layout this release reads or writes. A rollback
/// reinstates this reader, so a later release that changes the layout keeps
/// this one readable where it is.
pub const INSTANCE_BINDING_SCHEMA_VERSION: u32 = 1;

/// The binding file's content.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceBinding {
    pub schema_version: u32,
    /// The instance id the metadata service answered, in decimal.
    pub instance_id: String,
    /// The bare machine type the same start recorded.
    pub machine_type: String,
    /// The kernel boot in which both answers were obtained.
    pub boot_id: String,
    /// Lowercase hex SHA-256 of the machine-type record bytes the same start
    /// wrote, or found already written.
    pub machine_type_record_sha256: String,
}

impl InstanceBinding {
    /// The binding for `record`, whose exact file bytes are `record_body`,
    /// taken on the instance a live metadata answer named `instance_id`.
    #[must_use]
    pub fn new(instance_id: String, record: &MachineTypeRecord, record_body: &str) -> Self {
        Self {
            schema_version: INSTANCE_BINDING_SCHEMA_VERSION,
            instance_id,
            machine_type: record.machine_type.clone(),
            boot_id: record.boot_id.clone(),
            machine_type_record_sha256: sha256_hex(record_body.as_bytes()),
        }
    }

    /// Parse and validate a binding file's content.
    ///
    /// # Errors
    ///
    /// Says why the content is not a binding this release can use: not
    /// JSON, an unknown or missing field, another schema version, or a field
    /// that is not in its canonical form. The reason never quotes the file,
    /// which carries the instance id.
    pub fn parse(body: &str) -> Result<Self, String> {
        let binding: Self = serde_json::from_str(body).map_err(|err| {
            let kind = match err.classify() {
                serde_json::error::Category::Io => "I/O",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "data",
                serde_json::error::Category::Eof => "truncated",
            };
            format!(
                "not a valid binding ({kind} error at line {}, column {})",
                err.line(),
                err.column()
            )
        })?;
        if binding.schema_version != INSTANCE_BINDING_SCHEMA_VERSION {
            return Err(format!(
                "schema_version {} is not {INSTANCE_BINDING_SCHEMA_VERSION}",
                binding.schema_version
            ));
        }
        if instance_id_from_metadata(&binding.instance_id).as_deref()
            != Some(binding.instance_id.as_str())
        {
            return Err("the instance id is not a canonical decimal id".to_string());
        }
        if !is_canonical_identifier(&binding.machine_type) {
            return Err("the machine type is not a canonical identifier".to_string());
        }
        if !is_boot_id(&binding.boot_id) {
            return Err("the boot ID is not a canonical UUID".to_string());
        }
        if !is_sha256_hex(&binding.machine_type_record_sha256) {
            return Err("the record digest is not a lowercase hex SHA-256".to_string());
        }
        Ok(binding)
    }

    /// The exact bytes the binding file holds.
    ///
    /// # Errors
    ///
    /// Only if serialization itself fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self).map(|json| json + "\n")
    }

    /// How this binding disagrees with `record`, whose file bytes are
    /// `record_body`, or `None` when it does not. Only asked of a binding
    /// written in the boot `record` is bound to.
    #[must_use]
    pub fn disagreement(&self, record: &MachineTypeRecord, record_body: &str) -> Option<String> {
        if self.machine_type != record.machine_type {
            return Some(format!(
                "the instance binding written in this boot names machine type `{}` and the \
                 machine-type record names `{}`",
                self.machine_type, record.machine_type
            ));
        }
        if self.machine_type_record_sha256 != sha256_hex(record_body.as_bytes()) {
            return Some(
                "the machine-type record is not the one the instance binding written in this \
                 boot was taken with"
                    .to_string(),
            );
        }
        None
    }
}

/// Refuse a live answer from another instance, or for this instance on
/// another machine type than `live_machine_type`, the bare machine type the
/// live answer names.
///
/// Called only with a live machine-type answer in hand. Passes when there is
/// no instance-id answer to compare -- the probe never produces one without
/// the other, so that is a fixture without the source -- when there is no
/// binding, and when the binding cannot be parsed: that start replaces it.
/// The binding's boot does not matter: a machine type changes only across a
/// stop and a start, so a binding from before the change is always from an
/// earlier boot.
///
/// # Errors
///
/// [`PlatformProbeError::Unrecognized`] for an instance-id answer that is not
/// a decimal id, [`PlatformProbeError::InstanceChanged`] when the binding
/// names a different instance, and
/// [`PlatformProbeError::MachineTypeChanged`] when it names this instance
/// on another machine type. None quotes an id.
pub fn check_live_instance(
    sources: &HostSources,
    live_machine_type: &str,
) -> Result<(), PlatformProbeError> {
    let Some(answer) = sources.gce_instance_id.as_deref() else {
        return Ok(());
    };
    let live =
        instance_id_from_metadata(answer).ok_or_else(|| PlatformProbeError::Unrecognized {
            source_name: GCE_METADATA_SOURCE_NAME.to_string(),
            detail: format!(
                "the instance-id answer is not a decimal instance id; {BROKEN_METADATA_ANSWER}"
            ),
        })?;
    let Some(binding) = sources
        .instance_binding
        .as_deref()
        .and_then(|body| InstanceBinding::parse(body).ok())
    else {
        return Ok(());
    };
    if binding.instance_id != live {
        return Err(PlatformProbeError::InstanceChanged {
            source_name: INSTANCE_BINDING_PATH.to_string(),
            detail: format!(
                "the metadata service answers for a different Compute Engine instance than the \
                 one this host's identity was recorded on; the disk was moved or cloned; \
                 {REPROVISION}"
            ),
        });
    }
    if binding.machine_type != live_machine_type {
        return Err(machine_type_changed(&format!(
            "the metadata service answers machine type `{live_machine_type}` for the instance \
             this host's identity was recorded on as `{}`; the instance was given a different \
             machine type",
            binding.machine_type
        )));
    }
    Ok(())
}

/// The explicit reprovisioning both identity refusals end with: the
/// journal line that reports them is all an operator may read.
pub(crate) const REPROVISION: &str = "to reprovision the host, stop tensorplate-agent, delete \
    /var/lib/tensorplate/identity/instance-binding.json and \
    /var/lib/tensorplate/state/machine-type.json, and start it while the metadata service is \
    reachable";

/// The refusal for a binding on another machine type, with `what` saying
/// which two machine types disagree and why, followed by the reprovisioning
/// steps.
pub(crate) fn machine_type_changed(what: &str) -> PlatformProbeError {
    PlatformProbeError::MachineTypeChanged {
        source_name: INSTANCE_BINDING_PATH.to_string(),
        detail: format!("{what}; {REPROVISION}"),
    }
}

/// Lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_sha256_hex(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
