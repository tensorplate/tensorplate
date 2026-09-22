# macOS offline-runtime fixtures

Inputs for `test/packaging/verify_macos_offline_runtime.sh`, which tests
`tools/validation/macos_offline_runtime.py` and the offline stages of
`tools/validation/macos-homebrew-lifecycle.sh`.

| File | Kind | What it is |
| --- | --- | --- |
| `probe-final.json` | Recorded | The helper's unsandboxed control and its sandboxed probe under the rendered profile, on two ephemeral loopback ports. Every refusal is `EPERM`, and no control operation is refused. |
| `probe-deny-network-only.json` | Recorded | The same under `(deny network*)` alone, the profile the stage used before: loopback, the agent socket and the health endpoint are refused too. |
| `probe-draft-localhost-any-port.json` | Recorded | Loopback allowed on every port: `fe80::1`, loopback port 9 and binds on unlisted ports are not refused. |
| `probe-allow-all-local-ip.json` | Recorded | `(allow network* (local ip "localhost:*"))`: every remote send gets past the sandbox and fails only because the socket is pinned to `lo0` (`ENETUNREACH`, `EHOSTUNREACH`). |
| `probe-resolver-deny-first.json` | Recorded | The mDNSResponder deny placed before the unix-socket allows, which override it. |
| `probe-resolver-unresolved-path.json` | Recorded | The mDNSResponder deny naming `/var/run/...`, which silently matches nothing. |
| `probe-no-base-deny.json` | Recorded | An outbound-only deny with no `(deny network*)`: every send is refused, but binds and mDNSResponder are not. `sandbox_check` also does not read the network as denied, which is why the stage reads it back as well as probing. |
| `probe-no-inbound-allow.json` | Recorded | No inbound rule: the loopback listen on the candidate port is refused. |
| `probe-tcp-loopback-any-port.json` | Recorded | The rendered profile plus `(allow network-outbound (remote tcp "localhost:*"))`: only the TCP connect to loopback port 9 gets through. |
| `probe-tcp-any-host-serving-ports.json` | Recorded | Plus TCP to any host on the two serving ports: only the TCP sends from documentation addresses to those ports get through. |
| `probe-udp-any-host-serving-ports.json` | Recorded | The same for UDP. |
| `probe-tcp-listen-any-address.json` | Recorded | Plus `(allow network-inbound (local tcp "*:*"))`: only the TCP listens on unlisted ports succeed. |
| `probe-udp-bind-any-address.json` | Recorded | Plus `(allow network-inbound (local udp "*:*"))`: only the UDP bind on an unlisted port succeeds. |
| `launchctl-print-sandboxed-agent.txt` | Synthetic | The shape of `launchctl print` for the agent job run from the derived plist, under Homebrew 7's `sh.brew.tensorplate-agent` label, with nested blocks that carry their own `pid` and `state` lines. |
| `fake_host.py` | Test support | The fake Homebrew, launchd, process table and CLI the stage tests run against. It names jobs, keg plists and LaunchAgents plists as Homebrew 7 does, `sh.brew.<formula>`, or with the `legacy-labels` mode as an older Homebrew did, `homebrew.mxcl.<formula>`. |

The probe recordings were made on an Apple M1 Pro development Mac under
macOS 26.6 with the helper's own `control` and `probe` code, set up the
way the `offline-profile` preflight runs them. They hold only errno names by
destination role and the synthetic preflight deployment id, so there is
nothing host-specific to sanitize. Every probe socket for a non-loopback
destination is pinned to `lo0`, so recording them sent nothing off the
host. On macOS the verifier repeats the same runs live and requires the
same classification.

`launchctl-print-sandboxed-agent.txt` is transcribed, not recorded: the
first hardware run of the offline stage keeps the raw `launchctl print`
output of both sandboxed jobs in its local `offline-runtime.log`. Replace
this file with a sanitized copy of that output, and correct the parser if
the recording disagrees with it.
