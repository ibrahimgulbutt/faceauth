# Changelog

All notable changes to FaceAuth are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/).

## [1.0.0] - unreleased

First release meant for general use. Versions before 1.0 have critical local
privilege-escalation bugs; upgrade, and set up face unlock again afterwards.

### Security

- **The PAM module can't take down a login.** It runs inside the login screen,
  the lock screen and sudo; any internal error now falls back to the password
  instead of crashing the host process.
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
- **Infrared cameras work.** IR cameras could never be opened before: the
  camera library asks V4L2 for the wrong greyscale format code. They are now
  read directly through V4L2. `camera.device = "ir"` selects one, the installer
  offers it, `camera.ir_emitter` can switch on an emitter that stays off, and
  emitters that flash on alternate frames are handled. Profiles record which
  kind of camera they were made with.
- **Better in dim light.** Dark frames and faces are brightened before
  detection and recognition, the camera gets extra frames to adjust its
  exposure in the dark, and setup accepts dimmer samples.
- **Camera names** come from the UVC interface, so the colour and IR halves of
  a combined camera are told apart ("Integrated IR Camera").
- **Improve stops when nothing matches.** If no sample matches the saved face
  data for about 10 seconds, setup ends with a clear message instead of
  waiting forever.
- `faceauth benchmark` reports the face brightness and warns when an IR
  camera's light seems to be off.
- **A face that reacts (GNOME 45+).** A GNOME Shell extension shows an
  animated face while FaceAuth checks you: on the lock screen in place of your
  picture, and in the corner for sudo and admin prompts. It looks for you,
  smiles when you're recognized and looks sad when you're not. It's driven by
  the new `Watch` request, which streams only the caller's own face checks
  (capped at 4 watchers per user, 16 in all). The installer adds and enables
  it (`--no-animation` skips it); `faceauth watch` prints the same events.
- **Step-by-step install guide with screenshots** (`docs/INSTALL_GUIDE.md`).
- **One command does everything.** An infrared camera is offered (and is the
  default answer) when present. The installer checks the IR light through the camera; if it's
  off, it downloads linux-enable-ir-emitter 7.0.0 (pinned, SHA-256 verified), guides
  the user through it, imports the result, checks again, and falls back to the
  colour camera if the light can't be switched on.
- **Re-running the installer says what it will do:** install, upgrade
  ("FaceAuth A is installed. Upgrading to B"), repair the same version, or refuse a silent downgrade. Current settings are the defaults, and
  the summary says what happened.
- **IR light settings are imported.** When the infrared camera is chosen, the
  installer reads what linux-enable-ir-emitter (v7) saved for that camera and
  writes it to `[[camera.ir_emitter]]` (several commands supported). FaceAuth
  then switches the light on itself every time, and only ever sends these
  commands to an infrared stream. `faceauth ir-emitter` shows the saved
  settings; `faceauth doctor` reports whether the light is being switched on.
- **Face data from before 1.0 is paused, not compared.** It was made with the old INT8 model, so face unlock reports that it needs setting up again (in sudo, `faceauth status` and the app) instead of failing the match and counting toward lockout.

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
