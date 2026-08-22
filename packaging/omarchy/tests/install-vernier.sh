#!/usr/bin/env bash

set -euo pipefail

readonly test_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly installer="$(cd -- "$test_dir/.." && pwd)/scripts/install-vernier"

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

assert_contains() {
  local file=$1
  local expected=$2
  grep -Fqx -- "$expected" "$file" ||
    fail "expected '$expected' in $file"
}

assert_not_contains() {
  local file=$1
  local unexpected=$2
  if grep -Fqx -- "$unexpected" "$file"; then
    fail "did not expect '$unexpected' in $file"
  fi
}

new_fixture() {
  fixture_root="$(mktemp -d)"
  mock_bin="$fixture_root/bin"
  mock_log="$fixture_root/calls.log"
  mock_state="$fixture_root/state"
  runtime_dir="$fixture_root/runtime"
  install_state="$runtime_dir/vernier-companion/install-state"
  mkdir -p -- "$mock_bin" "$mock_state" "$runtime_dir"
  chmod 0700 "$runtime_dir"
  : >"$mock_log"

  # Give the installer and its mocks only the small toolset they need. Keeping
  # /usr/bin off this PATH ensures the workstation's own Vernier installation
  # cannot make the missing-binary fixture pass accidentally.
  ln -s /usr/bin/bash "$mock_bin/bash"
  ln -s /usr/bin/cat "$mock_bin/cat"
  ln -s /usr/bin/chmod "$mock_bin/chmod"
  ln -s /usr/bin/flock "$mock_bin/flock"
  ln -s /usr/bin/install "$mock_bin/install"
  ln -s /usr/bin/mktemp "$mock_bin/mktemp"
  ln -s /usr/bin/mv "$mock_bin/mv"
  ln -s /usr/bin/rm "$mock_bin/rm"
  ln -s /usr/bin/sleep "$mock_bin/sleep"
  ln -s /usr/bin/touch "$mock_bin/touch"

  cat >"$mock_bin/omarchy" <<'MOCK'
#!/usr/bin/env bash
printf 'omarchy %s\n' "$*" >>"$MOCK_LOG"
[[ ${MOCK_OMARCHY_EXIT:-0} == 0 ]] || exit "$MOCK_OMARCHY_EXIT"
if [[ ${MOCK_BLOCK_INSTALL:-0} == 1 ]]; then
  touch "$MOCK_STATE/install-entered"
  while [[ ! -e $MOCK_STATE/release-install ]]; do sleep 0.01; done
fi
touch "$MOCK_STATE/package-installed"
if [[ ${MOCK_CREATE_BINARY:-1} == 1 ]]; then
  cat >"$MOCK_BIN/vernier" <<'VERNIER'
#!/usr/bin/env bash
if [[ ${1:-} == start && ${2:-} == --help ]]; then
  [[ ${MOCK_SUPPORTS_START:-1} == 1 ]]
  exit
fi
exit 0
VERNIER
  chmod +x "$MOCK_BIN/vernier"
fi
MOCK

  cat >"$mock_bin/pacman" <<'MOCK'
#!/usr/bin/env bash
printf 'pacman %s\n' "$*" >>"$MOCK_LOG"
[[ $1 == -Q && $2 == vernier-bin ]] || exit 64
[[ -f $MOCK_STATE/package-installed ]]
MOCK

  cat >"$mock_bin/uwsm-app" <<'MOCK'
#!/usr/bin/env bash
printf 'uwsm-app' >>"$MOCK_LOG"
printf ' %s' "$@" >>"$MOCK_LOG"
printf '\n' >>"$MOCK_LOG"
exit "${MOCK_UWSM_EXIT:-0}"
MOCK

  chmod +x "$mock_bin/omarchy" "$mock_bin/pacman" "$mock_bin/uwsm-app"
}

