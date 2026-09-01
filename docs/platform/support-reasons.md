# Platform reason vocabulary

Ten typed values say why a machine is not a supported combination. They
are **frozen for v0.2.1**: the set, the wire spellings, and the trigger
conditions below do not change within the release line. Rendering may be
reworded; the spelling a caller matches on may not.

The point of a fixed vocabulary is that one condition reads the same way
in every surface. `doctor`, deploy admission, and the durable error record
emit the same value for the same cause, so an operator who reads one and
an engineer who greps another are looking at the same fact.

## The values

| Spelling | Triggered when |
| --- | --- |
| `unsupported_accelerator_sku` | The accelerator matches neither an exact row nor an explicit family row. Vendor-neutral: it carries an Apple chip and an NVIDIA card alike. A near miss is unsupported, never degraded. |
| `unsupported_os_version` | The OS version is below a row's floor, or is not the exact version a row names. |
| `unsupported_cpu_arch` | The CPU architecture is not one this release builds for. |
| `unsupported_cpu_vendor` | The architecture is built for, but no row covers this vendor. Distinct from the arch reason: the two send an operator to different answers. |
| `mig_mode_enabled` | The accelerator is partitioned. Checked before SKU, so a partitioned supported card refuses for partitioning rather than for identity. |
| `missing_backend_package` | A package the matched row requires is not installed — including a backend whose descriptor is absent. |
| `missing_driver_runtime` | A required driver or compute runtime is absent or version-mismatched, **or** the PCI bus reports an accelerator that no driver could identify. |
| `accelerator_runtime_unavailable` | The runtime is installed and not usable: a malformed descriptor, an absent or wrong-version interpreter, a module or framework that will not import, or an accelerator runtime (MPS today) that reports itself unavailable. Never a missing package. |
| `telemetry_degraded` | A telemetry collector expected on the matched row fails at run time. |
| `row_planned_not_validated` | The machine matches a Planned row exactly: named, carrying no validation evidence. |

## Boundaries that are easy to blur

**A missing package is not a dead runtime.** `missing_backend_package`
means install something; `accelerator_runtime_unavailable` means the thing
is installed and cannot run. Collapsing them tells an operator whose
PyTorch cannot reach its accelerator to reinstall a package they already
have. The classification is one function so both sides cannot drift.

**A driverless accelerator is not an absent one.** A GPU host whose driver
is broken reports no accelerator and would otherwise resolve to a CPU-only
row — a supported answer, for a machine that will not serve. The PCI bus
distinguishes the two without a driver, which is why it is consulted
before resolution.

**A machine-shape miss has no reason at all.** The vocabulary has no value
for "this hardware is validated, this chassis is not", and every nearby
value names a dimension that is fine. It is reported without one rather
than borrowing the nearest.

## Crossing the process boundary

The Python sidecar emits reason strings of its own; today an unavailable
MPS runtime is the only one. Those strings are this vocabulary's
spellings, checked at the IPC boundary and asserted against the Python
source in `platform/tests/reason_contract.rs` — a rename on either side
that the other does not follow would otherwise surface as an unrecognized
string on a machine rather than as a failing test.
