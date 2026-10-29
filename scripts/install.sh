#!/bin/bash
set -euo pipefail

# ── Fastab macOS installer ──────────────────────────────────────────

APP_NAME="fastab"          # binary / process name (no spaces)
APP_DISPLAY="FasTab"       # human-readable product name
APP_BUNDLE_NAME="Fastab.app" # stable installation path
BUNDLE_ID="app.fastab"

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
STAGING_BUNDLE="${REPO_DIR}/build/${APP_BUNDLE_NAME}"
APP_BUNDLE="/Applications/${APP_BUNDLE_NAME}"
LOCAL_BIN="${HOME}/.local/bin"

GREEN='\033[0;32m'; YELLOW='\033[0;33m'; RED='\033[0;31m'; NC='\033[0m'
info()  { echo -e "${GREEN}==>${NC} $*"; }
warn()  { echo -e "${YELLOW}==>${NC} $*"; }
error() { echo -e "${RED}==>${NC} $*" >&2; }

file_sha() {
  local f="$1"
  if [ -f "$f" ]; then
    shasum -a 256 "$f" | awk '{print $1}'
  fi
}

# Same rule as fastab_integrations::input_method::code_identity: the code
# digest the IME's build script writes into its Info.plist, else (bundles from
# before it) the executable's SHA-256. The executable changes on every release
# even when the IME's code does not, so comparing it replaced the helper, and
# cut every open terminal's IMK connection, on each install.
ime_code_identity() {
  local helper="$1/Contents/Helpers/FastabInputMethod.app"
  local plist="${helper}/Contents/Info.plist"
  local digest=""
  # Only ask PlistBuddy about a file that exists: for a missing one it prints
  # "File Doesn't Exist, Will Create: ..." on stdout, which would read as a
  # digest and skip the executable fallback.
  if [ -f "${plist}" ]; then
    digest="$(/usr/libexec/PlistBuddy -c 'Print :FastabCodeDigest' "${plist}" 2>/dev/null || true)"
  fi
  if [ -n "${digest}" ]; then
    printf 'code:%s\n' "${digest}"
  else
    file_sha "${helper}/Contents/MacOS/fastab_input_method"
  fi
}

# Preserve unrelated tools when the shorter PTY name is already in use.
link_pty_binary() {
  local name="$1" link="${LOCAL_BIN}/$1"
  if [ -L "${link}" ]; then
    case "$(readlink "${link}")" in
      "${APP_BUNDLE}/Contents/MacOS/fterm"|"${APP_BUNDLE}/Contents/MacOS/fastabterm") ;;
      *) warn "Keeping existing ${link}; it does not belong to FasTab."; return 0 ;;
    esac
  elif [ -e "${link}" ]; then
    warn "Keeping existing ${link}; it does not belong to FasTab."
    return 0
  fi
  ln -sfn "${APP_BUNDLE}/Contents/MacOS/${name}" "${link}"
}

process_running() {
  pgrep -x "$1" >/dev/null 2>&1
}

process_running_match() {
  pgrep -f "$1" >/dev/null 2>&1
}

# Kill only Fastab's IME. The current executable is `fastab_input_method`.
# Builds before this rename used `fig_input_method` inside the same bundle.
# Both patterns include FastabInputMethod.app, so Easy Complete's helper stays up.
FASTAB_IME_MATCHES=(
  "FastabInputMethod.app/Contents/MacOS/fastab_input_method"
  "FastabInputMethod.app/Contents/MacOS/fig_input_method"
)

fastab_ime_running() {
  local match
  for match in "${FASTAB_IME_MATCHES[@]}"; do
    if process_running_match "${match}"; then
      return 0
    fi
  done
  return 1
}

stop_fastab_ime() {
  fastab_ime_running || return 0
  local match
  for match in "${FASTAB_IME_MATCHES[@]}"; do
    pkill -f "${match}" 2>/dev/null || true
  done
  local waited=0
  while [ "${waited}" -lt 20 ]; do
    fastab_ime_running || return 0
    sleep 0.1
    waited=$((waited + 1))
  done
  warn "FasTab IME did not exit; forcing it."
  for match in "${FASTAB_IME_MATCHES[@]}"; do
    pkill -9 -f "${match}" 2>/dev/null || true
  done
  sleep 0.2
}

