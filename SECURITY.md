# Security policy

FaceAuth decides who may run `sudo` and log in, so security reports are taken
seriously and handled before anything else.

## Reporting a vulnerability

Report privately through GitHub:
**[Report a vulnerability](https://github.com/ibrahimgulbutt/faceauth/security/advisories/new)**.

Please don't open a public issue for security problems. Include:

- what an attacker can do and what access they need to start with (any local
  user, a process in the user's session, physical access…);
- steps to reproduce, ideally a minimal script;
- FaceAuth version (`faceauth version`), distribution and how it was installed.

You should get a first reply within 7 days. Once a fix is released, we will
credit you in the advisory and the changelog, unless you prefer otherwise.

## Supported versions

| Version | Supported |
|---|---|
| 1.0.x | ✅ |
| < 1.0 | ❌ Upgrade. Versions before 1.0 had critical local privilege-escalation bugs. |

## Threat model in brief

FaceAuth is a **convenience unlock**, like a fingerprint reader: the password
always remains a valid way in. The trust boundaries it does enforce are listed
in [docs/DESIGN.md](docs/DESIGN.md#components-and-trust), and its limitations
in the README's
[Security model & limitations](README.md#security-model--limitations).

### In scope

Anything that breaks the boundaries FaceAuth claims to enforce, for example:

- a local user getting the daemon to authenticate, enroll, delete or list data
  for **another** user;
- getting `pam_faceauth` to accept an answer from anything other than the real
  daemon (socket squatting, peer-credential bypass);
- path traversal or arbitrary file access through usernames or other inputs;
- changing someone's enrollment without passing the polkit password check,
  including by using face authentication to satisfy that check;
- crashing or hanging the daemon in a way that affects other users, or memory
  corruption in the PAM module;
- defeating the remote-session (SSH) check in a way that doesn't require the
  remote user to deliberately clear their own environment;
- the daemon reading or writing outside its sandbox;
- the installer or uninstaller doing something unsafe, such as installing
  unverified models or leaving PAM in a state that locks users out.

### Out of scope (known limitations)

- **Photo, video or mask attacks against ordinary RGB webcams.** RGB-only face
  recognition can be fooled; the liveness check only rejects completely static
  images. Better anti-spoofing is welcome as a feature, not as a vulnerability
  report.
- Look-alikes and identical twins being recognized.
- Attacks that need root, or physical access to an unlocked session where the
  attacker already knows the password.
- A remote user who deliberately unsets `SSH_*` variables and finds the
  account owner sitting in front of the camera (documented, best-effort check).
- Face login not unlocking GNOME Keyring or KWallet.
