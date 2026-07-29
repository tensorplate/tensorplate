#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
cd "$repo_root"

templates="packaging/homebrew/Formula"
configs="packaging/homebrew/conf"
renderer="tools/release/render-homebrew-formulas.sh"
publisher="tools/release/publish-homebrew-formula.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

expected=(
  tensorplate-agent.rb
  tensorplate-serving.rb
  tensorplate-cli.rb
  tensorplate-observability.rb
  tensorplate-backend-python-pytorch.rb
  tensorplate.rb
)
placeholder_url="https://github.com/tensorplate/tensorplate/archive/refs/tags/v0.0.0.tar.gz"
placeholder_sha="0000000000000000000000000000000000000000000000000000000000000000"
release_url="https://github.com/tensorplate/tensorplate/archive/refs/tags/v0.2.1.tar.gz"
release_sha="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

bash -n "$renderer"
"$renderer" --help >/dev/null

actual="$(find "$templates" -maxdepth 1 -type f -name '*.rb' -exec basename {} \; | sort)"
wanted="$(printf '%s\n' "${expected[@]}" | sort)"
[[ "$actual" == "$wanted" ]] || {
  printf 'FAIL: formula template set differs from the managed graph\n' >&2
  exit 1
}

for name in "${expected[@]}"; do
  formula="${templates}/${name}"
  grep -qF "  url \"${placeholder_url}\"" "$formula"
  grep -qF "  sha256 \"${placeholder_sha}\"" "$formula"
  grep -qF "  depends_on arch: :arm64" "$formula"
  grep -qF "  depends_on macos: :tahoe" "$formula"
  if command -v ruby >/dev/null 2>&1; then
    ruby -c "$formula" >/dev/null
  fi
done

for name in agent cli observability; do
  config="${configs}/${name}.json.in"
  [[ -f "$config" ]] || {
    printf 'FAIL: missing Homebrew config template %s\n' "$config" >&2
    exit 1
  }
  grep -qF "@HOMEBREW_PREFIX@" "$config"
  rendered_config="${tmp}/${name}.json"
  sed "s|@HOMEBREW_PREFIX@|/opt/homebrew|g" "$config" >"$rendered_config"
  python3 -m json.tool "$rendered_config" >/dev/null
done

python3 - \
  "${tmp}/agent.json" \
  "${tmp}/cli.json" \
  "${tmp}/observability.json" <<'PY'
import json
import sys
from pathlib import Path

agent, cli, observability = (json.loads(Path(p).read_text()) for p in sys.argv[1:])
prefix = "/opt/homebrew"

assert agent["transport"] == "unix_socket"
assert agent["socket_path"] == f"{prefix}/var/run/tensorplate/agent.sock"
assert agent["worker"]["serving_binary_path"] == (
    f"{prefix}/opt/tensorplate-serving/libexec/tensorplate-serving"
)
assert cli["profiles"]["local"]["socket_path"] == agent["socket_path"]
assert cli["log_source"]["path"] == f"{prefix}/var/log/tensorplate/events.ndjson"
assert observability["diagnostics_retention"]["file_path"] == cli["log_source"]["path"]
PY

python3 - packaging/backend-metadata/python_pytorch.json <<'PY'
import json
import sys
from pathlib import Path

descriptor = json.loads(Path(sys.argv[1]).read_text())
assert "3.14" in descriptor["python"]["supported_versions"]
assert descriptor["python"]["interpreter"] == "/usr/bin/python3"
PY

for component in \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-cli \
  tensorplate-observability \
  tensorplate-backend-python-pytorch; do
  grep -qF "depends_on \"${component}\"" "${templates}/tensorplate.rb" || {
    printf 'FAIL: meta-formula is missing dependency %s\n' "$component" >&2
    exit 1
  }
done

service_block() {
  sed -n '/^  service do$/,/^  end$/p' "$1"
}

require_service_line() {
  formula="$1"
  expected_line="$2"
  block="$(service_block "$formula")"
  if ! grep -qF "$expected_line" <<EOF
$block
EOF
  then
    printf 'FAIL: %s service is missing: %s\n' "$formula" "$expected_line" >&2
    exit 1
  fi
}

for component in tensorplate-agent tensorplate-observability; do
  formula="${templates}/${component}.rb"
  config="${component#tensorplate-}"
  require_service_line "$formula" \
    "    run [opt_bin/\"${component}\", \"--config\", etc/\"tensorplate/${config}.json\"]"
  require_service_line "$formula" "    working_dir var/\"tensorplate\""
  require_service_line "$formula" \
    "    log_path var/\"log/tensorplate/${config}.log\""
  require_service_line "$formula" \
    "    error_log_path var/\"log/tensorplate/${config}.error.log\""
  require_service_line "$formula" "    run_at_load true"
  require_service_line "$formula" "    keep_alive successful_exit: false"
  require_service_line "$formula" "    throttle_interval 5"
done

require_service_line "${templates}/tensorplate-observability.rb" \
  "    environment_variables TP_PLATFORM_REGISTRY_DIR: HOMEBREW_PREFIX/\"share/tensorplate/platform\""

agent_formula="${templates}/tensorplate-agent.rb"
require_service_line "$agent_formula" \
  "                          PYTHONPATH:                   formula_opt_libexec(\"tensorplate-backend-python-pytorch\"),"
require_service_line "$agent_formula" \
  "                          TP_BACKEND_DESCRIPTOR_DIR:    HOMEBREW_PREFIX/\"share/tensorplate/backends\","
require_service_line "$agent_formula" \
  "                          TP_PLATFORM_REGISTRY_DIR:     HOMEBREW_PREFIX/\"share/tensorplate/platform\","
