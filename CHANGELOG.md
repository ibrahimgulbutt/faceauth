# Changelog

All notable changes to FaceAuth are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/).

## [1.0.0] - unreleased

First release meant for general use. Versions before 1.0 have critical local
privilege-escalation bugs; upgrade, and set up face unlock again afterwards.

### Security

- **Fixed path traversal in the daemon.** Usernames were joined onto the
  storage path unchecked. Any local user could make the root daemon delete any
  directory, including `/` (`DeleteUser { user: "/" }`), or create directories
  anywhere. Usernames are now validated at the protocol layer and again in
  storage.
- **Fixed enrollment for other users.** The daemon trusted the username in each
  request, so any local user could add their own face to another account (for
  example with `USER=root faceauth-gui`), delete it, lock it out, or list
  everyone enrolled. The daemon now identifies every caller with `SO_PEERCRED`.
  Only root or the user themselves may authenticate, enroll or remove for an
  account, and non-root callers only see themselves.
- **Fixed a fail-open recognizer.** When the ArcFace model was missing, every
  face produced the same placeholder embedding, so any face unlocked any
  account. The daemon now refuses to start without a working model and rejects
  degenerate embeddings.
- **The socket moved out of `/tmp`.** It is now `/run/faceauth/faceauth.sock`, in
  a directory only the daemon user can write. The PAM module only accepts
  answers from a peer running as root or the `faceauth` user, so a squatted
  socket can't approve logins.
- **Message size is capped.** An unbounded length prefix let any user make the
  daemon allocate up to 4 GB; messages are now capped. Idle connections time
  out.
- **Remote sessions are skipped.** `sudo` over SSH could be approved by whoever
  sat in front of the camera. The PAM module now skips remote sessions
  (`PAM_RHOST`, SSH environment variables).
- **PAM lines from older installers are moved.** Earlier installers put
  `pam_faceauth.so` at line 1 of the PAM file, above `pam_nologin` and
  `user != root` checks, so a face match skipped them. The installer now moves
  such lines to just before password authentication, keeping a backup in
  `<file>.faceauth-move-backup`.
- **`pam_sm_acct_mgmt` returns `PAM_IGNORE`.** It returned `PAM_SUCCESS`, so a
  misplaced `account` line could skip expiry and access checks.
- **Enrollment changes need your password.** Enrolling, adding samples and
  removing data go through polkit (`org.faceauth.manage`, `auth_self`). While
  that check runs, the daemon refuses face authentication for the same user, so
  face unlock can't approve a change to itself.
- **The daemon is unprivileged and sandboxed.** It runs as the dedicated
  `faceauth` system user in a strict systemd sandbox: no capabilities, a
  read-only system, no home directories, no network, video devices only, a
  syscall filter, and core dumps disabled. Data moved to `/var/lib/faceauth`.
- **Safer storage and config handling.**
  - The master key and profiles are created with mode `0600`.
  - A corrupt key file produces a clear error instead of a crash.
  - The daemon no longer reads a `config.toml` from its working directory.
  - Values that would crash it or accept anyone are rejected.
- **Models are verified.** The installer checks each model against a pinned
  SHA-256 checksum and aborts on a mismatch.
- **Scores stay private.** Raw match scores are never sent to non-root clients,
  so the test feature can't be used to tune a spoof.

### Added
- **Choice of recognizer.** `recognition.model = "accurate"` (ResNet50, the new default) or `"fast"` (MobileFaceNet), chosen at install time with `--model`. On InsightFace's hard mixed benchmark the ResNet50 model scores 91.25 vs 71.87 (East Asian faces: 74.96 vs 51.03). It adds roughly 0.2–0.3 s to a login. `faceauth status`, `benchmark` and the app show which one is in use.
- `dist/fetch-models.sh`: downloads the models from InsightFace's official releases and verifies the archives and extracted files against pinned SHA-256 values; used by the installer and for development.
- GUI demo mode (`FACEAUTH_GUI_DEMO`) with a synthetic camera for UI work and face-free screenshots; screenshots in the README.

- **Daemon-side enrollment.** The daemon owns the camera and streams a mirrored
  preview to the app. Clients never send images.
