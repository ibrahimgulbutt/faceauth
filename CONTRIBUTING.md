# Contributing to FaceAuth

Thanks for your interest in contributing! This guide covers everything you need to get started.

---

## Code of Conduct

Be respectful and constructive. Harassment of any kind will not be tolerated.

---

## Getting Started

### Prerequisites

- Rust 1.75+ (`rustup` recommended)
- A Linux system with a webcam
- System dependencies (GTK4, PAM, clang):

```bash
# Ubuntu / Debian
sudo apt-get install build-essential pkg-config libpam0g-dev libgtk-4-dev libadwaita-1-dev clang libclang-dev

# Fedora
sudo dnf install gcc gcc-c++ pkg-config pam-devel gtk4-devel libadwaita-devel clang

# Arch
sudo pacman -S base-devel pam gtk4 libadwaita clang
```

### Build from Source

```bash
git clone https://github.com/ibrahimgulbutt/faceauth.git
cd faceauth
cargo build --release
```

### Project Structure

```
faceauth/
├── faceauthd/          # Background daemon (detection + recognition + PAM socket)
│   └── src/
│       ├── camera.rs   # V4L2 camera capture
│       ├── detection.rs # ScrFD face detector
│       ├── recognition.rs # ArcFace embeddings
│       ├── liveness.rs  # Anti-spoofing
│       ├── storage.rs   # Encrypted embedding store
│       ├── security.rs  # Rate limiting
│       ├── config.rs    # Config file parsing
│       └── main.rs      # Tokio async main + request handler
├── faceauth-core/      # Shared types (AuthRequest/Response enums)
├── faceauth-gui/       # GTK4 enrollment GUI
├── faceauth-cli/       # CLI tool (faceauth doctor, list, etc.)
├── pam_faceauth/       # PAM shared library
└── models/             # ONNX model files (not tracked in git)
```

### Running Locally

```bash
# Build all workspace members
cargo build --release

# Install binaries for testing
sudo install -m755 target/release/faceauthd /usr/local/bin/
sudo install -m755 target/release/faceauth /usr/local/bin/
sudo install -m755 target/release/faceauth-gui /usr/local/bin/

# Ensure models are in place
sudo mkdir -p /usr/share/faceauth/models
# copy det_500m_int8.onnx and arcface_int8.onnx to the above directory

# Start the daemon (needs root for /tmp/faceauth.sock)
sudo faceauthd

# In another terminal
faceauth doctor
```

---

## How to Contribute

### Reporting Bugs

1. Search existing [issues](https://github.com/ibrahimgulbutt/faceauth/issues) first
2. Open a new issue with:
   - OS + version (e.g. Ubuntu 24.04)
   - Webcam model
   - Output of `faceauth doctor`
   - Relevant daemon logs: `sudo journalctl -u faceauth.service --since "5 min ago"`

### Suggesting Features

Open an issue with the `enhancement` label. Describe the use case, not just the solution.

### Submitting Pull Requests

1. **Fork** the repository and create a branch from `main`:
   ```bash
   git checkout -b feature/my-change
   ```

2. **Keep changes focused** — one logical change per PR.

3. **Test your changes**:
   ```bash
   cargo check --workspace
   cargo clippy --workspace -- -D warnings
   ```
   Then do a real authentication test to make sure nothing regressed.

4. **Commit clearly**:
   ```
   fix: correct ScrFD bbox decoder stride formula
   feat: add additive enrollment (Add More Angles)
   docs: update config reference in README
   ```

5. **Open the PR** against `main` with a description of what changed and why.

---

## What to Work On

Good first contributions:
- Improve error messages and docs
- Add support for more Linux distros in `install.sh`
- Add tests for config parsing or embedding similarity logic
- Improve the GUI (better camera preview, accessibility)

Harder tasks:
- IR camera support
- IR liveness detection (structured light / time-of-flight)
- Multi-camera support
- Performance profiling and optimisation

---

## Questions?

Open a [GitHub Discussion](https://github.com/ibrahimgulbutt/faceauth/discussions) or an issue.
