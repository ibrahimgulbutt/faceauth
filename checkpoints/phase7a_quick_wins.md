# Phase 7A: Quick Wins - Overlapped Capture & Optimization

## Objective
Reduce total authentication latency to ~1.15s by optimizing camera initialization and overlapping hardware capture with AI processing.

## Changes Implemented

### 1. Camera Architecture Refactor (`faceauthd/src/camera.rs`)
- **ActiveCamera Pattern**: Decoupled camera session management from capture logic.
- **Session Persistence**: Introduced `start_session()` to hold the camera stream open, allowing fine-grained control over frame capture.
- **Warmup Control**: Moved warmup logic to `ActiveCamera::warmup()`, allowing it to be called explicitly.

### 2. Overlapped Capture & Processing (`faceauthd/src/main.rs`)
- **Parallel Execution**:
    - **Thread A (Camera)**: Captures Frame 0 -> Sends to Main -> Captures Frames 1-4.
    - **Thread B (Detection)**: Receives Frame 0 -> Runs Face Detection.
- **Concurrency**: Detection on Frame 0 now happens *while* the camera is capturing the remaining frames.
- **Latency Reduction**: This effectively hides the detection latency (~120ms) behind the camera capture latency (~160ms).

### 3. Configuration Tuning (`faceauthd/src/config.rs`)
- **Reduced Warmup**: Lowered `warmup_frames` from 3 to 2 (~66ms saving).
- **Sequence Interval**: Maintained at 40ms.

## Expected Performance
- **Old Flow**: Warmup (100ms) + Capture (200ms) + Detect (120ms) + Rec (40ms) ≈ 460ms + Overhead.
- **New Flow**: Warmup (66ms) + Capture Frame 0 (33ms) + max(Capture Rest (160ms), Detect (120ms)) + Rec (40ms) ≈ 300ms + Overhead.
- **Real-world Impact**: Should feel significantly snappier.

## Next Steps
- Run `cargo build` to verify compilation.
- Run benchmarks to validate the latency reduction.