require_service_line "$agent_formula" \
  "                          TP_PYTHON_PYTORCH_EXECUTABLE: formula_opt_libexec(\"pytorch\")/\"bin/python\""
grep -qF '(share/"tensorplate/platform").install \' "$agent_formula"
grep -qF '"config/platform/rows",' "$agent_formula"
grep -qF '"config/platform/roadmap_targets"' "$agent_formula"

backend_formula="${templates}/tensorplate-backend-python-pytorch.rb"
grep -qF 'inreplace descriptor, "/usr/bin/python3", pytorch_python' "$backend_formula"
grep -qF '(share/"tensorplate/backends/python_pytorch").install descriptor => "backend.json"' \
  "$backend_formula"

for component in tensorplate-agent tensorplate-cli tensorplate-observability; do
  formula="${templates}/${component}.rb"
  config="${component#tensorplate-}"
  grep -qF "packaging/homebrew/conf/${config}.json.in" "$formula"
  grep -qF "(etc/\"tensorplate\").install config => \"${config}.json\"" "$formula"
  grep -qF "def post_install" "$formula"
done

grep -qF 'export TENSORPLATE_CLI_CONFIG="#{etc}/tensorplate/cli.json"' \
  "${templates}/tensorplate-cli.rb"
grep -qF 'export TP_BACKEND_DESCRIPTOR_DIR="#{HOMEBREW_PREFIX}/share/tensorplate/backends"' \
  "${templates}/tensorplate-cli.rb"
grep -qF 'export TP_PLATFORM_REGISTRY_DIR="#{HOMEBREW_PREFIX}/share/tensorplate/platform"' \
  "${templates}/tensorplate-cli.rb"
grep -qF 'export PYTHONPATH="#{formula_opt_libexec("tensorplate-backend-python-pytorch")}${PYTHONPATH:+:${PYTHONPATH}}"' \
  "${templates}/tensorplate-cli.rb"

if service_block "${templates}/tensorplate-serving.rb" | grep -q .; then
  printf 'FAIL: serving worker must not define a Homebrew service\n' >&2
  exit 1
fi

"$renderer" \
  --source-url "$release_url" \
  --sha256 "$release_sha" \
  --output-dir "$tmp/Formula" >/dev/null

for name in "${expected[@]}"; do
  rendered="${tmp}/Formula/${name}"
  grep -qF "  url \"${release_url}\"" "$rendered"
  grep -qF "  sha256 \"${release_sha}\"" "$rendered"
  if grep -qF "$placeholder_url" "$rendered" ||
     grep -qF "$placeholder_sha" "$rendered"; then
    printf 'FAIL: placeholder release data remains in %s\n' "$name" >&2
    exit 1
  fi
done

for component in tensorplate-agent tensorplate-observability; do
  template_block="$(service_block "${templates}/${component}.rb")"
  rendered_block="$(service_block "${tmp}/Formula/${component}.rb")"
  [[ "$rendered_block" == "$template_block" ]] || {
    printf 'FAIL: rendered %s service differs from its template\n' "$component" >&2
    exit 1
  }
done

# Exercise the publisher against a local tap whose meta-formula is already
# current but whose five component files are absent. This pins the atomic
# graph behavior: untracked component files must prevent a false no-op.
printf 'source archive fixture\n' >"${tmp}/source.tar.gz"
publisher_sha="$(sha256sum "${tmp}/source.tar.gz" | awk '{print $1}')"
"$renderer" \
  --source-url "$release_url" \
  --sha256 "$publisher_sha" \
  --output-dir "$tmp/current-formulas" >/dev/null

mkdir -p "$tmp/tap-origin/Formula"
cp "$tmp/current-formulas/tensorplate.rb" "$tmp/tap-origin/Formula/"
git init -b main "$tmp/tap-origin" >/dev/null
git -C "$tmp/tap-origin" config user.name "Formula Test"
git -C "$tmp/tap-origin" config user.email "formula-test@example.invalid"
git -C "$tmp/tap-origin" add Formula/tensorplate.rb
git -C "$tmp/tap-origin" commit -m "Seed partial formula graph" >/dev/null

mkdir -p "$tmp/fake-bin"
cat >"$tmp/fake-bin/curl" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
output=""
while [[ $# -gt 0 ]]; do
  if [[ "$1" == "-o" ]]; then
    output="${2:-}"
    shift 2
  else
    shift
  fi
done
[[ -n "$output" ]]
cp "$FAKE_TARBALL" "$output"
EOF
cat >"$tmp/fake-bin/gh" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
case "${1:-} ${2:-}" in
  "auth setup-git")
    exit 0
    ;;
  "repo clone")
    exec "$REAL_GIT" clone --depth=1 "$FAKE_TAP_SOURCE" "${4:-}"
    ;;
  "pr list")
    exit 0
    ;;
  "pr create")
    printf 'https://example.invalid/formula-graph\n'
    ;;
  "pr merge")
    exit 0
    ;;
  *)
    printf 'unexpected fake gh command: %s\n' "$*" >&2
    exit 1
    ;;
esac
EOF
chmod +x "$tmp/fake-bin/curl" "$tmp/fake-bin/gh"

GH_TOKEN="fixture" \
FAKE_TARBALL="${tmp}/source.tar.gz" \
FAKE_TAP_SOURCE="${tmp}/tap-origin" \
REAL_GIT="$(command -v git)" \
PATH="${tmp}/fake-bin:${PATH}" \
  "$publisher" \
    --tag v0.2.1 \
    --source-repo tensorplate/tensorplate \
    --tap-repo tensorplate/homebrew-tap >/dev/null 2>&1

for name in "${expected[@]}"; do
  git -C "$tmp/tap-origin" cat-file -e "bump-tensorplate-0.2.1:Formula/${name}"
done

printf 'homebrew formula template checks green\n'
