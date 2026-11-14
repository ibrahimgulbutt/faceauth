#!/usr/bin/env bash
# FaceAuth installer — face unlock for Linux
#
#   curl -fsSL https://raw.githubusercontent.com/ibrahimgulbutt/faceauth/main/install.sh | bash
#   ./install.sh                 # from a checkout: builds and installs that checkout
#   ./install.sh --help
#
# Run as your normal user; sudo is used for the steps that need it.
# The flow is documented in docs/DESIGN.md ("Install, upgrade, uninstall").

set -euo pipefail

FACEAUTH_REF="${FACEAUTH_REF:-v1.0.0}"
REPO_URL="https://github.com/ibrahimgulbutt/faceauth.git"
PREFIX="/usr/local"
MODELS_DIR="/usr/share/faceauth/models"
CONFIG_DIR="/etc/faceauth"
STATE_DIR="/var/lib/faceauth"
OLD_STATE_DIR="/root/.local/share/faceauth"
PAM_RECORD="$CONFIG_DIR/pam-services"
DAEMON_USER="faceauth"
PAM_LINE="auth       sufficient   pam_faceauth.so"
MIN_CARGO="1.82.0"

# INT8 models shipped by versions before 1.0. They are replaced by
# InsightFace's original files (see dist/fetch-models.sh) and removed when
# their checksum shows they are ours.
declare -A OLD_MODEL_SHA256=(
    [det_500m_int8.onnx]="c44b7600cda82186285f587e744e3c4a6171f24e464f8eecb2996bbf331fb9b5"
    [arcface_int8.onnx]="7caa7fb0ad6c37b33e6c59f6411084b487aaabd596bba645d2cce391a5871120"
)

# ── Output helpers ───────────────────────────────────────────────────────────

if [ -t 1 ]; then
    BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[0;31m'; GREEN=$'\033[0;32m'
    YELLOW=$'\033[0;33m'; BLUE=$'\033[0;34m'; NC=$'\033[0m'
else
    BOLD=""; DIM=""; RED=""; GREEN=""; YELLOW=""; BLUE=""; NC=""
fi

step() { printf '\n%s==>%s %s%s%s\n' "$BLUE" "$NC" "$BOLD" "$*" "$NC"; }
info() { printf '    %s\n' "$*"; }
ok()   { printf '    %s✓%s %s\n' "$GREEN" "$NC" "$*"; }
warn() { printf '    %s!%s %s\n' "$YELLOW" "$NC" "$*" >&2; }
die()  { printf '\n%sError:%s %s\n' "$RED" "$NC" "$*" >&2; exit 1; }

usage() {
    cat <<EOF
FaceAuth installer

Usage: install.sh [options]

Options:
  --pam=LIST   Where to enable face unlock, comma separated, without asking:
               sudo, login, lock, polkit, or none.
  --yes, -y    Don't ask questions; use the defaults (face unlock for sudo only).
  --no-gui     Don't build or install the Face Unlock app.
  --no-animation  Don't install the animated face for GNOME (lock screen,
               sudo and admin prompts).
  --model=NAME Face recognizer: accurate (ResNet50, ~300 MB download,
               recommended) or fast (MobileFaceNet, ~15 MB). Default: accurate.
  --camera=KIND  ir (infrared camera) or auto (normal colour camera).
               Asked when an IR camera is present; IR is the default.
  -h, --help   Show this help.

Environment:
  FACEAUTH_REF  Git tag or branch to install when not run from a checkout
                (default: $FACEAUTH_REF).

Run from a checkout (./install.sh), it builds and installs that checkout.
EOF
}

# ── Arguments ────────────────────────────────────────────────────────────────

PAM_CHOICE=""
MODEL_CHOICE=""
CAMERA_CHOICE=""
ASSUME_YES=0
WANT_GUI=1
WANT_ANIMATION=1
for arg in "$@"; do
    case "$arg" in
        --pam=*)  PAM_CHOICE="${arg#--pam=}" ;;
        --model=*) MODEL_CHOICE="${arg#--model=}" ;;
        --camera=*) CAMERA_CHOICE="${arg#--camera=}" ;;
        --yes|-y) ASSUME_YES=1 ;;
        --no-gui) WANT_GUI=0 ;;
        --no-animation) WANT_ANIMATION=0 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "Unknown option: $arg" ;;
    esac
done

# `curl | bash` feeds the script itself on stdin, so prompts must read from the
# terminal directly or they would swallow the rest of the script.
HAVE_TTY=0
if { : </dev/tty; } 2>/dev/null; then
    HAVE_TTY=1
fi

# ask PROMPT DEFAULT — prints the answer (DEFAULT when not interactive).
ask() {
    local reply=""
    if [ "$ASSUME_YES" -eq 1 ] || [ "$HAVE_TTY" -eq 0 ]; then
        printf '%s\n' "$2"
        return
    fi
    read -r -p "$1" reply </dev/tty || reply=""
    printf '%s\n' "${reply:-$2}"
}

# ── Banner and preflight ─────────────────────────────────────────────────────

printf '\n%sFaceAuth installer%s\n%sFace unlock for Linux%s\n' "$BOLD" "$NC" "$DIM" "$NC"

if [ "${EUID:-$(id -u)}" -eq 0 ]; then
    die "Run this as your regular user, not root. It asks for sudo when it needs it."
fi
command -v sudo >/dev/null 2>&1 || die "sudo is required."
command -v systemctl >/dev/null 2>&1 || die "FaceAuth needs systemd (systemctl not found)."

step "Checking your system"
[ -f /etc/os-release ] || die "Cannot detect your distribution (/etc/os-release missing)."
# shellcheck disable=SC1091
. /etc/os-release
OS_ID="${ID:-unknown}"
OS_LIKE="${ID_LIKE:-}"
info "${PRETTY_NAME:-$OS_ID} · $(uname -m) · kernel $(uname -r)"