run_installer() {
  env \
    PATH="$mock_bin" \
    XDG_RUNTIME_DIR="$runtime_dir" \
    MOCK_BIN="$mock_bin" \
    MOCK_LOG="$mock_log" \
    MOCK_STATE="$mock_state" \
    MOCK_CREATE_BINARY="${MOCK_CREATE_BINARY:-1}" \
    MOCK_BLOCK_INSTALL="${MOCK_BLOCK_INSTALL:-0}" \
    MOCK_OMARCHY_EXIT="${MOCK_OMARCHY_EXIT:-0}" \
    MOCK_SUPPORTS_START="${MOCK_SUPPORTS_START:-1}" \
    MOCK_UWSM_EXIT="${MOCK_UWSM_EXIT:-0}" \
    "$installer" >"$fixture_root/stdout" 2>"$fixture_root/stderr"
}

test_installs_verifies_and_starts() {
  local MOCK_CREATE_BINARY=1
  local MOCK_BLOCK_INSTALL=0
  local MOCK_OMARCHY_EXIT=0
  local MOCK_SUPPORTS_START=1
  local MOCK_UWSM_EXIT=0
  new_fixture
  trap 'rm -rf -- "$fixture_root"' RETURN

  run_installer

  assert_contains "$mock_log" "omarchy pkg aur add vernier-bin"
  assert_contains "$mock_log" "pacman -Q vernier-bin"
  assert_contains "$mock_log" "uwsm-app -s b -t service -a vernier -- $mock_bin/vernier start"
  grep -Fq 'installed and starting' "$fixture_root/stdout" ||
    fail "success output was not shown"
  [[ $(<"$install_state") == ready ]] ||
    fail "successful install did not publish ready"
  [[ $(stat -c %a "$runtime_dir/vernier-companion") == 700 ]] ||
    fail "runtime state directory is not mode 0700"
  [[ $(stat -c %a "$install_state") == 600 ]] ||
    fail "runtime state file is not mode 0600"
  if compgen -G "$runtime_dir/vernier-companion/.install-state.*" >/dev/null; then
    fail "atomic state update left a temporary file behind"
  fi
}

test_existing_binary_skips_package_install() {
  local MOCK_CREATE_BINARY=1
  local MOCK_BLOCK_INSTALL=0
  local MOCK_OMARCHY_EXIT=0
  local MOCK_SUPPORTS_START=1
  local MOCK_UWSM_EXIT=0
  new_fixture
  trap 'rm -rf -- "$fixture_root"' RETURN
  cat >"$mock_bin/vernier" <<'MOCK'
#!/usr/bin/env bash
if [[ ${1:-} == start && ${2:-} == --help ]]; then
  [[ ${MOCK_SUPPORTS_START:-1} == 1 ]]
  exit
fi
exit 0
MOCK
  chmod +x "$mock_bin/vernier"

  run_installer

  assert_not_contains "$mock_log" "omarchy pkg aur add vernier-bin"
  assert_not_contains "$mock_log" "pacman -Q vernier-bin"
  assert_contains "$mock_log" "uwsm-app -s b -t service -a vernier -- $mock_bin/vernier start"
  [[ $(<"$install_state") == ready ]] ||
    fail "existing install did not publish ready"
}

test_legacy_binary_uses_bare_launch() {
  local MOCK_CREATE_BINARY=1
  local MOCK_BLOCK_INSTALL=0
  local MOCK_OMARCHY_EXIT=0
  local MOCK_SUPPORTS_START=0
  local MOCK_UWSM_EXIT=0
  new_fixture
  trap 'rm -rf -- "$fixture_root"' RETURN

  run_installer

  assert_contains "$mock_log" "uwsm-app -s b -t service -a vernier -- $mock_bin/vernier"
  assert_not_contains "$mock_log" "uwsm-app -s b -t service -a vernier -- $mock_bin/vernier start"
  [[ $(<"$install_state") == ready ]] ||
    fail "legacy install did not publish ready"
}

test_uwsm_handoff_failure_is_reported() {
  local MOCK_CREATE_BINARY=1
  local MOCK_BLOCK_INSTALL=0
  local MOCK_OMARCHY_EXIT=0
  local MOCK_SUPPORTS_START=1
  local MOCK_UWSM_EXIT=23
  new_fixture
  trap 'rm -rf -- "$fixture_root"' RETURN

  if run_installer; then
    fail "installer unexpectedly succeeded after UWSM rejected the launch"
  fi

  assert_contains "$mock_log" "uwsm-app -s b -t service -a vernier -- $mock_bin/vernier start"
  [[ $(<"$install_state") == failed ]] ||
    fail "UWSM handoff failure did not publish failed"
  grep -Fq 'UWSM could not start Vernier' "$fixture_root/stderr" ||
    fail "UWSM handoff failure was not explained"
}

