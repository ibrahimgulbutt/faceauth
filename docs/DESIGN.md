# FaceAuth design

This document is the source of truth for how FaceAuth's pieces fit together:
who trusts whom, what each user-facing flow does step by step, and why.
Code that disagrees with it is a bug in one of the two.

## Components and trust

```
 ┌──────────────┐  PAM   ┌───────────────┐
 │ sudo / GDM / │──────▶ │ pam_faceauth  │ runs inside the PAM application
 │ SDDM / ...   │        │ (root, or the │ (usually root)
 └──────────────┘        │  user itself) │
                         └──────┬────────┘
 ┌──────────────┐               │ /run/faceauth/faceauth.sock
 │ faceauth-gui │──────┐        │ length-prefixed JSON
 │ faceauth CLI │      ▼        ▼
 └──────────────┘   ┌──────────────────┐   pkcheck   ┌─────────┐
   (the user)       │    faceauthd     │───────────▶ │ polkit  │──▶ password dialog
                    │ user "faceauth"  │             └─────────┘    in the user's session
                    │ camera · models  │
                    │ /var/lib/faceauth│
                    └──────────────────┘
```

| Boundary | Rule |
|---|---|
| Client → daemon | The daemon identifies every caller by `SO_PEERCRED` (kernel-supplied uid/pid). Usernames in requests are never trusted by themselves. |
| Daemon → PAM | `pam_faceauth` only accepts an answer from a socket whose peer uid is `0` or the `faceauth` system user. The socket lives in `/run/faceauth/`, which only that user can write. |
| Changing face data | Enrolling, adding samples and removing data require the user's password through polkit (`org.faceauth.manage`, `auth_self`). Root callers skip polkit. |
| Face auth while authorizing | While polkit is authorizing a change for user *X*, the daemon refuses face authentication for *X*. A polkit stack that includes `pam_faceauth` therefore cannot approve an enrollment change with the face it is about to change. |
| Remote sessions | `pam_faceauth` returns `PAM_IGNORE` when `PAM_RHOST` names another host or SSH variables are present. |

Compromising the daemon is equivalent to root, because PAM trusts its answer.
Running it as an unprivileged, sandboxed user still contains bugs that would
otherwise let a client read, write or delete files as root.

## Files

| Path | Owner / mode | Purpose |
|---|---|---|
| `/usr/local/bin/faceauthd`, `faceauth`, `faceauth-gui` | root 0755 | Daemon, CLI, GUI |
| `<pam dir>/pam_faceauth.so` | root 0644 | PAM module (directory of `pam_unix.so`) |
| `/etc/systemd/system/faceauth.service` | root 0644 | Sandboxed unit, `User=faceauth` |
| `/usr/share/polkit-1/actions/org.faceauth.manage.policy` | root 0644 | Polkit action, owned by `unix-user:faceauth` |
| `/etc/faceauth/config.toml` | root 0644 | Configuration (only path the daemon reads) |
| `/etc/faceauth/pam-services` | root 0644 | PAM files the installer edited, for clean uninstall |
| `/usr/share/faceauth/models/*.onnx` | root 0644 | SCRFD-500MF detector; ResNet50 (`accurate`) and/or MobileFaceNet (`fast`) recognizer, downloaded from InsightFace's releases, SHA-256 pinned in `dist/fetch-models.sh` |
| `/var/lib/faceauth/` | faceauth 0700 | `master.key` and `<user>/models.enc` |
| `/run/faceauth/faceauth.sock` | faceauth, dir 0755, socket 0666 | IPC |
| `/usr/local/share/faceauth/uninstall.sh` | root 0755 | Uninstaller |

Development overrides, ignored by the PAM module: `FACEAUTH_SOCKET`,
`FACEAUTH_STATE_DIR`, `FACEAUTH_MODELS_DIR`, `FACEAUTH_CONFIG`.
`FACEAUTH_DEV_SKIP_POLKIT=1` is honoured only when the daemon runs as an
ordinary login user (never root, never `faceauth`), and such a daemon is never
trusted by PAM.

