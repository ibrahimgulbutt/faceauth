# FaceAuth Project Documentation

## Overview

**FaceAuth** is a modern, Rust-based face authentication system for Linux, designed as a secure and Wayland-compatible replacement for Howdy. It integrates with the Linux PAM (Pluggable Authentication Modules) system to allow users to log in, unlock screens, and authorize `sudo` commands using facial recognition.

## Architecture

The project is organized as a Rust workspace with the following components:

### 1. `faceauthd` (Daemon)
The core service that runs in the background (user-level systemd service).
- **Responsibilities**:
  - Manages the camera device (Video4Linux2 or Libcamera).
  - Loads AI models for Face Detection (RetinaFace/SCRFD) and Recognition (ArcFace).
  - Handles secure storage of face embeddings.
  - Listens on a Unix socket (`/tmp/faceauth.sock`) for requests.
- **Key Modules**:
  - `camera.rs`: Handles frame capture.
  - `detection.rs`: Handles Face Detection (SCRFD).
  - `recognition.rs`: Wraps the ONNX Runtime for inference (ArcFace).
  - `storage.rs`: Manages encrypted storage of user profiles.

### 2. `pam_faceauth` (PAM Module)
A C-compatible shared library (`libpam_faceauth.so`) that integrates with the system's authentication stack.
- **Responsibilities**:
  - Intercepts authentication requests (e.g., from `sudo` or GDM).
  - Communicates with `faceauthd` via IPC to request face verification.
  - Returns `PAM_SUCCESS` or `PAM_AUTH_ERR` based on the daemon's response.

### 3. `faceauth-core` (Shared Library)
A common Rust crate used by the daemon, CLI, and GUI.
- **Responsibilities**:
  - Defines the IPC protocol (`AuthRequest`, `AuthResponse`).
  - Shared constants (e.g., socket paths).

### 4. `faceauth-cli` (Command Line Interface)
A terminal tool for managing the system.
- **Commands**:
  - `faceauth ping`: Check if the daemon is running.
  - `faceauth enroll --name <NAME>`: Enroll a new face model.
  - `faceauth list`: List enrolled models.
  - `faceauth doctor`: Run system diagnostics (check camera, socket, etc.).

### 5. `faceauth-gui` (Graphical Interface)
A GTK4/Libadwaita application for user-friendly enrollment.
- **Features**:
  - Live camera preview.
  - Visual feedback for face positioning.
  - Simple enrollment process.

## Project Status

### Implemented Features
- **Core Infrastructure**:
  - Rust workspace setup.
  - IPC communication (Unix Domain Sockets).
  - Systemd service integration.
- **Computer Vision**:
  - Camera access via `nokhwa`.
  - Face Detection & Recognition using ONNX models.
- **Security**:
  - Encrypted storage for face embeddings.
  - PAM integration for secure authentication.
- **User Experience**:
  - CLI tool for management and diagnostics.
  - GUI application for enrollment.
- **Performance (Phase 6 & 7A)**:
  - **Quantized Models**: INT8 models for faster inference.
  - **Adaptive Detection**: Smart skipping of redundant detection frames.
  - **Overlapped Processing**: Camera capture and AI detection run in parallel.
  - **Latency**: Reduced to <300ms per authentication.

### Pending / Future Work
- **Advanced Liveness**: Blink detection or depth analysis.
- **Intruder Snapshots**: Saving images of failed authentication attempts.

## Installation & Usage

### Prerequisites
- Rust (Cargo)
- `clang` and `libclang-dev` (for bindgen)
- `libgtk-4-dev`, `libadwaita-1-dev` (for GUI)
- `libpam0g-dev` (for PAM)

### Build
```bash
cargo build --release
```

### Installation
Run the installation script to set up the service and PAM configuration:
```bash
sudo ./install.sh
```

### Enrollment
You can enroll your face using the CLI or GUI:
```bash
# CLI
target/release/faceauth enroll --name "My Face"

# GUI
target/release/faceauth-gui
```

### Testing
To test the authentication without locking yourself out:
```bash
faceauth ping
# Use a test PAM service if available, or try sudo in a separate terminal
sudo -k && sudo echo "FaceAuth Test"
```