# SIGTERM and wait for the process to actually be gone, so the bundle is only
# replaced (and `open` only called) once nothing is holding the old one.
stop_process() {
  local name="$1"
  process_running "${name}" || return 0

  pkill -x "${name}" 2>/dev/null || true
  local waited=0
  while [ "${waited}" -lt 20 ]; do
    process_running "${name}" || return 0
    sleep 0.1
    waited=$((waited + 1))
  done

  warn "${name} did not exit; forcing it."
  pkill -9 -x "${name}" 2>/dev/null || true
  sleep 0.2
}

# Ask the relaunched desktop process whether its Accessibility grant is in
# effect, and leave the answer in `accessibility_state`: true, false, or
# unknown when the app never answered. `ftab debug accessibility status` reports
# the desktop process's own `AXIsProcessTrusted()` over its local socket, so it
# speaks for the binary that was just installed, not for this shell. The socket
# takes a moment to come up after `open`, and a `false` only counts once it has
# been repeated, so a TCC lookup that is still settling cannot trigger a reset.
probe_accessibility() {
  accessibility_state=unknown
  local answer="" false_count=0 tries=0
  while [ "${tries}" -lt 15 ]; do
    answer="$(ftab debug accessibility status 2>/dev/null | awk '/^Accessibility Enabled: /{print $3}' || true)"
    case "${answer}" in
      true)
        accessibility_state=true
        return 0
        ;;
      false)
        false_count=$((false_count + 1))
        if [ "${false_count}" -ge 3 ]; then
          accessibility_state=false
          return 0
        fi
        ;;
    esac
    sleep 1
    tries=$((tries + 1))
  done
  # Under `set -e` the caller must see success either way; the answer is in
  # `accessibility_state`, not the return code.
  return 0
}

# ── 1. Build & assemble the .app ──────────────────────────────────────────────
# Shared with CI (see .github/workflows/release.yml) so the bundle is assembled
# identically whether installed locally or packaged into a release DMG.
"${REPO_DIR}/scripts/build-app.sh"

# ── 3. Install to /Applications ───────────────────────────────────────────────
info "Installing to /Applications/..."

DESKTOP_BIN="Contents/MacOS/${APP_NAME}"
IME_BIN="Contents/Helpers/FastabInputMethod.app/Contents/MacOS/fastab_input_method"

for required in "${DESKTOP_BIN}" "Contents/MacOS/fterm" "Contents/MacOS/fastabterm" "${IME_BIN}"; do
  if [ ! -f "${STAGING_BUNDLE}/${required}" ]; then
    error "Build produced no ${required}. Refusing to install over the current bundle."
    exit 1
  fi
done

desktop_changed=1
if [ "$(file_sha "${APP_BUNDLE}/${DESKTOP_BIN}")" = "$(file_sha "${STAGING_BUNDLE}/${DESKTOP_BIN}")" ]; then
  desktop_changed=0
fi
ime_changed=1
installed_ime="$(ime_code_identity "${APP_BUNDLE}")"
if [ -n "${installed_ime}" ] && [ "${installed_ime}" = "$(ime_code_identity "${STAGING_BUNDLE}")" ]; then
  ime_changed=0
fi

# The desktop app always goes down, whether or not its binary changed: the
# bundle below is wiped and re-dittoed, and Contents/Resources/specs-ir is read
# lazily at completion time, so replacing it under a live process silently
# breaks completions until the next restart. It has no clients to preserve and
# is relaunched at the end of this script.
stop_process "${APP_NAME}"

# The IME is the one process worth keeping alive: open Otty / Ghostty / Kitty
# windows hold IMK connections to it and macOS never re-attaches them to a
# replacement. Same bytes → leave it running.
keep_ime=0
if [ "${ime_changed}" -eq 1 ]; then
  stop_fastab_ime
