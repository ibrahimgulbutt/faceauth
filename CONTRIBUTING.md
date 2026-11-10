# Contributing to FaceAuth

Thanks for helping. This guide covers the development setup, the checks to run
and what reviewers look for.

## Code of conduct

Be respectful and constructive. Harassment of any kind is not tolerated.

## Before you start

Read [docs/DESIGN.md](docs/DESIGN.md). It describes every flow, message and
trust boundary, and code that disagrees with it is a bug in one of the two.
FaceAuth sits in the login path, so changes to the daemon's caller checks, the
PAM module's trust in the daemon, polkit gating or the remote-session check
need an issue and discussion first.

## Prerequisites

- Rust 1.82 or newer (`rustup` recommended)
- A Linux system with systemd and a webcam
- System packages:

```bash
# Ubuntu / Debian
sudo apt-get install build-essential pkg-config libpam0g-dev clang libclang-dev \
    libgtk-4-dev libadwaita-1-dev

# Fedora
sudo dnf install gcc gcc-c++ pkg-config pam-devel clang clang-devel gtk4-devel libadwaita-devel

# Arch
sudo pacman -S base-devel pam clang gtk4 libadwaita
```

The app needs GTK 4.12+ and libadwaita 1.4+. On older systems, build without
it: `cargo build --workspace --exclude faceauth-gui`.

## Project structure

```
faceauth/
├── faceauth-core/      # Protocol types and framing shared by everything
├── faceauthd/          # Daemon: server, authorization, polkit, camera,
│                       # detection, recognition, enrollment, storage, config
├── pam_faceauth/       # PAM module (cdylib)
├── faceauth-cli/       # `faceauth` command
├── faceauth-gui/       # Face Unlock app (GTK 4 + libadwaita)
├── dist/               # systemd unit, polkit policy, default config
├── docs/DESIGN.md      # Flows, protocol, trust boundaries
├── assets/             # Icon and .desktop file
├── install.sh          # Installer
├── uninstall.sh        # Uninstaller
└── models/             # ONNX models for development (not in git)
```

## Running everything without root

The daemon, CLI and app can run as your own user against a private socket.
Such a daemon is never trusted by the PAM module, so this cannot affect real
logins.

1. **Get the models** into `./models/` (they are not in git). This downloads
   them from InsightFace's releases and verifies their checksums:

   ```bash
   bash dist/fetch-models.sh models accurate   # or: fast (~15 MB)
   ```

   Model speed on your machine:
   `FACEAUTH_MODELS_DIR=$PWD/models cargo test --release -p faceauthd bench_ -- --ignored --nocapture`

2. **Build:**

   ```bash
   cargo build
   ```

3. **Terminal 1, the daemon:**

   ```bash
   export FACEAUTH_SOCKET=$XDG_RUNTIME_DIR/faceauth-dev.sock \
          FACEAUTH_STATE_DIR=$PWD/target/dev/state \
          FACEAUTH_MODELS_DIR=$PWD/models \
          FACEAUTH_CONFIG=$PWD/dist/config.toml \
          FACEAUTH_DEV_SKIP_POLKIT=1
   RUST_LOG=debug target/debug/faceauthd
   ```

4. **Terminal 2, the app or the CLI:**

   ```bash
   export FACEAUTH_SOCKET=$XDG_RUNTIME_DIR/faceauth-dev.sock
   target/debug/faceauth-gui          # or: target/debug/faceauth enroll
   target/debug/faceauth test
   ```

| Variable | Read by | Effect |
|---|---|---|
| `FACEAUTH_SOCKET` | daemon, CLI, app | Socket path (the PAM module ignores it) |
| `FACEAUTH_STATE_DIR` | daemon | Where `master.key` and profiles live |
| `FACEAUTH_MODELS_DIR` | daemon | Where the `.onnx` models are |
| `FACEAUTH_CONFIG` | daemon | Config file to read |
| `FACEAUTH_DEV_SKIP_POLKIT=1` | daemon | Skip the password check. Only honoured when the daemon runs as a normal login user, never as root or `faceauth` |
| `FACEAUTH_GUI_DEMO=new\|ready\|outdated\|down` | app | Scripted replies and a synthetic camera; no daemon or camera needed |
| `FACEAUTH_GUI_START=intro\|capture\|improve\|done\|test\|remove` | app | Open that page at startup |
| `FACEAUTH_GUI_SNAPSHOT=file.png` | app | Save the window to a PNG every second (turns animations off) |

Unix socket paths are limited to 108 bytes, which is why the socket lives in
`$XDG_RUNTIME_DIR` rather than inside a deeply nested checkout.

**Working on the app without a camera.** `FACEAUTH_GUI_DEMO=ready target/debug/faceauth-gui`
runs every screen against scripted data with a synthetic silhouette instead of
a camera, so screenshots never contain anyone's face. Combine it with
`FACEAUTH_GUI_START` and `FACEAUTH_GUI_SNAPSHOT` to render a specific page, for
example off-screen with `gtk4-broadwayd :7` and `GDK_BACKEND=broadway BROADWAY_DISPLAY=:7`.

To try the PAM module for real, install with `./install.sh` from your checkout.
Keep a root shell open while you test PAM changes, in case something goes
wrong.

## Checks

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
bash -n install.sh uninstall.sh && shellcheck install.sh uninstall.sh
```

CI runs all of these. It builds without the app on Ubuntu 22.04 and with it on
Ubuntu 24.04.

## Reporting bugs

Search [existing issues](https://github.com/ibrahimgulbutt/faceauth/issues)
first, then include:

- your distribution and version, and your camera model;
- the output of `faceauth doctor`;
- daemon logs: `sudo journalctl -u faceauth.service --since "5 min ago"`.

Security problems go through a private advisory instead; see
[SECURITY.md](SECURITY.md).

## Pull requests

1. Branch from `main`, and keep each PR to one logical change.
2. Run the checks above, then test the real flow you touched (enrollment in the
   app or the CLI, `faceauth test`, `sudo`).
3. Use clear commit messages, for example:

   ```
   fix(daemon): reject profiles whose embeddings have the wrong length
   feat(gui): show the camera name on the home page
   docs: explain IR emitter setup
   ```

4. In the PR description, say what changed and why, and how you tested it.

**Checklist**

- [ ] `docs/DESIGN.md` is updated if a flow, message or file location changed.
- [ ] No trust boundary in DESIGN.md is weakened without prior discussion.
- [ ] New failure modes fall through to the password; they never succeed.
- [ ] User-facing text is short, plain and specific.
- [ ] `CHANGELOG.md` has an entry under the unreleased version.

## Good places to help

- **First contributions:** distribution support in `install.sh`, clearer error
  messages, tests for config parsing, pose estimation and alignment,
  translations.
- **Harder work:**
  - infrared camera testing on more laptops;
  - a passive anti-spoofing model;
  - packaging (AUR, `.deb`, `.rpm`);
  - performance profiling.

## Questions

Open a [discussion](https://github.com/ibrahimgulbutt/faceauth/discussions) or
an issue.
