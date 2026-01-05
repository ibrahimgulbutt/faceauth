# FaceAuth

**Secure, Modern Face Authentication for Linux**

FaceAuth brings Windows Hello™ style facial recognition to Linux, integrating seamlessly with `sudo`, GDM, and other PAM-enabled services. It is designed with a focus on security, performance, and Wayland compatibility.

## Key Features

*   **Secure Architecture**: Privileged PAM module is separated from the complex AI daemon.
*   **Privacy First**: Stores mathematical embeddings only, never photos.
*   **Anti-Spoofing**: Liveness detection prevents static photo attacks.
*   **Performance**: Optimized ONNX runtime with **sub-300ms** authentication (Phase 7A Overlapped Architecture).
*   **Security**: Rate limiting and audit logging to prevent brute-force attacks.
*   **Wayland Ready**: Uses `nokhwa` for modern camera access (V4L2/Libcamera).

## Quick Start

1.  **Install**:
    ```bash
    ./install.sh
    ```

2.  **Enroll**:
    Launch the GUI to register your face:
    ```bash
    faceauth-gui
    ```

3.  **Test**:
    Open a new terminal and try `sudo ls`. The camera should activate and authenticate you automatically.

## Documentation

*   **[Architecture & Explanation](ARCHITECTURE.md)**: Detailed breakdown of how the system works, security model, and component descriptions.
*   **[Technical Details](TECHNICAL_DETAILS.md)**: Deep dive into algorithms, models, and configuration.
*   **[Checkpoints](checkpoints/)**: Detailed progress logs for each phase of development.

## Project Structure

*   `faceauthd`: The background service handling camera, AI, and secure storage.
*   `faceauth-gui`: GTK4 user interface for enrollment.
*   `pam_faceauth`: PAM module for system integration.
*   `faceauth-cli`: Command-line tools for diagnostics and benchmarking.

## Troubleshooting

Run the built-in diagnostic tool:
```bash
faceauth doctor
```

To check performance:
```bash
faceauth benchmark
```
