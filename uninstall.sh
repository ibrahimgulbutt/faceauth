#!/usr/bin/env bash
# FaceAuth uninstaller
#
#   /usr/local/share/faceauth/uninstall.sh      # installed copy
#   ./uninstall.sh                              # from a checkout
#   ./uninstall.sh --purge                      # also delete face data and config
#
# Safe to run more than once.

set -euo pipefail

PREFIX="/usr/local"
CONFIG_DIR="/etc/faceauth"
STATE_DIR="/var/lib/faceauth"
OLD_STATE_DIR="/root/.local/share/faceauth"
PAM_RECORD="$CONFIG_DIR/pam-services"
DAEMON_USER="faceauth"

if [ -t 1 ]; then
    BOLD=$'\033[1m'; RED=$'\033[0;31m'; GREEN=$'\033[0;32m'; YELLOW=$'\033[0;33m'; BLUE=$'\033[0;34m'; NC=$'\033[0m'
else
    BOLD=""; RED=""; GREEN=""; YELLOW=""; BLUE=""; NC=""
fi
step() { printf '\n%s==>%s %s%s%s\n' "$BLUE" "$NC" "$BOLD" "$*" "$NC"; }
info() { printf '    %s\n' "$*"; }
ok()   { printf '    %s✓%s %s\n' "$GREEN" "$NC" "$*"; }
warn() { printf '    %s!%s %s\n' "$YELLOW" "$NC" "$*" >&2; }
die()  { printf '\n%sError:%s %s\n' "$RED" "$NC" "$*" >&2; exit 1; }

usage() {
    cat <<EOF
FaceAuth uninstaller

Usage: uninstall.sh [options]

Options:
  --purge      Also delete enrolled face data, configuration and the
               '$DAEMON_USER' system user, without asking.
  --yes, -y    Don't ask questions; keep face data and configuration.
  -h, --help   Show this help.
EOF
}

PURGE=0
ASSUME_YES=0
for arg in "$@"; do
    case "$arg" in
        --purge)  PURGE=1 ;;
        --yes|-y) ASSUME_YES=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "Unknown option: $arg" ;;
    esac
done

HAVE_TTY=0
if { : </dev/tty; } 2>/dev/null; then
    HAVE_TTY=1
fi

command -v sudo >/dev/null 2>&1 || [ "${EUID:-$(id -u)}" -eq 0 ] || die "sudo is required."
if [ "${EUID:-$(id -u)}" -eq 0 ]; then
    sudo() { "$@"; }
fi

printf '\n%sFaceAuth uninstaller%s\n' "$BOLD" "$NC"
sudo -v 2>/dev/null || true

# ── Service ──────────────────────────────────────────────────────────────────

step "Stopping the service"
if systemctl list-unit-files faceauth.service >/dev/null 2>&1 \
    && systemctl cat faceauth.service >/dev/null 2>&1; then
    sudo systemctl disable --now faceauth.service >/dev/null 2>&1 || true
    ok "Stopped and disabled faceauth.service"
else
    info "Service not installed"
fi

# ── PAM ──────────────────────────────────────────────────────────────────────

step "Removing face unlock from PAM"

remove_pam_line() { # remove_pam_line FILE — strips pam_faceauth.so lines, with a backup
    local file=$1
    [ -f "$file" ] || return 0
    if sudo grep -q 'pam_faceauth\.so' "$file"; then
        sudo cp -p "$file" "$file.faceauth-uninstall-backup"
        sudo sed -i '/pam_faceauth\.so/d' "$file"
        ok "$(basename "$file"): removed"
    fi
}

if sudo test -f "$PAM_RECORD"; then
    while read -r kind path; do
        [ -n "${path:-}" ] || continue
        case "$kind" in
            edited)
                remove_pam_line "$path"
                ;;
            created)
                remove_pam_line "$path"
                vendor="/usr/lib/pam.d/$(basename "$path")"
                if [ -f "$path" ] && [ -f "$vendor" ] && sudo cmp -s "$path" "$vendor"; then
                    sudo rm -f "$path" "$path.faceauth-uninstall-backup"
                    ok "$(basename "$path"): removed the copy made at install time"
                fi
                ;;
        esac
    done < <(sudo cat "$PAM_RECORD")
    sudo rm -f "$PAM_RECORD"
fi