elif fastab_ime_running; then
  keep_ime=1
fi

# Preserve framework symlinks and bundle metadata. On macOS, `cp -r` follows
# symlinks while traversing and expands Sparkle.framework's versioned layout,
# which makes the installed app fail `codesign --verify --deep --strict`.
# When the IME stays up, keep Contents/Helpers where it is — deleting that
# bundle out from under the process drops its IMK clients. Everything else
# still has to go, because `ditto` merges into the destination and would
# otherwise leave files from the previous build behind.
if [ "${keep_ime}" -eq 1 ] && [ -d "${APP_BUNDLE}/Contents" ]; then
  find "${APP_BUNDLE}/Contents" -mindepth 1 -maxdepth 1 ! -name Helpers -print0 | xargs -0 rm -rf
else
  rm -rf "${APP_BUNDLE}"
fi
ditto "${STAGING_BUNDLE}" "${APP_BUNDLE}"

# ── 4. Symlink CLI binaries to ~/.local/bin ───────────────────────────────────
info "Linking binaries to ${LOCAL_BIN}..."
mkdir -p "${LOCAL_BIN}"
ln -sf "/Applications/${APP_BUNDLE_NAME}/Contents/MacOS/ftab"     "${LOCAL_BIN}/ftab"
link_pty_binary fterm
link_pty_binary fastabterm

# ── 6. Shell integration ───────────────────────────────────────────────────────
info "Installing shell integration..."
export PATH="${LOCAL_BIN}:${PATH}"
ftab integrations install --silent dotfiles 2>/dev/null || {
  warn "Automatic shell setup skipped. Run manually:"
  warn "  ftab integrations install dotfiles"
}

# ── 7. Input Method ────────────────────────────────────────────────────────────
info "Registering Input Method..."
# `ftab integrations install input-method` launches the IME when it is down, and
# replaces it only when the on-disk helper is not the binary we last started.
ftab integrations install --silent input-method 2>/dev/null || {
  warn "Input method registration skipped (only needed for Kitty/Alacritty/Zed/Ghostty/WezTerm)"
}

# ── 8. Accessibility permission ─────────────────────────────────────────────────
open "${APP_BUNDLE}"

# A new desktop binary carries a new ad-hoc signature, and TCC pins the
# Accessibility grant to a cdhash — but whether that costs the grant depends on
# the OS. macOS 26 re-pins the stored requirement to the new binary on its own
# (TCC.db ends up holding the fresh cdhash with auth_reason "System Set") and
# the app comes up trusted; older releases keep the stale requirement, leave the
# checkbox in System Settings ticked, and fail every AX call. So ask the new
# process instead of assuming. Resetting first would throw away a grant that
# carried over and send the user back to System Settings on every install.
# Same binary → same signature → nothing to check.
accessibility_reset=0
if [ "${desktop_changed}" -eq 1 ]; then
  info "Checking whether the Accessibility grant survived the new binary..."
  probe_accessibility
  case "${accessibility_state}" in
    true)
      info "Accessibility grant carried over."
      ;;
    false)
      info "Accessibility grant did not survive; resetting it..."
      tccutil reset Accessibility "${BUNDLE_ID}" 2>/dev/null || true
      accessibility_reset=1
      warn "Open FasTab Settings and click Grant Accessibility."
      ;;
    *)
      warn "Could not reach the desktop app to check its Accessibility grant. Once it is running, run:"
      warn "  ftab debug accessibility status"
      warn "If it reports false, open FasTab Settings and click Grant Accessibility."
      ;;
  esac
fi

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
info "${APP_DISPLAY} installation complete!"
echo ""
echo "  App:  /Applications/${APP_BUNDLE_NAME}"
echo "  CLI:  ${LOCAL_BIN}/ftab  ($(ftab --version 2>/dev/null || echo 'restart shell to verify'))"
echo ""
if [ "${accessibility_reset}" -eq 1 ]; then
  echo "  If autocomplete does not appear, open FasTab Settings and click"
  echo "    Grant Accessibility, then drag FasTab into the list."
fi
