<p align="center">
  <img src="docs/assets/logo.svg" alt="FaceAuth Logo" width="120"/>
</p>

<h1 align="center">FaceAuth</h1>

<p align="center">
  <strong>Windows Hello for Linux</strong><br>
  Face authentication in ~1 second. Works with any webcam.
</p>

<p align="center">
  <img src="docs/assets/demo.gif" alt="FaceAuth Demo" width="600"/>
</p>

<p align="center">
  <a href="#-quick-install">Install</a> •
  <a href="#-features">Features</a> •
  <a href="#-how-it-works">How It Works</a> •
  <a href="#-troubleshooting">Troubleshooting</a> •
  <a href="ARCHITECTURE.md">Documentation</a>
</p>

---

## ⚡ Quick Install

### One-Line Installation (Recommended)
```bash
curl -fsSL https://raw.githubusercontent.com/ibrahimgulbutt/faceauth/main/install.sh | bash
```

That's it! The installer will:
- ✅ Check your system compatibility
- ✅ Install dependencies automatically
- ✅ Build and install FaceAuth
- ✅ Set up PAM integration
- ✅ Start the background service

**Total time: ~5 minutes**

---

### Manual Installation

If you prefer to review the installer first:
```bash
# 1. Download
wget https://raw.githubusercontent.com/ibrahimgulbutt/faceauth/main/install.sh

# 2. Review the script
cat install.sh

# 3. Run it
bash install.sh
```

---

## 📸 Getting Started

### Step 1: Enroll Your Face

After installation, run:
```bash
faceauth-gui
```

**What to do:**
1. Look at the camera
2. Follow on-screen instructions (5 angles)
3. Click "Enroll"

**Takes ~30 seconds**

### Step 2: Test It
```bash
sudo echo "Hello FaceAuth!"
```

The camera will activate and authenticate you automatically. No password needed! ✨

---

## ✨ Features

### 🚀 Fast
- **~1 second** authentication (comparable to Windows Hello)
- Faster than typing your password (avg: 2-3 seconds)
- Much faster than Howdy (2-3 seconds)

### 🔒 Secure
- **No photos stored** - only mathematical embeddings
- **AES-256-GCM encryption** for face data
- **Liveness detection** prevents photo attacks
- **Rate limiting** prevents brute-force attempts
- **Audit logging** tracks all authentication attempts

### 🐧 Native Linux
- Works with **any webcam** (no special hardware)
- Supports **Wayland and X11**
- Integrates with **sudo, GDM, SDDM, LightDM**
- Compatible with **Ubuntu, Fedora, Arch, Debian**

### 🎨 User-Friendly
- **GTK4 GUI** for easy enrollment
- **Automatic fallback** to password if face auth fails
- **Won't lock you out** - password always works
- **Clear error messages** and diagnostics

---

## 🎯 How It Works
1. You run sudo
2. Camera activates
3. AI detects your face (~360ms)
4. AI recognizes you (~187ms)
5. System unlocks (~1.1s total)


**Technology:**
- Face Detection: SCRFD (lightweight, accurate)
- Face Recognition: ArcFace + MobileFaceNet
- Runtime: ONNX with INT8 quantization
- Language: Rust (memory-safe, fast)

[📖 Read detailed architecture](ARCHITECTURE.md)

---

## 📊 Performance

**Real-World Benchmarks:**
- Average Authentication Time: ~1.1 seconds
- Best Case: ~1.0 second
- Worst Case: ~1.3 seconds

**Components:**
- Camera Init:    ~950ms  (hardware bottleneck)
- Face Detection: ~360ms  (overlapped with init)
- Recognition:    ~187ms  (parallel processing)

**How We Compare:**

| System | Time | Hardware Required |
|--------|------|------------------|
| **FaceAuth** | **~1.1s** | Any webcam |
| Windows Hello (RGB) | 1.2-1.8s | Any webcam |
| Windows Hello (IR) | 0.4-0.6s | IR camera |
| Face ID | 0.6-1.0s | TrueDepth sensor |
| Howdy | 2.0-3.5s | Any webcam |

---

## 🛠️ Commands
```bash
# Check if daemon is running
sudo systemctl status faceauth.service

# List enrolled faces
sudo faceauth list

# Remove a face model
sudo faceauth remove <username>

# View logs
sudo journalctl -u faceauth.service -f
```

---

## 🔧 Configuration

Config file: `/etc/faceauth/config.toml`
```toml
[recognition]
match_threshold = 0.45  # Lower = more lenient (0.4-0.5 recommended)

[security]
require_liveness = true  # Anti-spoofing (recommended)
max_attempts = 3         # Attempts before lockout

[camera]
warmup_frames = 2        # Balance speed vs stability
```

After changing config:
```bash
sudo systemctl restart faceauth.service
```

---

## ❓ Troubleshooting

### Camera not detected
```bash
# Check if camera exists
ls /dev/video*

# Test camera
mpv /dev/video0

# Daemon runs as root, so user permissions issues are rare
# But you can check if restricted:
ls -l /dev/video0
```

### Authentication fails
```bash
# Run diagnostics
sudo faceauth doctor

# Check logs
sudo journalctl -u faceauth.service --since "5 minutes ago"

# Re-enroll face
faceauth-gui
```

### Daemon won't start
```bash
# Check status
sudo systemctl status faceauth.service

# View errors
sudo journalctl -u faceauth.service -f

# Restart daemon
sudo systemctl restart faceauth.service
```

### Still having issues?
1. Run `faceauth doctor` and share output
2. Open an issue: [GitHub Issues](https://github.com/ibrahimgulbutt/faceauth/issues)
3. Include: OS version, camera model, error logs

---

## 🔓 Uninstalling
```bash
# Download uninstaller
curl -fsSL https://raw.githubusercontent.com/ibrahimgulbutt/faceauth/main/uninstall.sh | bash

# Or manually:
systemctl --user stop faceauth.service
sudo rm /usr/local/bin/faceauth*
sudo rm /lib/security/pam_faceauth.so
sudo rm /etc/pam.d/sudo  # Edit to remove faceauth line
```

---

## 🤝 Contributing

Contributions welcome! See [CONTRIBUTING.md](docs/CONTRIBUTING.md)

**Ways to help:**
- 🐛 Report bugs
- 💡 Suggest features  
- 📖 Improve documentation
- 🔧 Submit pull requests
- ⭐ Star the project

---

## 🔐 Security

- **No photos stored** - only 512-number mathematical vectors
- **Encrypted storage** - AES-256-GCM
- **Open source** - audit the code yourself
- **Privacy-first** - no telemetry, no cloud

[📖 Read security model](docs/SECURITY.md)

Found a security issue? Email: security@yourproject.com

---

## 📄 License

MIT License - see [LICENSE](LICENSE)

---

## 🙏 Acknowledgments

- [SCRFD](https://github.com/deepinsight/insightface/tree/master/detection/scrfd) - Face detection
- [ArcFace](https://github.com/deepinsight/insightface/tree/master/recognition/arcface_torch) - Face recognition
- Inspired by Windows Hello and Face ID

---

## ⭐ Star History

[![Star History Chart](https://api.star-history.com/svg?repos=ibrahimgulbutt/faceauth&type=Date)](https://star-history.com/#ibrahimgulbutt/faceauth&Date)

---

<p align="center">
  Made with ❤️ for the Linux community
</p>

<p align="center">
  <a href="https://github.com/ibrahimgulbutt/faceauth">GitHub</a> •
  <a href="https://github.com/ibrahimgulbutt/faceauth/issues">Issues</a> •
  <a href="docs/CONTRIBUTING.md">Contributing</a>
</p>