## Protocol (version 2)

Each message is a `u32` big-endian length (at most 4 MiB) followed by JSON
(serde's externally tagged enums, defined in `faceauth-core`).

| Request | Who may send it | Responses |
|---|---|---|
| `Ping` | anyone | `Pong { version, protocol }` |
| `Status` | anyone (about themselves) | `Status(UserStatus)` |
| `Authenticate { user }` | root or that user | `Scanning`, then `Auth(AuthOutcome)` |
| `Enroll { mode, user }` | self via polkit; root for anyone | stream of `Enroll(EnrollEvent)` |
| `Remove { user }` | self via polkit; root for anyone | `Removed` |
| `ListEnrolled` | anyone (non-root sees only themselves) | `Users(..)` |
| `Cameras` | anyone | `Cameras(..)` |
| `Benchmark` | anyone | `Benchmark(..)` |
| `Watch` | anyone (their own checks only) | `Watching`, then `AuthEvent(Scanning \| Finished(AuthOutcome))` per check, until disconnect |

Any request may instead get `Denied { reason }` or `Error { message }`.

## Flow: authentication (PAM)

1. PAM application calls `pam_sm_authenticate`.
2. Module gets the user; skips (`PAM_IGNORE`) if the session is remote.
3. Connects to `/run/faceauth/faceauth.sock`; skips if the daemon is not running
   or the peer is not root/`faceauth`.
4. Sends `Authenticate { user }`. The daemon checks, in order, *without touching
   the camera*:
   * caller is root or the user → else `Denied`
   * user is enrolled → else `NotEnrolled`
   * enrollment model matches the installed model → else `ModelChanged`
   * not locked out → else `LockedOut { seconds }`
   * no enrollment change is being authorized for this user → else `Busy`
   * the camera is free within 1.5 s → else `CameraUnavailable`
5. Daemon sends `Scanning`; the module shows **"FaceAuth: look at the camera…"**
   (suppressed by `PAM_SILENT` or the `quiet` module argument).
6. Capture → detect (largest face; frame 0, or the last frame if the sensor had
   not settled) → align → liveness → embed → consensus vote over the frame
   sequence, stopping as soon as the vote is decided. Outcome is one of `Matched`, `NotMatched`,
   `NotLive`, `NoFace`, `BadLighting`.
7. Only `NotMatched` and `NotLive` count toward lockout. Bad lighting or no face
   in view are not impostor attempts.
8. Module maps the outcome:

| Outcome | PAM result | Message |
|---|---|---|
| `Matched` | `PAM_SUCCESS` | — |
| `NotMatched`, `NotLive`, `NoFace`, `BadLighting` | `PAM_AUTH_ERR` | short reason |
| `LockedOut` | `PAM_AUTH_ERR` | "too many attempts, try again in N s" |
| `NotEnrolled`, `ModelChanged`, `CameraUnavailable`, `Busy`, errors | `PAM_IGNORE` | `ModelChanged` asks to set up again |

The password is always the next module in the stack, so every non-success
falls through to it.

## Flow: enrollment

Enrollment always happens in the daemon: it owns the camera and streams a
preview to the client. Clients never send images, need no camera access, and
need no `video` group or re-login after install.

```
client                         daemon
  │ Enroll{mode}  ───────────────▶ │ resolve user from peer uid (root: from request)
  │ ◀──────────── Authorizing      │ mark user "authorizing" (blocks face auth)
  │      [system password dialog]  │ pkcheck --process pid,start,uid --allow-user-interaction
  │ ◀──────── Failed(AuthCancelled)│   (or NotAuthorized / NoAuthAgent) ── end
  │ ◀──────── Started{steps}       │ open camera (fail: CameraUnavailable)
  │ ◀──────── Frame{jpeg,face}  ×N │ ~15 fps mirrored preview
  │ ◀──────── Progress{collected, pose, hint}  on change
  │   ...                          │
  │ ◀──────── Completed{samples}   │ write profile atomically, then close camera
```

* **Cancel** = the client closes the connection. The daemon notices on its next
  write, stops the camera and discards everything. A cancelled re-enrollment
  leaves the previous enrollment intact. Nothing is written until the end.
* **Modes**: `Replace` (first setup and "set up again", 10 samples) and
  `Append` ("improve recognition", 6 samples). Profiles hold at most 40
  samples; `Append` past that fails with `Full`.
* **Steps**: `Replace` = Center ×2, Left ×2, Right ×2, Up ×2, Down ×2.
  `Append` = Center ×2, Left, Right, Up, Down.
* **A sample is accepted when all of these hold** (otherwise the hint says why):
  1. exactly one face (`MultipleFaces`); a second face at least ¼ the size of
     the largest counts;
  2. face at least `min_face_size` px wide (`MoveCloser`);
  3. face brightness within 28–215 (`TooDark`, or `IrLightOff` on an IR camera / `TooBright`);
  4. head pose matches the step (`TurnMore` / `TooFar`), relaxed after 6 s on a
     step so nobody gets stuck;
  5. same person: similarity ≥ 0.40 to the samples already collected (or, in
     `Append`, to the stored ones) (`DifferentPerson`);
  6. not a near-duplicate (similarity ≤ 0.92) of a sample already taken
     (`MoveSlightly`);
  7. at least 350 ms since the previous sample.
* **Detection** letterboxes the frame to 640×640 (aspect ratio kept), merges
  overlapping boxes with NMS, and also drops boxes lying mostly inside a
  stronger one: ScrFD sometimes fires a partial-face box inside the real face,
  which must not count as a second person. The face used is the largest one
  among those scoring at least 60% of the best.
* **Pose** comes from the 5 ScrFD landmarks. Yaw = horizontal nose offset from
  the eye midpoint ÷ eye distance; pitch = nose position between eye line and
  mouth line. Both are measured relative to the user's own first frontal sample.
* **Time limit**: 120 s for capture and 120 s for the polkit dialog.
* **Preview** runs on its own thread at up to ~16 fps (time-throttled, since
  webcams drop to 15 fps or less in dim light); analysis always takes the
  newest frame, so a slow detector never makes the preview lag.
* The profile stores the recognizer's model ID (first 16 hex chars of the model
  file's SHA-256). A different installed model makes the profile unusable
  (`ModelChanged`) until the user sets up again.

## Flow: removal

`Remove` → polkit (self) → delete `/var/lib/faceauth/<user>` → `Removed`. The
GUI asks for confirmation first; the CLI asks unless `--yes`.

## Face animation (GNOME Shell extension)

`dist/gnome-shell/faceauth@ibrahimgulbutt.github.io`, installed system-wide
for GNOME 45+ and enabled for the installing user.

* It connects to the socket and sends `Watch`. The daemon tags every
  `authenticate` event with the uid of the account being checked and sends
  each watcher only events for its own uid (`SO_PEERCRED`), so a user learns
  only when their own account is checked, and the result.
* Watchers are capped at 4 per uid and 16 in all, so they can't use up the
  client slots logins need. The extension reconnects with back-off.
* `Scanning` shows the face (looking); `Finished(Matched)` smiles,
  `Finished(NotMatched | NotLive | NoFace | BadLighting)` looks sad, any other
  outcome hides it. A check that never reached the camera shows nothing.
* Placement: on the lock screen, over the user picture above the password box
  (hidden while the face is up; a fixed spot if it isn't there); otherwise a
  card in the bottom-right corner (sudo, polkit prompts). Added with
  `addTopChrome`, so it is above windows and the lock screen and never takes
  input.
* It only displays. Nothing in it can approve or block authentication.
* Not covered: the GDM login screen before a session exists, and other
  desktops (they keep the PAM text messages).

## GUI (GTK ≥ 4.12, libadwaita ≥ 1.4)

Navigation (`AdwNavigationView`):

```
Home ─┬─ (not set up) ── Intro ── Capture ── Done ── Test
      ├─ (set up) ─┬─ Test
      │            ├─ Improve ── Capture ── Done
      │            ├─ Set up again ── Capture ── Done
      │            └─ Remove (confirm dialog)
      └─ (service unavailable) ── Retry
```

* **Home** loads status in the background and shows one of: *loading*;
  *service unavailable* (with the command to start it); *not set up* (hero +
  "Set Up"); *set up* (status, "Used for" list read from `/etc/pam.d`, actions,
  outdated-model banner).
* **Capture**: circular, mirrored, face-centred preview; a ring of ticks that
  fills as samples are accepted; an arrow showing which way to turn; a
  one-line instruction and a one-line hint; step chips. Back or Cancel closes
  the connection, which cancels the enrollment. While polkit is pending, the
  circle shows "Waiting for your password…".
* **Done**: success page with "Try It Now" (→ Test) and "Done".
* **Test**: runs `Authenticate` for the current user and shows the outcome in
  plain words, never a raw score (scores would help someone tune a spoof).
* The ring honours the desktop's reduced-motion setting (`gtk-enable-animations`).
* `FACEAUTH_GUI_DEMO` runs every screen against scripted replies and a
  synthetic camera, for UI work and screenshots without anyone's face.

## CLI

`faceauth [status]`, `enroll [--add] [--user U]`, `test [--user U]`,
`remove [--user U] [--yes]`, `list`, `cameras`, `doctor`, `benchmark`,
`version`. `enroll` starts `pkttyagent --fallback` so polkit can ask for the
password in the terminal when no graphical agent is running.

## Models

The recognizer is a config choice (`recognition.model`): `accurate` (ResNet50,
default) or `fast` (MobileFaceNet). The daemon refuses to start if the chosen
file is missing rather than falling back, since a silent switch would
invalidate every profile. The installer downloads InsightFace's original files
from InsightFace's releases (nothing is rehosted) and verifies both the archive
and the extracted file against SHA-256 values in `dist/fetch-models.sh`.

## Camera

`camera.device = "auto"` picks the first V4L2 device offering a colour format
(MJPEG, YUYV, NV12); `"ir"` picks the first infrared one. A path
(`/dev/video2`, `/dev/v4l/by-id/...`) selects a device explicitly. Format
preference: MJPEG closest to 1280×720 → YUYV closest to 640×480 → GREY closest
to 640×480 → anything else. GREY (IR) frames are read with `v4l` and expanded
to RGB before detection.

Profiles record whether they were made with an IR camera; a profile from the
other kind of camera is `ModelChanged` (set up again). The installer offers
the IR camera when it finds one. IR emitters often need
[linux-enable-ir-emitter](https://github.com/EmixamPP/linux-enable-ir-emitter);
`camera.ir_emitter` re-sends its control each time the stream starts, and
emitters flashing on alternate frames are handled by skipping dark frames.
`faceauth benchmark` reports face brightness, which shows whether the emitter
is on.

## Install, upgrade, uninstall

* `install.sh` builds the checkout it lives in. Run via `curl | bash`, it
  clones the pinned release tag. All prompts read from `/dev/tty`.
* Order: dependencies → build → create `faceauth` user → install files →
  recognizer choice (`--model`) → models from InsightFace (verified) → config (kept if present; `recognition.model` set) → migrate
  `/root/.local/share/faceauth` → start service → wait for `Ping` → PAM
  choices.
* The GUI is built only when GTK ≥ 4.12 and libadwaita ≥ 1.4 are available;
  otherwise the CLI is the enrollment path.
* PAM lines are inserted before the first `auth` / `@include common-auth` line.
  A vendor file under `/usr/lib/pam.d` is copied to `/etc/pam.d` first.
  Edited services go into `/etc/faceauth/pam-services`.
* `uninstall.sh` reverts exactly those edits, and also removes stray
  `pam_faceauth.so` lines. It removes all installed files and asks before
  deleting face data and config (`--purge` deletes without asking).
