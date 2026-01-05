# FaceAuth Technical Deep Dive

This document provides a comprehensive technical explanation of the algorithms, libraries, and procedures used in the FaceAuth system.

## 1. Core Technologies & Libraries

The system is built in **Rust** to ensure memory safety and high performance. The following key libraries (crates) are used:

*   **`ort` (ONNX Runtime)**: The core inference engine. It runs the pre-trained AI models. We use the `ndarray` feature to handle tensor inputs/outputs efficiently.
    *   *Update (Phase 6)*: Models are now quantized to **INT8** for 30-40% faster inference on CPUs.
*   **`nokhwa`**: A cross-platform camera library used to capture raw frames from Video4Linux2 (V4L2) or Libcamera devices.
*   **`image`**: Used for image manipulation (resizing, cropping, pixel access).
*   **`ndarray`**: Provides N-dimensional array structures (tensors) required by the ONNX models.
*   **`aes-gcm`**: Implements the AES-256-GCM authenticated encryption standard for securing user data.
*   **`tokio`**: An asynchronous runtime that allows the daemon to handle IPC requests, camera operations, and **parallel recognition tasks** concurrently.
*   **`serde` / `serde_json`**: Handles serialization of IPC messages and storage files.

---

## 2. Computer Vision Pipeline

The authentication process follows a strict pipeline: **Capture → Detect → Align → Recognize → Compare**.

### Step 1: Image Acquisition & Overlapped Processing (Phase 7A)
*   **Source**: The system connects to `/dev/video0` (or the first available camera).
*   **Active Session**: An `ActiveCamera` session is established to maintain the stream, reducing initialization overhead.
*   **Overlapped Strategy**:
    1.  **Frame 0 Capture**: The first frame is captured immediately after a short warmup (2 frames).
    2.  **Parallel Fork**:
        *   **Path A (Detection)**: Frame 0 is sent immediately to the AI thread for Face Detection.
        *   **Path B (Capture)**: The camera continues to capture Frames 1-4 concurrently.
    3.  **Synchronization**: The system awaits the completion of both paths. This effectively hides the detection latency (~120ms) behind the physical capture time of the remaining frames.
*   **Format**: Frames are captured in RGB format at 1280x720.

### Step 2: Face Detection (The "Finder")
Before recognizing *who* someone is, we must confirm *if* someone is there.
*   **Algorithm**: **SCRFD** (Sample and Computation Redistribution for Face Detection).
*   **Model File**: `det_500m_int8.onnx` (Quantized INT8 version).
*   **Input**: The camera frame is resized to **640x640** pixels.
*   **Normalization**: Pixel values are normalized to the range `[-1.0, 1.0]` using `(pixel - 127.5) / 128.0`.
*   **Adaptive Strategy (Phase 6 & 7A)**:
    *   **Frame 0**: Full detection runs on Frame 0 *while* Frames 1-4 are being captured.
    *   **Frames 1-4**: Detection is skipped. We use the cached bounding box from Frame 0 (center-cropped) to save ~400ms of processing time.
*   **Output**: The model outputs "score maps" and "bounding box maps".
*   **Logic**:
    1.  We scan the score map for values > **0.5** (50% confidence).
    2.  If no face is found, the process aborts immediately (Security feature: prevents background false positives).
    3.  If a face is found, we calculate the bounding box coordinates.

### Step 3: Preprocessing & Alignment
*   **Cropping**: The system crops the original image to the detected bounding box.
*   **Resizing**: The cropped face is resized to **112x112** pixels, which is the required input size for the recognition model.
*   **Normalization**: The 112x112 image is normalized again to `[-1.0, 1.0]`.

### Step 4: Feature Extraction (The "Recognizer")
This step converts a face image into a unique mathematical "fingerprint".
*   **Algorithm**: **ArcFace** (Additive Angular Margin Loss).
*   **Backbone Architecture**: **MobileFaceNet** (Optimized for mobile/CPU performance).
*   **Model File**: `arcface_int8.onnx` (Quantized INT8 version).
*   **Input**: A 112x112 RGB tensor `(1, 3, 112, 112)`.
*   **Output**: A **512-dimensional floating-point vector** (Embedding).
*   **Properties**: This vector represents the facial features. Two images of the same person will produce vectors that point in roughly the same direction in 512-dimensional space.
*   **Optimization**: Image normalization is accelerated using LLVM auto-vectorization (SIMD) instead of external crates.

