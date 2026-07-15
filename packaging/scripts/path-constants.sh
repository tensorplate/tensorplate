# SPDX-License-Identifier: Apache-2.0
#
# Sourced by maintainer scripts and packaging tests. Single source of
# truth for the v0.1.0 install layout. Kept in lockstep with
# `protocol/rust/src/install_paths.rs`. The packaging verification
# suite reads both and asserts identical values.

# shellcheck shell=sh

TP_SYSTEM_USER="tensorplate"
TP_SYSTEM_GROUP="tensorplate"

TP_ETC_DIR="/etc/tensorplate"
TP_AGENT_CONFIG_PATH="/etc/tensorplate/agent.json"
TP_OBSERVABILITY_CONFIG_PATH="/etc/tensorplate/observability.json"
TP_SERVING_WORKER_CONFIG_PATH="/etc/tensorplate/serving_worker.json"
TP_CLI_CONFIG_PATH="/etc/tensorplate/cli.json"

TP_STATE_DIR="/var/lib/tensorplate"
TP_STATE_INNER_DIR="/var/lib/tensorplate/state"
TP_BUNDLE_STAGING_DIR="/var/lib/tensorplate/bundles/staging"
TP_BUNDLE_ACTIVE_DIR="/var/lib/tensorplate/bundles/active"
TP_BUNDLE_PREVIOUS_DIR="/var/lib/tensorplate/bundles/previous"
TP_BUNDLE_QUARANTINE_DIR="/var/lib/tensorplate/bundles/quarantine"
TP_BUNDLE_IMPORT_DIR="/var/lib/tensorplate/bundles/import"
TP_WORKER_CONFIG_DIR="/var/lib/tensorplate/worker-configs"

TP_LOG_DIR="/var/log/tensorplate"
TP_RUN_DIR="/run/tensorplate"
TP_AGENT_SOCKET_PATH="/run/tensorplate/agent.sock"

TP_BACKEND_DESCRIPTOR_DIR="/usr/share/tensorplate/backends"
TP_PYTHON_PYTORCH_BACKEND_DESCRIPTOR="/usr/share/tensorplate/backends/python_pytorch/backend.json"
TP_SERVING_BINARY_PATH="/usr/lib/tensorplate/tensorplate-serving"

TP_DIR_MODE="0750"
# Bundle import dir: sticky + group-writable so an SSH copy user in the
# tensorplate group can stage bundles without deleting each other's.
TP_IMPORT_DIR_MODE="1775"
TP_CONF_FILE_MODE="0640"
TP_CLI_FILE_MODE="0644"
TP_SOCKET_MODE="0660"

TP_REQUIRED_DIRECTORIES="${TP_ETC_DIR} ${TP_STATE_DIR} ${TP_STATE_INNER_DIR} \
  ${TP_BUNDLE_STAGING_DIR} ${TP_BUNDLE_ACTIVE_DIR} ${TP_BUNDLE_PREVIOUS_DIR} \
  ${TP_BUNDLE_QUARANTINE_DIR} ${TP_BUNDLE_IMPORT_DIR} ${TP_WORKER_CONFIG_DIR} \
  ${TP_LOG_DIR} ${TP_RUN_DIR} ${TP_BACKEND_DESCRIPTOR_DIR}"

TP_REQUIRED_CONFIG_FILES="${TP_AGENT_CONFIG_PATH} ${TP_OBSERVABILITY_CONFIG_PATH} \
  ${TP_SERVING_WORKER_CONFIG_PATH} ${TP_CLI_CONFIG_PATH}"
