<h1 align="center">FaceAuth</h1>

<p align="center">
  <strong>Windows Hello for Linux</strong><br>
  Face authentication for <code>sudo</code>, GDM, SDDM, and LightDM. Works with any webcam.
</p>

<p align="center">
  <a href="#-quick-install">Install</a> •
  <a href="#-features">Features</a> •
  <a href="#-how-it-works">How It Works</a> •
  <a href="#-troubleshooting">Troubleshooting</a> •
  <a href="ARCHITECTURE.md">Architecture</a>
</p>

---

## ⚡ Quick Install

### One-Line Installation (Recommended)
```bash
curl -fsSL https://raw.githubusercontent.com/ibrahimgulbutt/faceauth/main/install.sh | bash
```

The installer will:
- ✅ Install system dependencies (GTK4, PAM, clang, etc.)
- ✅ Install Rust toolchain if not present
- ✅ Build and install all binaries from source
- ✅ Download AI models (~20 MB)
- ✅ Create and start the system service
- ✅ Configure PAM integration (your choice of scope)

**Total time: ~5 minutes** (mostly compilation)

---

### Manual Installation

If you prefer to review the script before running it:
```bash
# Download and inspect
wget https://raw.githubusercontent.com/ibrahimgulbutt/faceauth/main/install.sh
cat install.sh

# Run
bash install.sh
```

Or build completely from source:
```bash
git clone https://github.com/ibrahimgulbutt/faceauth.git
cd faceauth
cargo build --release
# Then follow the installer steps manually
```

---

## 📸 Getting Started

### Step 1: Log Out and Back In

The installer adds you to the `video` group. That change only takes effect after a new login session.

### Step 2: Enroll Your Face

Launch the enrollment GUI:
```bash
faceauth-gui
```

The GUI collects **10 confirmed face samples** across different angles so the model learns your face robustly. You will see a guide oval — keep your face inside it and follow the on-screen angle prompts.

**Enrollment buttons:**

| Button | What it does |
|--------|-------------|
| **Enroll My Face** (first time) | Captures 10 samples and saves your face data |
| **Re-enroll (Replace)** | Deletes existing data and re-captures 10 fresh samples |
| **Add More Angles** | Appends 10 more samples to your existing enrollment (improves accuracy) |
| **Delete Enrollment** | Removes all your face data |

**Takes ~1 minute** the first time.

### Step 3: Test It
```bash
sudo echo "Hello FaceAuth!"
```

The camera activates, recognises you, and unlocks — no password needed. ✨

---

## ✨ Features

### 🚀 Fast
- **~1 second** end-to-end authentication on typical hardware
- Camera warms up in the background while PAM starts
- Much faster than Howdy (2–3 s)

### 🔒 Secure
- **No photos stored** — only 512-number ArcFace embeddings
- **AES-256-GCM** encrypted face data at rest
- **Liveness detection** — rejects printed photos and replays
- **Rate limiting** with configurable lockout after failed attempts
- **Audit logging** for every authentication event

### 🐧 Native Linux
- Works with **any V4L2 webcam** — no proprietary drivers
- Supports **Wayland and X11**
- PAM module integrates with **sudo, GDM, SDDM, LightDM**
- Tested on **Ubuntu 22.04+, Fedora 38+, Arch Linux, Debian 12**

### 🎨 User-Friendly
- **GTK4/Adwaita GUI** for enrollment with live camera preview
- **Automatic fallback** to password if face auth fails or is unavailable
- **Never locks you out** — password always works as a fallback

---

## 🎯 How It Works

```
You run: sudo <command>
         ↓
PAM calls pam_faceauth.so
         ↓
Daemon opens camera → warms up (3 frames discarded)
         ↓
ScrFD detects face in frame 0 (~360 ms)
         ↓
ArcFace embeds 5 crops and averages them (~187 ms)
         ↓
Cosine similarity vs stored embeddings
         ↓
Pass (≥ 0.55) → unlocked  |  Fail → password prompt
```

**Technology stack:**
| Component | Implementation |
|-----------|---------------|
| Detection | ScrFD 500M (INT8 quantized) |
| Recognition | ArcFace / MobileFaceNet (INT8 quantized) |
| Runtime | ONNX Runtime |
| Language | Rust — memory-safe, no GC pauses |
| IPC | Unix socket (`/tmp/faceauth.sock`) |

[📖 Read the detailed architecture](ARCHITECTURE.md)

---

## 📊 Performance

**Measured on Intel i5 laptop, USB 2.0 webcam:**

| Stage | Time |
|-------|------|
| Camera open + warmup | ~950 ms (hardware) |
| Face detection (ScrFD) | ~360 ms |
| Face recognition ×5 (ArcFace) | ~187 ms |
| **Total (typical)** | **~1.1 s** |

**Comparison:**

| System | Time | Hardware Required |
|--------|------|------------------|
| **FaceAuth** | **~1.1 s** | Any webcam |
| Windows Hello (RGB) | 1.2–1.8 s | Any webcam |
| Windows Hello (IR) | 0.4–0.6 s | IR camera required |
| Howdy | 2.0–3.5 s | Any webcam |