test_install_failure_does_not_start() {
  local MOCK_CREATE_BINARY=1
  local MOCK_BLOCK_INSTALL=0
  local MOCK_OMARCHY_EXIT=42
  local MOCK_SUPPORTS_START=1
  local MOCK_UWSM_EXIT=0
  new_fixture
  trap 'rm -rf -- "$fixture_root"' RETURN

  if run_installer; then
    fail "installer unexpectedly succeeded after Omarchy failed"
  fi

  assert_contains "$mock_log" "omarchy pkg aur add vernier-bin"
  assert_not_contains "$mock_log" "pacman -Q vernier-bin"
  if grep -Fq 'uwsm-app' "$mock_log"; then
    fail "daemon launch ran after an install failure"
  fi
  [[ $(<"$install_state") == failed ]] ||
    fail "failed install did not publish failed"
}

test_missing_binary_after_install_does_not_start() {
  local MOCK_CREATE_BINARY=0
  local MOCK_BLOCK_INSTALL=0
  local MOCK_OMARCHY_EXIT=0
  local MOCK_SUPPORTS_START=1
  local MOCK_UWSM_EXIT=0
  new_fixture
  trap 'rm -rf -- "$fixture_root"' RETURN

  if run_installer; then
    fail "installer unexpectedly succeeded without a vernier executable"
  fi

  assert_contains "$mock_log" "omarchy pkg aur add vernier-bin"
  assert_contains "$mock_log" "pacman -Q vernier-bin"
  if grep -Fq 'uwsm-app' "$mock_log"; then
    fail "daemon launch ran without a vernier executable"
  fi
  [[ $(<"$install_state") == failed ]] ||
    fail "incomplete install did not publish failed"
}

test_publishes_installing_and_rejects_duplicates() {
  local MOCK_CREATE_BINARY=1
  local MOCK_BLOCK_INSTALL=1
  local MOCK_OMARCHY_EXIT=0
  local MOCK_SUPPORTS_START=1
  local MOCK_UWSM_EXIT=0
  local install_pid
  new_fixture
  trap 'rm -rf -- "$fixture_root"' RETURN

  run_installer &
  install_pid=$!
  for _ in {1..100}; do
    [[ -f $mock_state/install-entered && -f $install_state ]] && break
    sleep 0.01
  done
  [[ $(<"$install_state") == installing ]] ||
    fail "active install did not publish installing"

  if run_installer; then
    fail "a duplicate installer unexpectedly acquired the lock"
  fi
  [[ $(<"$install_state") == installing ]] ||
    fail "duplicate invocation overwrote the active installer state"

  touch "$mock_state/release-install"
  wait "$install_pid"
  [[ $(<"$install_state") == ready ]] ||
    fail "completed blocked install did not publish ready"
}

test_requires_session_runtime_directory() {
  local MOCK_CREATE_BINARY=1
  local MOCK_BLOCK_INSTALL=0
  local MOCK_OMARCHY_EXIT=0
  local MOCK_SUPPORTS_START=1
  local MOCK_UWSM_EXIT=0
  new_fixture
  trap 'rm -rf -- "$fixture_root"' RETURN

  if env -u XDG_RUNTIME_DIR PATH="$mock_bin" "$installer" \
    >"$fixture_root/stdout" 2>"$fixture_root/stderr"; then
    fail "installer unexpectedly accepted a missing XDG_RUNTIME_DIR"
  fi
  [[ ! -e $runtime_dir/vernier-companion ]] ||
    fail "missing-runtime failure created companion state"
}

test_installs_verifies_and_starts
test_existing_binary_skips_package_install
test_legacy_binary_uses_bare_launch
test_uwsm_handoff_failure_is_reported
test_install_failure_does_not_start
test_missing_binary_after_install_does_not_start
test_publishes_installing_and_rejects_duplicates
test_requires_session_runtime_directory

printf 'PASS: Omarchy installer flow\n'