# ── Already installed? ──
# The version this run installs: the checkout's own, or the release tag.
workspace_version() { awk -F'"' '/^\[workspace.package\]/{s=1} s && /^version/{print $2; exit}' "$1" 2>/dev/null; }
is_version() { [[ "$1" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; }
TARGET_VERSION=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    TARGET_VERSION=$(workspace_version "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/Cargo.toml")
fi
TARGET_VERSION="${TARGET_VERSION:-${FACEAUTH_REF#v}}"

INSTALLED_VERSION=""
if [ -x "$PREFIX/bin/faceauth" ] || [ -x "$PREFIX/bin/faceauthd" ] || [ -f /etc/systemd/system/faceauth.service ]; then
    INSTALLED_VERSION=$("$PREFIX/bin/faceauth" --version 2>/dev/null | awk '{print $2}')
    is_version "$INSTALLED_VERSION" || INSTALLED_VERSION="old"   # versions before 1.0 can't tell
fi
# version_lt A B — A is older than B
version_lt() { [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1)" = "$1" ]; }

INSTALL_KIND=install
if [ -z "$INSTALLED_VERSION" ]; then
    ok "Installing FaceAuth $TARGET_VERSION"
elif [ "$INSTALLED_VERSION" = "old" ]; then
    INSTALL_KIND=upgrade
    ok "An older FaceAuth (before 1.0) is installed. Upgrading it to $TARGET_VERSION."
    info "Your settings and face data are moved over. You'll set up face unlock once more"
    info "afterwards, because the face model changed."
elif ! is_version "$TARGET_VERSION"; then
    INSTALL_KIND=upgrade
    ok "FaceAuth $INSTALLED_VERSION is installed. Installing $FACEAUTH_REF over it (settings and face data are kept)."
elif [ "$INSTALLED_VERSION" = "$TARGET_VERSION" ]; then
    INSTALL_KIND=repair
    ok "FaceAuth $INSTALLED_VERSION is already installed."
    info "Reinstalling it to repair anything missing. Your settings and face data are kept."
elif version_lt "$INSTALLED_VERSION" "$TARGET_VERSION"; then
    INSTALL_KIND=upgrade
    ok "FaceAuth $INSTALLED_VERSION is installed. Upgrading to $TARGET_VERSION (settings and face data are kept)."
else
    warn "FaceAuth $INSTALLED_VERSION is installed, which is newer than this installer ($TARGET_VERSION)."
    case "$(ask "    Replace it with the older $TARGET_VERSION? [y/N] " n)" in
        [Yy]*) INSTALL_KIND=downgrade ;;
        *) info "Nothing was changed."; exit 0 ;;
    esac
fi

if compgen -G "/dev/video*" >/dev/null; then
    ok "Camera device found"
else
    warn "No camera found (/dev/video*). You can install now and connect one later."
fi

# config_value SECTION KEY — a string value from the installed config, if any.
config_value() {
    sed -n "/^[[:space:]]*\\[$1\\]/,/^[[:space:]]*\\[/s/^[[:space:]]*$2[[:space:]]*=[[:space:]]*\"\\(.*\\)\".*/\\1/p" \
        /etc/faceauth/config.toml 2>/dev/null | head -n1
}
CURRENT_MODEL=$(config_value recognition model)
if [ -z "$MODEL_CHOICE" ]; then
    default=1
    [ "$CURRENT_MODEL" = "fast" ] && default=2
    printf '\n    Which face recognizer should FaceAuth use?\n'
    printf '      1) Accurate  ResNet50, much better at telling similar faces apart (~300 MB download) [recommended]\n'
    printf '      2) Fast      MobileFaceNet, smaller and quicker (~15 MB download)\n'
    [ -n "$CURRENT_MODEL" ] && printf '      (currently: %s; changing it means setting up face unlock again)\n' "$CURRENT_MODEL"
    case "$(ask "    Choose [$default]: " "$default")" in
        2|fast) MODEL_CHOICE=fast ;;
        *) MODEL_CHOICE=accurate ;;
    esac
fi
case "$MODEL_CHOICE" in
    accurate|fast) ok "Recognizer: $MODEL_CHOICE" ;;
    *) die "Unknown --model '$MODEL_CHOICE' (use accurate or fast)." ;;
esac

# Infrared camera: the UVC interface name says "IR Camera" even when the
# device name doesn't (combined RGB + IR modules).
IR_CAMERA=""
for node in /sys/class/video4linux/video*; do
    [ -e "$node" ] || continue
    for f in "$node/device/interface" "$node/name"; do
        if [ -r "$f" ] && grep -qiE '(^|[^[:alnum:]])(ir|infrared)([^[:alnum:]]|$)' "$f"; then
            IR_CAMERA="$(tr -d '\n' <"$f")"
            break 2
        fi
    done
done
CURRENT_CAMERA=$(config_value camera device)
# An infrared camera is preferred: it works in the dark and is harder to fool.
# (--camera=auto keeps the colour camera.) If its light can't be switched on,
# the colour camera is used after all; see "Checking the infrared light".
if [ -z "$CAMERA_CHOICE" ] && [ -n "$IR_CAMERA" ]; then
    default=1
    [ "$CURRENT_CAMERA" = "auto" ] && default=2
    printf '\n    This computer has an infrared camera (%s). Which camera should FaceAuth use?\n' "$IR_CAMERA"
    printf '      1) Infrared camera  works in the dark, harder to fool [recommended]\n'
    printf '         (if its light is off, the installer tries to switch it on by itself)\n'
    printf '      2) Normal camera    works right away, needs some light\n'
    case "$(ask "    Choose [$default]: " "$default")" in
        2|auto|normal|colour) CAMERA_CHOICE=auto ;;
        *) CAMERA_CHOICE=ir ;;
    esac
fi
case "$CAMERA_CHOICE" in
    "") ;;
    auto|ir) ok "Camera: $([ "$CAMERA_CHOICE" = ir ] && echo infrared || echo colour)" ;;
    *) die "Unknown --camera '$CAMERA_CHOICE' (use auto or ir)." ;;
esac

info "Asking for your password once so later steps can use sudo..."
sudo -v || die "sudo authentication failed."

# ── Dependencies ─────────────────────────────────────────────────────────────