### Step 5: Comparison (Matching)
*   **Metric**: **Cosine Similarity**.
*   **Formula**: $Similarity = A \cdot B$ (Dot product of two normalized vectors).
*   **Threshold**: **0.6**.
    *   Score > 0.6: Match Confirmed.
    *   Score < 0.6: Match Failed.

---

## 3. Data Storage & Security Procedure

FaceAuth **never stores actual photos** of your face on the disk. It only stores the mathematical embeddings.

### Storage Location
*   Path: `~/.local/share/faceauth/<username>/models.enc`

### Encryption Standard
*   **Algorithm**: **AES-256-GCM** (Galois/Counter Mode).
*   **Key Generation**: A 256-bit master key is generated on first run and stored in `~/.local/share/faceauth/master.key` (Note: In a production enterprise environment, this should be stored in the TPM).
*   **Nonce**: A unique 96-bit random nonce is generated for every save operation.

### File Structure (`models.enc`)
The file is binary and follows this layout:
1.  **Nonce (12 bytes)**: The random initialization vector used for encryption.
2.  **Ciphertext (Variable)**: The encrypted JSON data.
    *   *Decrypted Content*: A JSON object containing:
        *   `user`: Username.
        *   `embeddings`: A list of 512-float vectors (one for each enrolled angle).
        *   `last_updated`: Timestamp.

### Why this is secure?
1.  **No Images**: Even if an attacker decrypts the file, they cannot reconstruct your face photo from the 512 numbers (it is a one-way abstraction).
2.  **Tamper Proof**: AES-GCM provides integrity checking. If someone modifies the file, decryption will fail immediately.

---

## 4. Full Authentication Flow

When you run `sudo` or try to log in:

1.  **PAM Trigger**: The `pam_faceauth.so` module is loaded by the system.
2.  **IPC Request**: PAM sends an `AUTH_REQUEST` to the `faceauthd` daemon via a Unix socket.
3.  **Daemon Wakeup**:
    *   The daemon wakes up the camera.
    *   It captures a frame.
4.  **AI Inference**:
    *   **Detector**: Is there a face? (Frame 0 only).
    *   **Parallel Recognition**: The daemon spawns parallel threads to process all 5 frames simultaneously.
    *   **Recognizer**: Get the 512-vector for the detected face.
5.  **Matching**:
    *   The daemon decrypts your stored `models.enc`.
    *   It compares the live vector against *all* your enrolled vectors.
    *   It finds the highest similarity score.
6.  **Decision**:
    *   If Score > 0.6: Daemon sends `AUTH_SUCCESS`. PAM allows login.
    *   If Score < 0.6: Daemon sends `AUTH_FAIL`. PAM asks for password.

---

## 5. Advanced Features

### Liveness Detection
To prevent spoofing with static photos, the system analyzes the sequence of 5 frames captured during authentication.
*   **Method**: Frame Difference Analysis.
*   **Logic**: We calculate the average pixel difference between consecutive frames.
    *   **Static (< 0.005)**: Rejected (Likely a photo).
    *   **Natural (0.005 - 0.15)**: Accepted (Natural head movement/noise).
    *   **Chaotic (> 0.15)**: Rejected (Excessive motion or lighting changes).

### Rate Limiting
To prevent brute-force attacks on the model:
*   **Algorithm**: Token Bucket / Lockout.
*   **Policy**: 3 failed attempts trigger a 60-second lockout for that user.
*   **Implementation**: In-memory tracking within the daemon.

### Configuration
The system is configurable via `/etc/faceauth/config.toml`:
```toml
[detection]
confidence_threshold = 0.4
min_face_size = 64

[recognition]
match_threshold = 0.40
strong_match_threshold = 0.55

[camera]
warmup_frames = 10
sequence_length = 5

[security]
require_liveness = true
max_attempts = 3
lockout_seconds = 60
```
