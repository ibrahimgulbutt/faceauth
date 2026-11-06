# FaceAuth technical details

How FaceAuth turns camera frames into an unlock decision. For flows, messages
and trust boundaries see [docs/DESIGN.md](docs/DESIGN.md); for the component
overview see [ARCHITECTURE.md](ARCHITECTURE.md).

## Libraries

| Crate | Used for |
|---|---|
| `ort` (ONNX Runtime) | Running the detection and recognition models |
| `nokhwa` | V4L2 camera capture (MJPEG, YUYV, NV12, GREY) |
| `image`, `ndarray` | Image manipulation, tensors, JPEG preview encoding |
| `aes-gcm`, `sha2` | Encrypted profiles; model IDs |
| `tokio` | The daemon's socket server and request streaming |
| `serde`, `serde_json` | Protocol messages and stored profiles |
| `gtk4`, `libadwaita`, `cairo-rs` | The Face Unlock app |

## Pipeline

**Capture → Detect → Align → Embed → Compare → Vote**

### 1. Capture

- **Camera choice.** `camera.device = "auto"` picks the first device with a
  colour format; a path selects one explicitly, for example an IR camera.
- **Format choice, in order:**
  1. MJPEG closest to 1280×720;
  2. YUYV closest to 640×480;
  3. GREY closest to 640×480;
  4. anything else.

  GREY (infrared) frames are expanded to three channels.
- **Adaptive warm-up.** The daemon remembers when the camera was last closed,
  using wall-clock time so a suspend counts as idle:

  | Idle time | Settle delay | Warm-up frames |
  |---|---|---|
  | < 30 s | 0 ms | 2 |
  | 30 s – 5 min | 100 ms | 4 |
  | longer | 300 ms | `warmup_frames` |

- **Overlap.** Frame 0 goes to detection while frames 1–N are captured, every
  `sequence_interval_ms`.
- **Brightness guard.** A frame whose mean brightness is outside a usable range
  ends the attempt as `BadLighting`.

### 2. Detection: SCRFD-500MF (`det_500m.onnx`, from InsightFace `buffalo_sc`)

- **Input.** The frame is letterboxed into 640×640: scaled to fit while keeping
  its aspect ratio, then padded. It is normalised with `(p − 127.5) / 128`.
  Earlier versions squashed the frame, which distorted faces and landmarks.
- **Decoding.** Outputs at strides 8, 16 and 32 give scores, distance-encoded
  boxes and 5 landmarks per anchor (2 anchors per grid cell). Anchors above
  `detection.confidence_threshold` are decoded, mapped back to frame
  coordinates and merged with non-maximum suppression.
- **Selection.** The **largest** face is used, among those scoring close to the
  best one. Someone farther in the background can't be picked over the person
  in front.

### 3. Alignment

A 4-DOF similarity transform (closed-form Umeyama least squares) maps the five
landmarks (eyes, nose tip, mouth corners) onto ArcFace's canonical 112×112
positions. The frame is then warped with bilinear sampling. ArcFace was trained
only on faces aligned this way; without it, similarity scores collapse when
distance or head angle changes. Frames 1–N reuse frame 0's landmarks, because
the face barely moves within ~200 ms.

### 4. Embedding: ResNet50 or MobileFaceNet

`recognition.model` selects the recognizer; both were trained by InsightFace
with ArcFace loss on WebFace600K and produce 512-dimensional embeddings:

| `model` | File | Source | Threads (`threads = 0`) |
|---|---|---|---|
| `accurate` (default) | `arcface_r50.onnx` | `buffalo_l/w600k_r50.onnx` | half the cores, at most 4 |
| `fast` | `arcface.onnx` | `buffalo_sc/w600k_mbf.onnx` | 1 |

The aligned RGB crop is normalised with `(p − 127.5) / 127.5` and the output is
L2-normalised. A zero or non-finite vector is an error, never a match. Frame 0
is detected and embedded on a second thread while the camera is still
capturing frames 1–4. Legacy `*_int8.onnx` files from versions before 1.0 are
only used when the original file is absent.

### 5. Comparison and vote

- **Similarity.** Cosine similarity (a dot product of unit vectors) between a
  frame's embedding and every stored sample; the best score counts for that
  frame. Vectors of different lengths never match.
- **Vote.** A frame matches when its best score exceeds
  `recognition.match_threshold` (default 0.55). The attempt succeeds when a
  majority of `sequence_length` frames match.
- **Liveness.** With `require_liveness`, the mean absolute difference between
  consecutive frames must fall within a range typical of a live scene. This
  rejects perfectly static images only.

### 6. Rate limiting

Per user, in memory. After `max_attempts` attempts that end in `NotMatched` or
`NotLive`, face unlock answers `LockedOut` for `lockout_seconds`. `NoFace` and
`BadLighting` don't count.

## Enrollment

Enrollment runs entirely in the daemon. The full flow is in
[DESIGN.md](docs/DESIGN.md#flow-enrollment).

- **Steps.** "Set up" captures 10 samples: Center ×2, Left ×2, Right ×2,
  Up ×2, Down ×2. "Improve" captures 6.
- **Head pose** comes from the landmarks:
  - **yaw** = horizontal nose offset from the eye midpoint ÷ eye distance;
  - **pitch** = where the nose sits between the eye line and the mouth line.

  Both are relative to the user's first frontal sample. A step's pose
  requirement is relaxed after 6 s.
- **A sample is accepted only when:**
  - there is exactly one face;
  - it is at least `min_face_size` px wide;
  - face brightness is 45–215;
  - the pose matches;
  - similarity is ≥ 0.40 to the samples already collected (same person);
  - similarity is ≤ 0.92 to each of them (not a duplicate);
  - at least 350 ms have passed since the previous sample.
- **Preview.** Every frame is sent to the client mirrored, as a JPEG, with the
  face box normalised to 0–1.
- **Commit.** The profile is written atomically at the end, never during
  capture. A profile holds at most 40 samples.

## Storage

Path: `/var/lib/faceauth/<user>/models.enc`, owned by `faceauth`, mode 0600.

| Item | Format |
|---|---|
| `master.key` | 32 random bytes, mode 0600, created on first start |
| `models.enc` | 12-byte random nonce followed by AES-256-GCM ciphertext |
| Plaintext | JSON: `user`, `embeddings` (lists of 512 floats), `model_id`, `created`, `updated` |

- **Model ID.** `model_id` is the first 16 hex characters of the SHA-256 of the
  recognition model file. A profile whose ID differs from the installed model is
  reported as `ModelChanged`. Profiles from before 1.0 have no ID and are
  adopted by the current model.
- **What the encryption protects.** GCM authenticates the ciphertext, so a
  modified file fails to decrypt instead of loading altered data. The key lives
  on the same disk, so this protects a copied profile file. It does not protect
  against someone with root or with the whole unencrypted disk.

## Configuration

`/etc/faceauth/config.toml`. All keys are optional; see
[dist/config.toml](dist/config.toml) for the commented defaults. Invalid values
stop the daemon with a clear error rather than weakening matching:

- `match_threshold` outside (0, 1];
- `sequence_length` of 0, or below 2 when liveness is enabled;
- `max_attempts` of 0.