---

## 🛠️ Commands

```bash
# Check daemon and system health
faceauth doctor

# List enrolled users
faceauth list

# Re-enroll or manage your face data
faceauth-gui

# Run a recognition benchmark
faceauth benchmark

# Daemon control
sudo systemctl status faceauth.service
sudo systemctl restart faceauth.service

# Live logs
sudo journalctl -u faceauth.service -f
```

---

## 🔧 Configuration

Config file: `/etc/faceauth/config.toml`

```toml
[detection]
confidence_threshold = 0.3   # Detection sensitivity (lower = finds more faces)
min_face_size = 80            # Minimum face size in pixels

[recognition]
match_threshold = 0.55        # Cosine similarity required to pass (0.5–0.65 range)
strong_match_threshold = 0.65 # Threshold for a high-confidence match
weak_match_threshold = 0.45   # Threshold for a weak/tentative match

[camera]
warmup_frames = 3             # Frames to discard on open (reduce motion blur)
sequence_length = 5           # Number of frames to embed and average
sequence_interval_ms = 40     # Delay between captured frames

[security]
require_liveness = true       # Reject photo attacks
max_attempts = 3              # Failed attempts before lockout
lockout_seconds = 60          # Lockout duration
```

After editing, apply changes with:
```bash
sudo systemctl restart faceauth.service
```

---

## ❓ Troubleshooting

### Camera not detected
```bash
ls /dev/video*         # Check devices exist
mpv /dev/video0        # Test camera works
sudo journalctl -u faceauth.service -f  # Check daemon errors
```

### Authentication always fails
```bash
# 1. Run the system health check
faceauth doctor

# 2. Check recent daemon activity
sudo journalctl -u faceauth.service --since "10 minutes ago"

# 3. Re-enroll with more angles
faceauth-gui   # Use "Re-enroll" or "Add More Angles"
```

If logs show a score of `0.35–0.48` consistently, lower `match_threshold` to `0.45` in the config, or re-enroll using more varied angles.

### Daemon won't start
```bash
sudo systemctl status faceauth.service   # Check for errors
sudo journalctl -u faceauth.service -f   # View full output
```

Common causes: ONNX model files missing from `/usr/share/faceauth/models/`, or a previous user-level service conflict. Run `faceauth doctor` for a full check.

### Still stuck?
1. Run `faceauth doctor` and copy the output
2. Open an issue: [github.com/ibrahimgulbutt/faceauth/issues](https://github.com/ibrahimgulbutt/faceauth/issues)
3. Include: OS + version, webcam model, daemon logs

---

## 🔓 Uninstalling

```bash
# Stop and disable the service
sudo systemctl stop faceauth.service
sudo systemctl disable faceauth.service

# Remove binaries
sudo rm -f /usr/local/bin/faceauthd /usr/local/bin/faceauth /usr/local/bin/faceauth-gui

# Remove PAM module
sudo rm -f /lib/x86_64-linux-gnu/security/pam_faceauth.so \
           /usr/lib/security/pam_faceauth.so \
           /lib/security/pam_faceauth.so

# Remove service and config
sudo rm -f /etc/systemd/system/faceauth.service
sudo rm -rf /etc/faceauth /usr/share/faceauth

# Remove from PAM (edit manually and delete the pam_faceauth.so line)
sudo nano /etc/pam.d/sudo

sudo systemctl daemon-reload
```

---

## 🤝 Contributing

Contributions welcome! See [CONTRIBUTING.md](CONTRIBUTING.md).

**Ways to help:**
- 🐛 Report bugs with logs and system info
- 💡 Suggest features via GitHub Issues
- 📖 Improve documentation
- 🔧 Submit pull requests (please read CONTRIBUTING.md first)
- ⭐ Star the project

---

## 🔐 Security

- **No photos or video stored** — only 512-dimensional cosine vectors
- **AES-256-GCM** encrypted at rest with a per-user key
- **Open source** — read and audit every line
- **No telemetry, no cloud** — fully offline operation

Found a security issue? Please open a [private security advisory](https://github.com/ibrahimgulbutt/faceauth/security/advisories/new) on GitHub.

---

## 📄 License

MIT License — see [LICENSE](LICENSE).

---

## 🙏 Acknowledgments

- [SCRFD](https://github.com/deepinsight/insightface/tree/master/detection/scrfd) — face detection model
- [ArcFace](https://github.com/deepinsight/insightface/tree/master/recognition/arcface_torch) — face recognition model
- Inspired by Windows Hello and [Howdy](https://github.com/boltgolt/howdy)

---

<p align="center">
  Made with ❤️ for the Linux community
</p>

<p align="center">
  <a href="https://github.com/ibrahimgulbutt/faceauth">GitHub</a> •
  <a href="https://github.com/ibrahimgulbutt/faceauth/issues">Issues</a> •
  <a href="CONTRIBUTING.md">Contributing</a>
</p>
