//! Which PAM services use FaceAuth (a read-only view of /etc/pam.d, shared by
//! the CLI and the GUI).

use std::path::{Path, PathBuf};

/// Service files (name, has an active `pam_faceauth.so` auth line).
pub fn configured_services() -> Vec<String> {
    let mut found = Vec::new();
    for dir in ["/etc/pam.d", "/usr/lib/pam.d"] {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // /etc/pam.d overrides the vendor directory; skip installer backups.
            if name.contains(".faceauth-") || name.ends_with('~') || found.contains(&name) {
                continue;
            }
            if dir == "/usr/lib/pam.d" && Path::new("/etc/pam.d").join(&name).exists() {
                continue;
            }
            if uses_faceauth(&entry.path()) {
                found.push(name);
            }
        }
    }
    found.sort();
    found
}

fn uses_faceauth(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines().any(|l| {
                let l = l.trim_start();
                !l.starts_with('#') && l.contains("pam_faceauth.so")
            })
        })
        .unwrap_or(false)
}

pub fn label(service: &str) -> &str {
    match service {
        "sudo" | "sudo-i" => "sudo",
        "su" | "su-l" => "su",
        "gdm-password" => "login & lock screen",
        "sddm" | "lightdm" => "login screen",
        "kde" | "swaylock" | "hyprlock" | "i3lock" | "xscreensaver" | "cinnamon-screensaver" | "mate-screensaver"
        | "xfce4-screensaver" => "lock screen",
        "polkit-1" => "admin prompts",
        "login" => "console login",
        "common-auth" | "system-auth" => "everything using common auth",
        other => other,
    }
}

pub fn labels(services: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for s in services {
        let l = label(s);
        if !out.contains(&l) {
            out.push(l);
        }
    }
    out.join(", ")
}

/// Where pam_faceauth.so is installed (the directory holding pam_unix.so).
pub fn module_path() -> Option<PathBuf> {
    for root in ["/usr/lib64/security", "/usr/lib/security", "/lib/security", "/lib64/security"] {
        let p = Path::new(root).join("pam_faceauth.so");
        if p.exists() {
            return Some(p);
        }
    }
    let multiarch = std::fs::read_dir("/usr/lib").ok()?;
    multiarch
        .flatten()
        .map(|e| e.path().join("security/pam_faceauth.so"))
        .find(|p| p.exists())
}