distro_family() {
    local id
    # shellcheck disable=SC2086  # ID_LIKE is a space-separated list
    for id in $OS_ID $OS_LIKE; do
        case "$id" in
            ubuntu|debian|pop|linuxmint|elementary|zorin|neon) echo debian; return ;;
            fedora|rhel|centos|rocky|almalinux|nobara) echo fedora; return ;;
            arch|manjaro|endeavouros|garuda|cachyos) echo arch; return ;;
            opensuse*|suse|sles) echo suse; return ;;
        esac
    done
    echo unknown
}

# pkg_run CMD... — run a package-manager command, waiting (up to 10 minutes)
# while another one holds its lock: Ubuntu's automatic updates, the Software
# app, or an update running in another terminal.
pkg_run() {
    local err waited=0
    err=$(mktemp)
    while :; do
        if "$@" 2>"$err"; then
            cat "$err" >&2
            rm -f "$err"
            return 0
        fi
        if ! grep -Eqi "could not get lock|unable to lock|is held by process|lock database|system management is locked" "$err"; then
            cat "$err" >&2
            rm -f "$err"
            return 1
        fi
        if [ "$waited" -ge 600 ]; then
            rm -f "$err"
            die "Another program kept the package manager busy for 10 minutes. Try again when it has finished."
        fi
        [ "$waited" -eq 0 ] && info "Waiting for another software update to finish (Ubuntu Software or automatic updates)..."
        sleep 5
        waited=$((waited + 5))
    done
}

install_deps() {
    local family gui_pkgs=() pkgs=() missing=()
    family=$(distro_family)
    case "$family" in
        debian)
            [ "$WANT_GUI" -eq 1 ] && gui_pkgs=(libgtk-4-dev libadwaita-1-dev)
            pkgs=(build-essential pkg-config libpam0g-dev clang libclang-dev curl git ca-certificates unzip "${gui_pkgs[@]}")
            for p in "${pkgs[@]}"; do
                dpkg-query -W -f='${Status}' "$p" 2>/dev/null | grep -q "install ok installed" || missing+=("$p")
            done
            # Nothing to do on a re-run, so don't touch apt at all.
            [ "${#missing[@]}" -eq 0 ] && return 0
            info "Installing: ${missing[*]}"
            pkg_run sudo apt-get update -qq
            pkg_run sudo apt-get install -y -q "${missing[@]}"
            ;;
        fedora)
            [ "$WANT_GUI" -eq 1 ] && gui_pkgs=(gtk4-devel libadwaita-devel)
            pkgs=(gcc gcc-c++ pkg-config pam-devel clang clang-devel curl git unzip "${gui_pkgs[@]}")
            rpm -q "${pkgs[@]}" >/dev/null 2>&1 && return 0
            pkg_run sudo dnf install -y -q "${pkgs[@]}"
            ;;
        arch)
            [ "$WANT_GUI" -eq 1 ] && gui_pkgs=(gtk4 libadwaita)
            pkgs=(base-devel pam clang curl git unzip "${gui_pkgs[@]}")
            pacman -T "${pkgs[@]}" >/dev/null 2>&1 && return 0
            pkg_run sudo pacman -S --needed --noconfirm "${pkgs[@]}"
            ;;
        suse)
            [ "$WANT_GUI" -eq 1 ] && gui_pkgs=(gtk4-devel libadwaita-devel)
            pkgs=(gcc gcc-c++ pkg-config pam-devel clang curl git unzip "${gui_pkgs[@]}")
            rpm -q "${pkgs[@]}" >/dev/null 2>&1 && return 0
            pkg_run sudo zypper --non-interactive install "${pkgs[@]}"
            ;;
        *)
            warn "Unsupported distribution: $OS_ID. Install these yourself:"
            info "  C/C++ toolchain, pkg-config, PAM headers, clang + libclang, curl, git, unzip"
            info "  and for the app: GTK 4.12+ and libadwaita 1.4+ development packages"
            if [ "$HAVE_TTY" -eq 1 ] && [ "$ASSUME_YES" -eq 0 ]; then
                ask "Press Enter when they are installed (Ctrl+C to stop)... " "" >/dev/null
            fi
            ;;
    esac
}

step "Installing build dependencies"
install_deps
ok "Dependencies ready"

BUILD_GUI=0
if [ "$WANT_GUI" -eq 1 ]; then
    if pkg-config --atleast-version=4.12 gtk4 2>/dev/null \
        && pkg-config --atleast-version=1.4 libadwaita-1 2>/dev/null; then
        BUILD_GUI=1
    else
        warn "The Face Unlock app needs GTK 4.12+ and libadwaita 1.4+, which this system doesn't have."
        info "Skipping the app. You can set up face unlock in the terminal with: faceauth enroll"
    fi
fi

# ── Rust ─────────────────────────────────────────────────────────────────────

step "Checking Rust"
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
    info "Installing Rust with rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --quiet --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi
CARGO_VERSION=$(cargo --version | awk '{print $2}')
if [ "$(printf '%s\n%s\n' "$MIN_CARGO" "$CARGO_VERSION" | sort -V | head -n1)" != "$MIN_CARGO" ]; then
    if command -v rustup >/dev/null 2>&1; then
        info "Rust $CARGO_VERSION is too old; updating..."
        rustup update stable
        CARGO_VERSION=$(cargo --version | awk '{print $2}')
    else
        die "Rust $MIN_CARGO or newer is required (found $CARGO_VERSION)."
    fi
fi
ok "Rust $CARGO_VERSION"

# ── Source ───────────────────────────────────────────────────────────────────

step "Getting the source"
SCRIPT_PATH="${BASH_SOURCE[0]:-}"
SRC_DIR=""
LOCAL_CHECKOUT=0
if [ -n "$SCRIPT_PATH" ] && [ -f "$SCRIPT_PATH" ]; then
    candidate=$(cd "$(dirname "$SCRIPT_PATH")" && pwd)
    if [ -f "$candidate/Cargo.toml" ] && [ -d "$candidate/faceauthd" ]; then
        SRC_DIR="$candidate"
        LOCAL_CHECKOUT=1
    fi
