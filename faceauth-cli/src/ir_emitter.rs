//! Reading linux-enable-ir-emitter's saved configuration, so FaceAuth can send
//! the same "switch the IR light on" commands itself.
//!
//! linux-enable-ir-emitter (v7) finds the vendor command interactively and
//! saves it to `~/.config/linux-enable-ir-emitter.toml`:
//!
//! ```toml
//! [["usb-04f2:b71c-01.00.00-if02-index0".savelist]]
//! unit = 7
//! selector = 6
//! control = [1, 3, 3, 0, 0, 0, 0, 0, 0]
//! ```
//!
//! Devices are keyed by a USB identifier (vendor:product, serial, interface,
//! node index, all from sysfs) or by a device path.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const SYSFS_V4L: &str = "/sys/class/video4linux";

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub unit: u8,
    pub selector: u8,
    pub control: Vec<u8>,
}

#[derive(Deserialize, Default)]
struct ToolDevice {
    #[serde(default)]
    savelist: Vec<Control>,
}

/// What was found for one camera.
pub struct Found {
    /// `/dev/videoN` the entry belongs to.
    pub node: PathBuf,
    pub infrared: bool,
    pub controls: Vec<Control>,
}

/// `~/.config/linux-enable-ir-emitter.toml` of the current user.
pub fn tool_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".config/linux-enable-ir-emitter.toml"))
}

/// The saved commands for a camera connected now (an IR camera first), or
/// None when the file has none.
pub fn find(file: &Path) -> Result<Option<Found>> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let devices: BTreeMap<String, ToolDevice> =
        toml::from_str(&text).with_context(|| format!("{} is not a linux-enable-ir-emitter configuration", file.display()))?;
    let nodes = video_nodes();
    let mut found: Vec<Found> = devices
        .into_iter()
        .filter(|(_, d)| !d.savelist.is_empty())
        .filter_map(|(key, d)| {
            let node = nodes.iter().find(|n| n.matches(&key))?;
            Some(Found { node: node.path.clone(), infrared: node.infrared, controls: d.savelist })
        })
        .collect();
    found.sort_by_key(|f| !f.infrared);
    Ok(found.into_iter().next())
}

/// FaceAuth config lines for `controls`.
pub fn to_toml(controls: &[Control]) -> String {
    let mut out = String::new();
    for c in controls {
        let bytes: Vec<String> = c.control.iter().map(|b| b.to_string()).collect();
        let _ = write!(out, "[[camera.ir_emitter]]\nunit = {}\nselector = {}\ncontrol = [{}]\n", c.unit, c.selector, bytes.join(", "));
    }
    out
}

/// Number of emitter controls in FaceAuth's own config, if it can be read.
pub fn configured_in(config: &Path) -> Option<usize> {
    #[derive(Deserialize, Default)]
    struct Cfg {
        #[serde(default)]
        camera: Camera,
    }
    #[derive(Deserialize, Default)]
    struct Camera {
        #[serde(default)]
        ir_emitter: Option<toml::Value>,
    }
    let cfg: Cfg = toml::from_str(&std::fs::read_to_string(config).ok()?).ok()?;
    Some(match cfg.camera.ir_emitter {
        None => 0,
        Some(toml::Value::Array(a)) => a.len(),
        Some(_) => 1,
    })
}

/// Whether any connected camera looks infrared (by its UVC interface or
/// device name).
pub fn has_ir_camera() -> bool {
    video_nodes().iter().any(|n| n.infrared)
}

struct Node {
    path: PathBuf,
    usb_id: Option<String>,
    infrared: bool,
}

impl Node {
    fn matches(&self, key: &str) -> bool {
        if key.starts_with("usb-") {
            return self.usb_id.as_deref() == Some(key);
        }
        match (std::fs::canonicalize(key), std::fs::canonicalize(&self.path)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }
}

fn video_nodes() -> Vec<Node> {
    let Ok(entries) = std::fs::read_dir(SYSFS_V4L) else { return Vec::new() };
    let mut nodes: Vec<Node> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.strip_prefix("video")?.parse::<u32>().ok()?;
            let dir = e.path();
            let read = |p: PathBuf| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let label = format!(
                "{} {}",
                read(dir.join("device/interface")).unwrap_or_default(),
                read(dir.join("name")).unwrap_or_default()
            );
            Some(Node { path: Path::new("/dev").join(&name), usb_id: usb_id(&dir), infrared: looks_infrared(&label) })
        })
        .collect();
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    nodes
}

/// Same identifier as linux-enable-ir-emitter:
/// `usb-<idVendor>:<idProduct>[-<serial>]-if<bInterfaceNumber>-index<index>`.
fn usb_id(node_dir: &Path) -> Option<String> {
    let read = |p: PathBuf| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let index = read(node_dir.join("index"))?;
    let iface_dir = std::fs::canonicalize(node_dir.join("device")).ok()?;
    let iface = read(iface_dir.join("bInterfaceNumber"))?;
    let usb = iface_dir.parent()?;
    let mut id = format!("usb-{}:{}", read(usb.join("idVendor"))?, read(usb.join("idProduct"))?);
    if let Some(serial) = read(usb.join("serial")) {
        id.push('-');
        id.push_str(&serial);
    }
    Some(format!("{id}-if{iface}-index{index}"))
}

fn looks_infrared(label: &str) -> bool {
    let lower = label.to_ascii_lowercase();
    lower.contains("infrared") || lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "ir")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_tool_format() {
        let text = r#"
[["usb-04f2:b71c-01.00.00-if02-index0".savelist]]
unit = 7
selector = 6
control = [1, 3, 3, 0, 0, 0, 0, 0, 0]

[["usb-04f2:b71c-01.00.00-if02-index0".savelist]]
unit = 4
selector = 2
control = [0]

[["/dev/video9".blacklist]]
unit = 3
selector = 1
"#;
        let devices: BTreeMap<String, ToolDevice> = toml::from_str(text).unwrap();
        let ir = &devices["usb-04f2:b71c-01.00.00-if02-index0"];
        assert_eq!(ir.savelist.len(), 2);
        assert_eq!(ir.savelist[0], Control { unit: 7, selector: 6, control: vec![1, 3, 3, 0, 0, 0, 0, 0, 0] });
        assert!(devices["/dev/video9"].savelist.is_empty());
    }

    #[test]
    fn writes_faceauth_config() {
        let toml = to_toml(&[Control { unit: 7, selector: 6, control: vec![1, 3] }]);
        assert_eq!(toml, "[[camera.ir_emitter]]\nunit = 7\nselector = 6\ncontrol = [1, 3]\n");
    }

    #[test]
    fn spots_ir_names() {
        assert!(looks_infrared("Integrated IR Camera Integrated RGB Camera: Integrat"));
        assert!(looks_infrared("HD Infrared Camera"));
        assert!(!looks_infrared("Integrated Camera Integrated RGB Camera: Integrat"));
        assert!(!looks_infrared("Mirror Cam"));
    }
}
