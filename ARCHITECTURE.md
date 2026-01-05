# FaceAuth System Architecture

## 1. High-Level Logical Architecture

FaceAuth is designed as a modular, secure, and non-intrusive authentication system. It separates the privileged authentication logic (PAM) from the complex computer vision processing (Daemon).

```mermaid
graph TD
    User[User / Login Screen] -->|Triggers| PAM[PAM Module (pam_faceauth.so)]
    PAM -->|IPC (Unix Socket)| Daemon[Daemon Service (faceauthd)]
    
    subgraph "Secure Context (faceauthd)"
        Daemon -->|Capture| Camera[Camera Device]
        Daemon -->|Inference| AI[AI Engine (ONNX Runtime)]
        AI -->|Detect| DetModel[SCRFD Detection Model]
        AI -->|Recognize| RecModel[ArcFace Recognition Model]
        Daemon -->|Read/Decrypt| Storage[Encrypted Storage (AES-256)]
    end
    
    Daemon -->|Auth Result| PAM
    PAM -->|Allow/Deny| System[Linux Auth Stack]
```

### Components

1.  **`pam_faceauth.so` (The Gatekeeper)**
    *   **Role**: Integrates with the Linux PAM stack (`/etc/pam.d/sudo`, `/etc/pam.d/gdm-password`).
    *   **Logic**: It is a lightweight C-compatible shared library. It does *not* process images. It simply asks the daemon "Is the user here?" and waits for a Yes/No response.
    *   **Security**: Runs with the privileges of the calling process (e.g., root for sudo, gdm for login).

2.  **`faceauthd` (The Brain)**
    *   **Role**: The central background service that handles hardware and AI.
    *   **Logic**:
        *   Manages the camera (exclusive access).
        *   Loads the AI models into memory.
        *   Handles the secure storage decryption.
        *   Performs the actual face matching.
    *   **Security**: Runs as a systemd service (currently user-level, moving to system-level for login support). It isolates the complex/risky parsing of images and AI models from the critical PAM process.

3.  **`faceauth-gui` (The Enrollment Tool)**
    *   **Role**: User-facing application to register faces.
    *   **Logic**: Provides a live video feed and visual feedback to help users capture high-quality reference images.

4.  **`faceauth-cli` (The Admin Tool)**
    *   **Role**: Diagnostics and management.
    *   **Logic**: Used for `ping`, `doctor` (diagnostics), and listing enrolled users.

---

## 2. Project File Structure

The project is organized as a Rust Workspace, containing multiple crates (packages) that share dependencies.

```text
FaceAuth/
├── Cargo.toml                  # Workspace configuration
├── install.sh                  # Installation & Setup script
├── faceauth.service            # Systemd service definition
├── README.md                   # Quick start guide
├── TECHNICAL_DETAILS.md        # Deep dive into algorithms
├── ARCHITECTURE.md             # This file
│
├── faceauth-core/              # [Shared Library]
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs              # Shared types (AuthRequest, AuthResponse) and constants
│
├── faceauthd/                  # [Daemon Service]
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             # Entry point, IPC loop
│       ├── camera.rs           # Camera management (nokhwa)
│       ├── detection.rs        # Face Detection logic (SCRFD)
│       ├── recognition.rs      # Face Recognition logic (ArcFace)
│       └── storage.rs          # Encrypted file handling (AES-GCM)
│
├── pam_faceauth/               # [PAM Module]
│   ├── Cargo.toml              # Configured as "cdylib" (C-dynamic library)
│   └── src/
│       └── lib.rs              # Implements pam_sm_authenticate
│
├── faceauth-gui/               # [GUI Application]
│   ├── Cargo.toml
│   ├── models/                 # Local models for development
│   └── src/
│       ├── main.rs             # GTK4 UI setup
│       └── detection.rs        # Local detection for UI feedback
│
├── faceauth-cli/               # [CLI Tool]
│   ├── Cargo.toml
│   └── src/
│       └── main.rs             # Command line argument parsing
│
└── models/                     # [AI Models]
    ├── arcface.onnx            # Face Recognition Model
    └── det_500m.onnx           # Face Detection Model
```

---

## 3. Data Flow & Logic

### Authentication Request
1.  **Trigger**: User types `sudo ls`.
2.  **PAM**: `pam_faceauth` initializes.
3.  **Connect**: PAM connects to `/tmp/faceauth.sock`.
4.  **Request**: Sends `AuthRequest::Authenticate { user: "ibrahim" }`.
5.  **Daemon Action**:
    *   Acquires Camera Lock & Starts Active Session.
    *   **Parallel Execution**:
        *   **Thread A**: Captures Frame 0 -> Sends to Detection -> Captures Frames 1-4.
        *   **Thread B**: Runs `det_500m.onnx` on Frame 0 immediately.
    *   **Detection**:
        *   *If no face*: Returns `AuthResponse::Failure`.
    *   **Crop**: Crops face from all frames (using Frame 0's box).
    *   **Embedding**: Runs `arcface.onnx` on cropped faces -> `Vec<f32>`.
    *   **Load**: Decrypts `~/.local/share/faceauth/ibrahim/models.enc`.
    *   **Compare**: Calculates Cosine Similarity against all stored embeddings.
6.  **Response**:
    *   *Match (>0.6)*: Sends `AuthResponse::Success`.
    *   *No Match*: Sends `AuthResponse::Failure`.
7.  **Result**: PAM returns `PAM_SUCCESS` or `PAM_AUTH_ERR` to sudo.

### Enrollment Request (GUI)
1.  **Trigger**: User opens `faceauth-gui`.
2.  **Preview**: GUI accesses camera directly (or via daemon in future) to show preview.
3.  **Capture**: GUI captures high-quality frames.
4.  **Send**: GUI sends `AuthRequest::EnrollSample` with image data to daemon.
5.  **Daemon Action**:
    *   Generates embedding from sample.
    *   Appends to user's encrypted profile.
    *   Saves updated `models.enc`.
6.  **Feedback**: Daemon confirms success.

---

## 4. Security Architecture

### Isolation
*   **Privilege Separation**: The complex parsing of images (which can be an attack vector) happens in the daemon, not in the root-privileged PAM process.
*   **Crash Safety**: If the daemon crashes, it does not crash the login screen (PAM simply falls back to password).

### Data Protection
*   **No Photos**: We do not store user photos. Only mathematical vectors.
*   **Encryption**: All vectors are stored using **AES-256-GCM**.
*   **Integrity**: The GCM mode ensures that if the storage file is tampered with, it will fail to decrypt rather than loading bad data.

### Active Security Measures
*   **Liveness Detection**: The daemon analyzes the frame sequence for natural motion (micro-movements) to reject static photos.
*   **Rate Limiting**: After 3 failed attempts, the user is locked out of face authentication for 60 seconds to prevent brute-force attacks.
*   **Audit Logging**: All authentication attempts (success/fail) are logged to the system journal with scores and liveness results.