fi

if [ "$LOCAL_CHECKOUT" -eq 1 ]; then
    ok "Using the checkout in $SRC_DIR"
else
    if [ -d "$HOME/.faceauth-install/faceauthd" ]; then
        info "Removing the old build directory ~/.faceauth-install (no longer used)."
        rm -rf "$HOME/.faceauth-install"
    fi
    WORK_DIR=$(mktemp -d)
    trap 'rm -rf "$WORK_DIR"' EXIT
    git clone --quiet --depth 1 --branch "$FACEAUTH_REF" "$REPO_URL" "$WORK_DIR/faceauth" \
        || die "Could not download FaceAuth $FACEAUTH_REF from $REPO_URL"
    SRC_DIR="$WORK_DIR/faceauth"
    ok "Downloaded FaceAuth $FACEAUTH_REF"
fi
cd "$SRC_DIR"

VERSION=$(awk -F'"' '/^\[workspace.package\]/{s=1} s && /^version/{print $2; exit}' Cargo.toml)
VERSION="${VERSION:-$FACEAUTH_REF}"

# ── Build ────────────────────────────────────────────────────────────────────

step "Building FaceAuth $VERSION (this takes a few minutes the first time)"
CARGO_ARGS=(build --release --workspace)
[ "$LOCAL_CHECKOUT" -eq 0 ] && CARGO_ARGS+=(--locked)
[ "$BUILD_GUI" -eq 0 ] && CARGO_ARGS+=(--exclude faceauth-gui)
BUILD_LOG=$(mktemp -t faceauth-build.XXXXXX)
if ! cargo "${CARGO_ARGS[@]}" >"$BUILD_LOG" 2>&1; then
    tail -n 60 "$BUILD_LOG" >&2
    die "Build failed. Full log: $BUILD_LOG"
fi
rm -f "$BUILD_LOG"
for bin in faceauthd faceauth libpam_faceauth.so; do
    [ -f "target/release/$bin" ] || die "Build finished but target/release/$bin is missing."
done
[ "$BUILD_GUI" -eq 1 ] && [ ! -f target/release/faceauth-gui ] && die "Build finished but faceauth-gui is missing."
ok "Build complete"

# ── Install files ────────────────────────────────────────────────────────────

# Copy to a temporary name and rename, so running processes that have the old
# file mapped (sudo, the login screen) never see a half-written library.
install_atomic() { # install_atomic MODE SRC DEST
    local tmp
    tmp="$3.faceauth-new.$$"
    sudo install -D -m "$1" "$2" "$tmp"
    sudo mv -f "$tmp" "$3"
}

step "Installing"
if systemctl is-active --quiet faceauth.service 2>/dev/null; then
    sudo systemctl stop faceauth.service
    info "Stopped the running FaceAuth service"
fi
# Older versions could install a per-user service; remove it.
systemctl --user disable --now faceauth.service >/dev/null 2>&1 || true
rm -f "$HOME/.config/systemd/user/faceauth.service"
systemctl --user daemon-reload >/dev/null 2>&1 || true
# Older versions used a socket in /tmp.
sudo find /tmp -maxdepth 1 -name faceauth.sock -type s -user root -delete 2>/dev/null || true

if ! getent group video >/dev/null; then
    sudo groupadd --system video
fi
if ! getent passwd "$DAEMON_USER" >/dev/null; then
    sudo useradd --system --user-group --home-dir "$STATE_DIR" \
        --shell "$(command -v nologin || echo /bin/false)" \
        --comment "FaceAuth daemon" "$DAEMON_USER"
    ok "Created system user '$DAEMON_USER'"
fi
sudo usermod -aG video "$DAEMON_USER"

install_atomic 755 target/release/faceauthd "$PREFIX/bin/faceauthd"
install_atomic 755 target/release/faceauth "$PREFIX/bin/faceauth"
ok "Installed faceauthd and faceauth to $PREFIX/bin"

PAM_UNIX=$(find /usr/lib64 /usr/lib /lib64 /lib -maxdepth 3 -name pam_unix.so -path '*/security/*' 2>/dev/null | head -n 1 || true)
[ -n "$PAM_UNIX" ] || die "Could not find the PAM module directory (pam_unix.so not found)."
PAM_DIR=$(dirname "$PAM_UNIX")
install_atomic 644 target/release/libpam_faceauth.so "$PAM_DIR/pam_faceauth.so"
ok "Installed the PAM module to $PAM_DIR"

sudo install -D -m 644 dist/faceauth.service /etc/systemd/system/faceauth.service
sudo install -D -m 644 dist/org.faceauth.manage.policy /usr/share/polkit-1/actions/org.faceauth.manage.policy
sudo install -D -m 755 uninstall.sh "$PREFIX/share/faceauth/uninstall.sh"
ok "Installed the service, polkit policy and uninstaller"

if [ "$BUILD_GUI" -eq 1 ]; then
    install_atomic 755 target/release/faceauth-gui "$PREFIX/bin/faceauth-gui"
    sudo install -D -m 644 assets/org.faceauth.gui.svg /usr/share/icons/hicolor/scalable/apps/org.faceauth.gui.svg
    sudo install -D -m 644 assets/org.faceauth.gui.desktop /usr/share/applications/org.faceauth.gui.desktop
    sudo gtk-update-icon-cache -qtf /usr/share/icons/hicolor 2>/dev/null || true
    sudo update-desktop-database -q /usr/share/applications 2>/dev/null || true
    ok "Installed the Face Unlock app"
else
    # An app left over from an older install would speak the old protocol.
    sudo rm -f "$PREFIX/bin/faceauth-gui" /usr/share/applications/org.faceauth.gui.desktop
fi

# GNOME: the animated face on the lock screen and for sudo/admin prompts. It
# only listens to faceauthd and can't approve or block anything.
SHELL_EXT_UUID="faceauth@ibrahimgulbutt.github.io"
SHELL_EXT_DIR="/usr/share/gnome-shell/extensions/$SHELL_EXT_UUID"
FACE_ANIMATION=0
GNOME_MAJOR=$(gnome-shell --version 2>/dev/null | awk '{print int($3)}')