# Catch lines added by hand or by older installers.
for file in /etc/pam.d/*; do
    case "$file" in *.faceauth-*) continue ;; esac  # our own backups
    remove_pam_line "$file"
done
ok "PAM configuration clean"

# ── Files ────────────────────────────────────────────────────────────────────

step "Removing files"
PAM_DIRS=(/usr/lib/x86_64-linux-gnu/security /lib/x86_64-linux-gnu/security
          /usr/lib/aarch64-linux-gnu/security /lib/aarch64-linux-gnu/security
          /usr/lib64/security /lib64/security /usr/lib/security /lib/security)
PAM_UNIX=$(find /usr/lib64 /usr/lib /lib64 /lib -maxdepth 3 -name pam_unix.so -path '*/security/*' 2>/dev/null | head -n 1 || true)
[ -n "$PAM_UNIX" ] && PAM_DIRS=("$(dirname "$PAM_UNIX")" "${PAM_DIRS[@]}")
for dir in "${PAM_DIRS[@]}"; do
    sudo rm -f "$dir/pam_faceauth.so"
done

sudo rm -f \
    "$PREFIX/bin/faceauthd" "$PREFIX/bin/faceauth" "$PREFIX/bin/faceauth-gui" \
    /etc/systemd/system/faceauth.service \
    /usr/share/polkit-1/actions/org.faceauth.manage.policy \
    /usr/share/applications/org.faceauth.gui.desktop \
    /usr/share/icons/hicolor/scalable/apps/org.faceauth.gui.svg
sudo rm -rf /usr/share/faceauth
# The GNOME face animation, and its entry in this user's enabled extensions.
SHELL_EXT_UUID="faceauth@ibrahimgulbutt.github.io"
sudo rm -rf "/usr/share/gnome-shell/extensions/$SHELL_EXT_UUID"
if [ "${EUID:-$(id -u)}" -ne 0 ] && command -v gsettings >/dev/null 2>&1; then
    current=$(gsettings get org.gnome.shell enabled-extensions 2>/dev/null || true)
    case "$current" in
        *"'$SHELL_EXT_UUID'"*)
            remaining=$(printf '%s' "$current" | sed -e "s/, '$SHELL_EXT_UUID'//; s/'$SHELL_EXT_UUID', //; s/'$SHELL_EXT_UUID'//")
            gsettings set org.gnome.shell enabled-extensions "$remaining" 2>/dev/null || true
            ;;
    esac
fi
sudo find /tmp -maxdepth 1 -name faceauth.sock -type s -user root -delete 2>/dev/null || true
sudo gtk-update-icon-cache -qtf /usr/share/icons/hicolor 2>/dev/null || true
sudo update-desktop-database -q /usr/share/applications 2>/dev/null || true
sudo systemctl daemon-reload 2>/dev/null || true
sudo systemctl reset-failed faceauth.service 2>/dev/null || true
ok "Removed programs, PAM module, service, polkit policy, app entry, face animation and models"

# ── Face data and configuration ──────────────────────────────────────────────

step "Face data and configuration"
DELETE_DATA=0
if [ "$PURGE" -eq 1 ]; then
    DELETE_DATA=1
elif [ "$ASSUME_YES" -eq 0 ] && [ "$HAVE_TTY" -eq 1 ]; then
    read -r -p "    Delete enrolled face data and configuration? [y/N] " reply </dev/tty || reply=""
    [[ "${reply:-}" =~ ^[Yy] ]] && DELETE_DATA=1
fi

if [ "$DELETE_DATA" -eq 1 ]; then
    sudo rm -rf "$STATE_DIR" "$OLD_STATE_DIR" "$CONFIG_DIR"
    if getent passwd "$DAEMON_USER" >/dev/null; then
        sudo userdel "$DAEMON_USER" 2>/dev/null || warn "Could not delete the '$DAEMON_USER' user"
    fi
    ok "Deleted face data, configuration and the '$DAEMON_USER' user"
else
    info "Kept $STATE_DIR (face data) and $CONFIG_DIR (configuration)."
    info "Delete them later with: sudo rm -rf $STATE_DIR $CONFIG_DIR"
fi

# This script may be the installed copy; remove it last.
sudo rm -rf "$PREFIX/share/faceauth"

printf '\n%sFaceAuth has been removed.%s Your password works exactly as before.\n\n' "$GREEN$BOLD" "$NC"