- **Pose-guided enrollment.**
  - Straight, left, right, up and down, checked against facial landmarks, and
    relaxed after a few seconds so nobody gets stuck.
  - Quality checks: exactly one face, close enough, not too dark or bright.
  - Same-person consistency check and near-duplicate rejection.
- **Real cancel.** Closing the app or pressing Cancel stops the camera and
  discards the session. Enrollment is committed atomically at the end, so a
  cancelled "set up again" keeps your old enrollment.
- **Model ID in profiles.** A profile made with a different recognition model
  is reported as needing setup again, instead of silently failing.
- **Camera selection.** `camera.device` in the config selects a camera
  (including infrared cameras with greyscale formats). `faceauth cameras` lists
  devices and marks likely IR cameras.
- **PAM feedback.** "Look at the camera…" and the reason for a failure are
  shown, sent only once the daemon actually starts scanning. Silence them with
  the `quiet` module argument or `PAM_SILENT`.
- **Specific outcomes** (matched, not recognized, no face, bad lighting,
  locked out, not set up, needs setup again, camera unavailable, busy) for PAM,
  the CLI and the app.
- **CLI commands:** `faceauth` (status), `enroll [--add]`, `test`, `remove`,
  `cameras`, `version`, alongside an improved `doctor`, `list`, `benchmark` and
  `ping`. `enroll` can ask for your password in the terminal.
- **New Face Unlock app** (GTK 4.12+ / libadwaita 1.4+):
  - circular face-centred preview, progress ring and turn arrows;
  - clear per-state pages: not set up, set up, service unavailable;
  - a built-in test;
  - "Used for" list, confirmation before removal.
- **Uninstaller:** `uninstall.sh`, also installed to
  `/usr/local/share/faceauth/`. It reverts exactly the PAM edits the installer
  made.
- **Project infrastructure:** CI (build, tests, clippy, shellcheck),
  `SECURITY.md`, this changelog, and `docs/DESIGN.md`.

### Changed
- **Models come from InsightFace directly** and are the original files, not this project's INT8 conversions (which were only ~25% faster, with unknown accuracy loss). Nothing is rehosted, so no model files need to be published with a FaceAuth release. Old INT8 files are removed on upgrade.
- Frame 0 is detected and recognised while the camera is still capturing the rest of the sequence.

- **Better detection.**
  - Frames are letterboxed to 640×640 instead of being squashed, which improves
    landmark accuracy.
  - Overlapping detections are merged with non-maximum suppression.
  - The largest face is used instead of the highest-scoring one.
- **Fairer lockout.** Only real impostor attempts (face not recognized, or
  looked like a photo) count toward it; bad lighting or no face in view don't.
- **Installer** (rewritten):
  - prompts now read from the terminal, so `curl | bash` no longer consumes the
    script as answers;
  - installs a pinned release tag, or builds the checkout it is run from;
  - finds the PAM directory on every distribution (Fedora, ARM);
  - inserts the PAM line after `pam_nologin` / root checks instead of at the top
    of the file;
  - offers sudo, login screen, lock screen and admin prompts;
  - migrates old data, and waits for the service to answer.
- **No group change or re-login.** Users no longer need to join the `video`
  group or log out and back in.
- **Requirements:** the GUI needs GTK 4.12+ and libadwaita 1.4+. On older
  systems the installer skips it, and `faceauth enroll` sets up face unlock in
  the terminal.
- **Tagline:** "Windows Hello for Linux" is now "Face unlock for Linux".
- **Honest docs.** The README no longer claims photo and replay resistance, a
  per-user key, or RGB Windows Hello support, and it states the model licence
  (InsightFace: non-commercial research only).

### Fixed

- The old app's Cancel button left the camera running and kept saving samples.
- "Re-enroll" deleted your data before capturing, so cancelling left you with
  nothing.
- "Delete Enrollment" had no confirmation and no error feedback.
- The app showed other users' enrollments and treated any enrolled user as
  "you".
- `faceauth doctor` reported missing models on every fresh install.
- The installer started the service before models and config existed, and
  overwrote a tracked file in its own clone, which broke later updates.
- `sequence_length = 0` crashed the daemon.

[1.0.0]: https://github.com/ibrahimgulbutt/faceauth/releases/tag/v1.0.0