# with_extension LIST — LIST (a GVariant string array) with the extension added.
with_extension() {
    case "$1" in
        *"'$SHELL_EXT_UUID'"*) printf '%s' "$1" ;;
        "@as []"|"[]"|"") printf "['%s']" "$SHELL_EXT_UUID" ;;
        *) printf "%s, '%s']" "${1%]}" "$SHELL_EXT_UUID" ;;
    esac
}

if [ "$WANT_ANIMATION" -eq 1 ] && [ "${GNOME_MAJOR:-0}" -ge 45 ]; then
    sudo rm -rf "$SHELL_EXT_DIR"
    sudo install -D -m 644 -t "$SHELL_EXT_DIR" \
        "dist/gnome-shell/$SHELL_EXT_UUID/metadata.json" \
        "dist/gnome-shell/$SHELL_EXT_UUID/extension.js" \
        "dist/gnome-shell/$SHELL_EXT_UUID/stylesheet.css"
    FACE_ANIMATION=1
    current=$(gsettings get org.gnome.shell enabled-extensions 2>/dev/null || true)
    if [ -n "$current" ] && gsettings set org.gnome.shell enabled-extensions "$(with_extension "$current")" 2>/dev/null; then
        ok "Installed the face animation for GNOME (lock screen, sudo and admin prompts)"
    else
        ok "Installed the face animation for GNOME"
        info "Turn it on after logging back in: gnome-extensions enable $SHELL_EXT_UUID"
    fi
    if [ "$(gsettings get org.gnome.shell disable-user-extensions 2>/dev/null)" = "true" ]; then
        warn "GNOME extensions are switched off in your settings, so the face animation won't show."
        info "Switch them on in the Extensions app."
    fi
elif [ "$WANT_ANIMATION" -eq 1 ] && [ -n "$GNOME_MAJOR" ] && [ "$GNOME_MAJOR" -gt 0 ]; then
    info "Face animation skipped: it needs GNOME 45 or newer (you have $GNOME_MAJOR)."
fi

# ── Models ───────────────────────────────────────────────────────────────────

step "Downloading face models from InsightFace"
info "The models come from InsightFace's official releases and are for non-commercial research use only."
sudo mkdir -p "$MODELS_DIR"
MODEL_STAGE=$(mktemp -d)
bash dist/fetch-models.sh "$MODEL_STAGE" "$MODEL_CHOICE" --installed "$MODELS_DIR" \
    || { rm -rf "$MODEL_STAGE"; die "Could not download the face models."; }
if compgen -G "$MODEL_STAGE/*.onnx" >/dev/null; then
    sudo install -m 644 "$MODEL_STAGE"/*.onnx "$MODELS_DIR"/
fi
rm -rf "$MODEL_STAGE"
for old in "${!OLD_MODEL_SHA256[@]}"; do
    if sudo test -f "$MODELS_DIR/$old" && echo "${OLD_MODEL_SHA256[$old]}  $MODELS_DIR/$old" | sudo sha256sum -c --status; then
        sudo rm -f "$MODELS_DIR/$old"
        info "Removed the old $old"
    fi
done
ok "Models ready in $MODELS_DIR"

# ── Configuration and data ───────────────────────────────────────────────────

step "Configuring"
sudo mkdir -p "$CONFIG_DIR"
if [ -f "$CONFIG_DIR/config.toml" ]; then
    ok "Keeping your existing $CONFIG_DIR/config.toml"
    if grep -Eq '^[[:space:]]*confidence_threshold[[:space:]]*=[[:space:]]*0\.3([^0-9]|$)' "$CONFIG_DIR/config.toml"; then
        info "Note: detection.confidence_threshold = 0.3 is set there; 0.5 is now recommended."
    fi
else
    sudo install -m 644 dist/config.toml "$CONFIG_DIR/config.toml"
    ok "Created $CONFIG_DIR/config.toml"
fi

# set_config SECTION KEY VALUE — set a string value in config.toml. Replaces an
# existing line in place (keeping its comments), else adds it right under the
# section header, else adds the section. Two passes over a copy: the first only
# looks for the line.
set_config() {
    local cfg="$CONFIG_DIR/config.toml" orig tmp
    orig=$(mktemp) tmp=$(mktemp)
    sudo cat "$cfg" >"$orig"
    awk -v sec="$1" -v key="$2" -v line="$2 = \"$3\"" '
        function is_header(l) { return l ~ /^[[:space:]]*\[/ }
        function is_sec(l) { return l ~ ("^[[:space:]]*\\[" sec "\\][[:space:]]*(#.*)?$") }
        function is_key(l) { return l ~ ("^[[:space:]]*" key "[[:space:]]*=") }
        NR == FNR { if (is_header($0)) in_sec = is_sec($0); else if (in_sec && is_key($0)) has = 1; next }
        FNR == 1 { in_sec = 0 }
        is_header($0) { in_sec = is_sec($0) }
        in_sec && is_key($0) { if (!done) { print line; done = 1 }; next }
        { print }
        in_sec && is_sec($0) && !has && !done { print line; done = 1 }
        END { if (!done) { print ""; print "[" sec "]"; print line } }
    ' "$orig" "$orig" >"$tmp"
    sudo install -m 644 "$tmp" "$cfg"
    rm -f "$orig" "$tmp"
}
set_config recognition model "$MODEL_CHOICE"
ok "Recognizer set to '$MODEL_CHOICE' (changing it later means setting up face unlock again)"
if [ -n "$CAMERA_CHOICE" ] && [ "$CAMERA_CHOICE" != "${CURRENT_CAMERA:-auto}" ]; then
    set_config camera device "$CAMERA_CHOICE"
    ok "Camera set to '$CAMERA_CHOICE' (face data made with the other camera has to be set up again)"
fi

# set_ir_emitter TOML — replace any [camera.ir_emitter] / [[camera.ir_emitter]]
# blocks with TOML (as printed by `faceauth ir-emitter --toml`).
IR_MARKER="# Infrared light settings, added by the FaceAuth installer."
IR_MARKER_OLD="# IR light commands imported from linux-enable-ir-emitter by the installer."
set_ir_emitter() {
    local cfg="$CONFIG_DIR/config.toml" tmp
    tmp=$(mktemp)
    sudo cat "$cfg" | awk -v marker="$IR_MARKER" -v old_marker="$IR_MARKER_OLD" '
        $0 == marker || $0 == old_marker { next }
        /^[[:space:]]*\[/ { skip = ($0 ~ /^[[:space:]]*\[\[?[[:space:]]*camera\.ir_emitter[[:space:]]*\]/) }
        skip { next }
        /^[[:space:]]*$/ { blanks++; next }
        { while (blanks > 0) { print ""; blanks-- }; print }
    ' >"$tmp"
    printf '\n%s\n%s\n' "$IR_MARKER" "$1" >>"$tmp"
    sudo install -m 644 "$tmp" "$cfg"
    rm -f "$tmp"
}

# Infrared camera: switch its light on with the commands linux-enable-ir-emitter
# found (it saves them in the user's home, which is why this runs as the user).
if [ "${CAMERA_CHOICE:-${CURRENT_CAMERA:-auto}}" = "ir" ]; then
    if EMITTER_TOML=$("$PREFIX/bin/faceauth" ir-emitter --toml 2>/dev/null) && [ -n "$EMITTER_TOML" ]; then
        set_ir_emitter "$EMITTER_TOML"
        ok "IR light: using the settings saved by linux-enable-ir-emitter"
    elif sudo grep -Eq '^[[:space:]]*\[\[?[[:space:]]*camera\.ir_emitter' "$CONFIG_DIR/config.toml"; then
        ok "IR light: keeping the settings already in $CONFIG_DIR/config.toml"
    fi
fi

# Versions before 1.0 kept face data in root's home directory.
if sudo test -f "$OLD_STATE_DIR/master.key"; then
    if sudo test -f "$STATE_DIR/master.key"; then
        warn "Old face data is still in $OLD_STATE_DIR; not moved because $STATE_DIR already has data."
    else
        sudo mkdir -p "$STATE_DIR"
        sudo sh -c 'mv "$1"/* "$2"/' sh "$OLD_STATE_DIR" "$STATE_DIR"
        sudo rmdir "$OLD_STATE_DIR" 2>/dev/null || true
        ok "Moved your existing face data from $OLD_STATE_DIR to $STATE_DIR"
    fi
fi
if sudo test -d "$STATE_DIR"; then
    sudo chown -R "$DAEMON_USER:$DAEMON_USER" "$STATE_DIR"
    sudo chmod 700 "$STATE_DIR"
fi

# ── Service ──────────────────────────────────────────────────────────────────

step "Starting the FaceAuth service"
sudo systemctl daemon-reload
sudo systemctl enable --quiet faceauth.service
sudo systemctl restart faceauth.service

SERVICE_OK=0
for _ in $(seq 1 15); do
    if env -u FACEAUTH_SOCKET "$PREFIX/bin/faceauth" ping >/dev/null 2>&1; then
        SERVICE_OK=1
        break
    fi
    sleep 1
done
if [ "$SERVICE_OK" -eq 1 ]; then
    ok "Service is running"
else
    warn "The service did not answer within 15 seconds. Recent log:"
    sudo journalctl -u faceauth.service -n 20 --no-pager >&2 || true
    info "Check it later with: faceauth doctor"
fi

# ── Infrared light ───────────────────────────────────────────────────────────
#
# An IR camera only sees in the dark when its own infrared light (emitter) is
# on, and on many laptops Linux doesn't switch it on. `faceauth ir-setup`
# finds the vendor setting that does, by itself: it tries the camera's
# settings one at a time, judges each by whether the face lights up, and puts
# each one back. If nothing works, the colour camera is used instead, so face
# unlock keeps working.

# Face brightness (0–255) seen by the camera in use, or nothing if no face.
ir_face_brightness() {
    env -u FACEAUTH_SOCKET "$PREFIX/bin/faceauth" benchmark 2>/dev/null \
        | sed -n 's/^Face[[:space:]]*brightness \([0-9][0-9]*\).*/\1/p'
}

restart_service() {
    sudo systemctl restart faceauth.service
    for _ in $(seq 1 15); do
        env -u FACEAUTH_SOCKET "$PREFIX/bin/faceauth" ping >/dev/null 2>&1 && return 0
        sleep 1
    done
    return 1
}

use_colour_camera() {
    CAMERA_CHOICE=auto
    set_config camera device auto
    restart_service || true
    ok "Using the normal camera instead"
}

has_emitter_config() {
    sudo grep -Eq '^[[:space:]]*\[\[?[[:space:]]*camera\.ir_emitter' "$CONFIG_DIR/config.toml"
}

# setup_ir_light — find and save the setting that switches the IR light on.
# Succeeds only if the light then works.
setup_ir_light() {
    local toml rc=0 level
    toml=$(sudo env -u FACEAUTH_SOCKET "$PREFIX/bin/faceauth" ir-setup --toml) || rc=$?
    [ "$rc" -eq 0 ] || return 1
    if [ -n "$toml" ]; then
        set_ir_emitter "$toml"
        restart_service || true
    fi
    sleep 1
    level=$(ir_face_brightness)
    if [ -n "$level" ] && [ "$level" -ge 40 ]; then
        ok "The infrared light works (face brightness $level)"
        return 0
    fi
    warn "The light still looks off (face brightness ${level:-none})."
    return 1
}

if [ "$SERVICE_OK" -eq 1 ] && [ "${CAMERA_CHOICE:-${CURRENT_CAMERA:-auto}}" = "ir" ]; then
    step "Setting up the infrared camera"
    if [ "$HAVE_TTY" -eq 1 ] && [ "$ASSUME_YES" -eq 0 ]; then
        ask "    Look at the camera, then press Enter: " "" >/dev/null
    fi
    IR_LEVEL=$(ir_face_brightness)
    if has_emitter_config && [ -n "$IR_LEVEL" ] && [ "$IR_LEVEL" -ge 40 ]; then
        ok "The infrared light works (face brightness $IR_LEVEL)"
    elif ! setup_ir_light; then
        info "This camera's infrared light couldn't be switched on, so it can't see in the dark."
        use_colour_camera
    fi
fi

# ── Face data ────────────────────────────────────────────────────────────────
# Face data made with the other camera (or an older version) can't be used.
# Offer to set it up again right away instead of leaving face unlock paused.

NEEDS_SETUP=0
if [ "$SERVICE_OK" -eq 1 ] && env -u FACEAUTH_SOCKET "$PREFIX/bin/faceauth" status 2>/dev/null | grep -q "paused"; then
    step "Setting up your face again"
    info "Your face data was made with the other camera (or an older version), so face"
    info "unlock is paused until you set it up again."
    if [ "$HAVE_TTY" -eq 1 ] && [ "$ASSUME_YES" -eq 0 ]; then
        case "$(ask '    Set it up now? It takes about 30 seconds. [Y/n] ' y)" in
            [Nn]*) NEEDS_SETUP=1 ;;
            *) env -u FACEAUTH_SOCKET "$PREFIX/bin/faceauth" enroll </dev/tty || NEEDS_SETUP=1 ;;
        esac
    else
        NEEDS_SETUP=1
    fi
fi

# ── PAM ──────────────────────────────────────────────────────────────────────

LOGIN_SERVICES=(gdm-password sddm lightdm)
LOCK_SERVICES=(kde swaylock hyprlock i3lock xscreensaver cinnamon-screensaver mate-screensaver xfce4-screensaver)

pam_available() { # pam_available SERVICE — true if a PAM file exists for it
    [ -f "/etc/pam.d/$1" ] || [ -f "/usr/lib/pam.d/$1" ]
}

available_in() { # available_in SERVICE... — prints the ones that exist
    local svc
    for svc in "$@"; do
        if pam_available "$svc"; then printf '%s ' "$svc"; fi
    done
}

record_pam() { # record_pam KIND PATH — once per path ("created" also covers edits)
    sudo touch "$PAM_RECORD"
    if ! sudo grep -qxF "edited $2" "$PAM_RECORD" && ! sudo grep -qxF "created $2" "$PAM_RECORD"; then
        printf '%s %s\n' "$1" "$2" | sudo tee -a "$PAM_RECORD" >/dev/null
    fi
}

FACEAUTH_AUTH_RE='^[[:space:]]*-?auth[[:space:]].*pam_faceauth\.so'

# pam_anchor_line FILE — line number pam_faceauth.so belongs in front of: just
# before password authentication is pulled in, so checks that come first
# (pam_nologin, "user != root" rules) still run. Empty if FILE has no auth
# section. FILE must not contain a pam_faceauth.so line.
pam_anchor_line() {
    local file=$1 n=""
    n=$(sudo grep -nE '^[[:space:]]*@include[[:space:]]+common-auth' "$file" | head -n1 | cut -d: -f1 || true)
    [ -z "$n" ] && n=$(sudo grep -nE '^[[:space:]]*-?auth[[:space:]]+(include|substack)[[:space:]]' "$file" | head -n1 | cut -d: -f1 || true)
    [ -z "$n" ] && n=$(sudo grep -nE '^[[:space:]]*-?auth[[:space:]].*pam_unix\.so' "$file" | head -n1 | cut -d: -f1 || true)
    [ -z "$n" ] && n=$(sudo grep -nE '^[[:space:]]*-?auth[[:space:]]' "$file" | head -n1 | cut -d: -f1 || true)
    printf '%s' "$n"
}

# relocate_pam FILE — versions before 1.0 inserted pam_faceauth.so at line 1,
# above pam_nologin and "user != root" checks, so a face match skipped them.
# Move such a line to where pam_anchor_line puts new ones. A line already at
# or after that spot is left alone.
relocate_pam() {
    local file=$1 fa anchor line below stripped merged
    [ -f "$file" ] || return 0
    fa=$(sudo grep -nE "$FACEAUTH_AUTH_RE" "$file" | head -n1 | cut -d: -f1 || true)
    [ -n "$fa" ] || return 0

    stripped=$(mktemp)
    sudo grep -vE "$FACEAUTH_AUTH_RE" "$file" >"$stripped" || true
    anchor=$(pam_anchor_line "$stripped")
    # Lines 1..fa-1 of FILE are lines 1..fa-1 of $stripped, so FaceAuth sits in
    # front of stripped line $fa. It is misplaced if real directives (not
    # comments or blank lines) lie between it and the anchor.
    if [ -z "$anchor" ] || [ "$fa" -ge "$anchor" ] \
        || ! sed -n "${fa},$((anchor - 1))p" "$stripped" | grep -qvE '^[[:space:]]*(#|$)'; then
        rm -f "$stripped"
        return 0
    fi

    line=$(sudo sed -n "${fa}p" "$file")
    below=$(sed -n "1,$((anchor - 1))p" "$stripped" | grep -vE '^[[:space:]]*(#|$)' | tail -n1 \
        | grep -oE '[^[:space:]]+\.so' | head -n1 || true)
    merged=$(mktemp)
    awk -v n="$anchor" -v l="$line" 'NR == n { print l } { print }' "$stripped" >"$merged"

    sudo cp -p "$file" "$file.faceauth-move-backup"
    # Same owner and mode as the original, then an atomic rename.
    sudo cp -p "$file" "$file.faceauth-new"
    sudo cp "$merged" "$file.faceauth-new"
    sudo mv -f "$file.faceauth-new" "$file"
    rm -f "$stripped" "$merged"
    record_pam edited "$file"
    ok "moved FaceAuth below ${below:-the earlier checks} in $file"
}

configure_pam() { # configure_pam SERVICE
    local svc=$1
    local file="/etc/pam.d/$svc" vendor="/usr/lib/pam.d/$svc" kind="edited" line_no=""

    if [ ! -f "$file" ]; then
        if [ -f "$vendor" ]; then
            sudo cp "$vendor" "$file"
            kind="created"
        else
            warn "$svc: no PAM configuration found, skipping"
            return
        fi
    fi
    if sudo grep -qE "$FACEAUTH_AUTH_RE" "$file"; then
        ok "$svc: already enabled"
        relocate_pam "$file"
        return
    fi
    [ "$kind" = "edited" ] && sudo cp -p "$file" "$file.faceauth-backup"

    line_no=$(pam_anchor_line "$file")

    if [ -n "$line_no" ]; then
        sudo sed -i "${line_no}i $PAM_LINE" "$file"
    else
        printf '%s\n' "$PAM_LINE" | sudo tee -a "$file" >/dev/null
        warn "$svc: no auth section found; added the line at the end of $file. Please check it."
    fi
    record_pam "$kind" "$file"
    ok "$svc: enabled"
}

LOGIN_AVAIL=$(available_in "${LOGIN_SERVICES[@]}")
LOCK_AVAIL=$(available_in "${LOCK_SERVICES[@]}")
SUDO_AVAIL=$(available_in sudo)
POLKIT_AVAIL=$(available_in polkit-1)

step "Choosing where face unlock works"
if [ -z "$PAM_CHOICE" ]; then
    if [ "$ASSUME_YES" -eq 1 ] || [ "$HAVE_TTY" -eq 0 ]; then
        PAM_CHOICE="sudo"
    else
        describe() { if [ -n "$2" ]; then printf '%s %s(%s)%s' "$1" "$DIM" "${2% }" "$NC"; else printf '%s %s(not found on this system)%s' "$1" "$DIM" "$NC"; fi; }
        info "Your password keeps working everywhere; face unlock is tried first."
        info ""
        info "1) $(describe "sudo in the terminal" "$SUDO_AVAIL")  — recommended"
        info "2) $(describe "Login screen" "$LOGIN_AVAIL")"
        info "3) $(describe "Lock screen" "$LOCK_AVAIL")"
        info "   GNOME's lock screen uses gdm-password, so choose 2 for it."
        info "4) $(describe "Admin password prompts" "$POLKIT_AVAIL")"
        info "0) Nowhere for now"
        info ""
        selection=$(ask "    Choose one or more, separated by commas [1]: " "1")
        PAM_CHOICE=""
        IFS=', ' read -r -a picks <<<"$selection"
        for pick in "${picks[@]}"; do
            case "$pick" in
                1) PAM_CHOICE+="sudo," ;;
                2) PAM_CHOICE+="login," ;;
                3) PAM_CHOICE+="lock," ;;
                4) PAM_CHOICE+="polkit," ;;
                0) PAM_CHOICE="none"; break ;;
                "") ;;
                *) warn "Ignoring unknown choice '$pick'" ;;
            esac
        done
        PAM_CHOICE="${PAM_CHOICE%,}"
        [ -z "$PAM_CHOICE" ] && PAM_CHOICE="none"
    fi
fi

IFS=',' read -r -a categories <<<"$PAM_CHOICE"
for category in "${categories[@]}"; do
    case "$category" in
        sudo)   services="$SUDO_AVAIL" ;;
        login)  services="$LOGIN_AVAIL" ;;
        lock)   services="$LOCK_AVAIL" ;;
        polkit) services="$POLKIT_AVAIL" ;;
        none|"") continue ;;
        *) warn "Unknown --pam value '$category' (use sudo, login, lock, polkit or none)"; continue ;;
    esac
    if [ -z "$services" ]; then
        warn "$category: nothing to configure on this system"
        continue
    fi
    # shellcheck disable=SC2086  # space-separated list of service names
    for svc in $services; do
        configure_pam "$svc"
    done
done

# ── Summary ──────────────────────────────────────────────────────────────────

# Fix the placement of lines added by older installers or by hand, including
# services the user didn't pick this time.
for f in /etc/pam.d/*; do
    case "$f" in *.faceauth-*) continue ;; esac
    relocate_pam "$f"
done

ENABLED=""
for f in /etc/pam.d/*; do
    case "$f" in *.faceauth-*) continue ;; esac
    if [ -f "$f" ] && sudo grep -qE "$FACEAUTH_AUTH_RE" "$f"; then
        ENABLED+="$(basename "$f") "
    fi
done

case "$INSTALL_KIND" in
    upgrade|downgrade) DONE_MSG="FaceAuth ${INSTALLED_VERSION/old/(before 1.0)} → $VERSION: done." ;;
    repair) DONE_MSG="FaceAuth $VERSION is repaired and up to date." ;;
    *) DONE_MSG="FaceAuth $VERSION is installed." ;;
esac
printf '\n%s%s%s\n\n' "$GREEN$BOLD" "$DONE_MSG" "$NC"
if [ -n "$ENABLED" ]; then
    info "Face unlock is enabled for: ${ENABLED% }"
else
    info "Face unlock isn't enabled anywhere yet. Re-run with --pam=sudo (or login, lock, polkit)."
fi
info ""
info "${BOLD}Next:${NC}"
# Only suggest setting up when the face isn't set up (or is paused).
if env -u FACEAUTH_SOCKET "$PREFIX/bin/faceauth" status 2>/dev/null | grep -q "is set up for"; then
    info "  Your face is set up. Try it:  sudo echo 'Hello from FaceAuth'"
elif [ "$BUILD_GUI" -eq 1 ]; then
    info "  1. Open ${BOLD}Face Unlock${NC} from your app menu and choose Set Up"
    info "     (or run: faceauth enroll)"
    info "  2. Try it:             sudo echo 'Hello from FaceAuth'"
else
    info "  1. Set up your face:   faceauth enroll"
    info "  2. Try it:             sudo echo 'Hello from FaceAuth'"
fi
info ""
if [ "$FACE_ANIMATION" -eq 1 ]; then
    info "Face animation:    log out and back in once to see it (lock screen, sudo)"
fi
info "Check your setup:  faceauth doctor"
info "Uninstall:         $PREFIX/share/faceauth/uninstall.sh"
info ""
