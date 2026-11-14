//! `faceauth` — command-line front end for faceauthd.

mod ir_emitter;
mod polkit_agent;
mod term;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use faceauth_core::{
    pam,
    client_socket_path, recv_message, send_message, AuthOutcome, EnrollEvent, EnrollMode, Hint, Request, Response,
    UserStatus, PROTOCOL_VERSION, VERSION,
};
use std::io::{IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;
use term::Style;

#[derive(Parser)]
#[command(name = "faceauth", version, about = "Face unlock for Linux", long_about = "\
Face unlock for sudo, the login screen, the lock screen and admin prompts.

GETTING STARTED
  faceauth enroll        Set up face unlock (asks for your password)
  faceauth test          Check that the camera recognizes you
  faceauth doctor        Diagnose problems

The graphical app is called Face Unlock (faceauth-gui).")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show whether face unlock is set up for you (default)
    Status,
    /// Set up face unlock, or add samples with --add
    Enroll {
        /// Add samples to your existing setup instead of replacing it
        #[arg(long)]
        add: bool,
        /// Account to set up (root only)
        #[arg(long)]
        user: Option<String>,
    },
    /// Check that the camera recognizes you
    Test {
        /// Account to test (root only; defaults to you)
        #[arg(long)]
        user: Option<String>,
    },
    /// Delete your face data
    Remove {
        /// Account whose data to delete (root only)
        #[arg(long)]
        user: Option<String>,
        /// Don't ask for confirmation
        #[arg(long, short)]
        yes: bool,
    },
    /// List enrolled accounts (root sees everyone)
    List,
    /// List cameras and show which one is used
    Cameras,
    /// Check the daemon, camera, models, PAM and polkit setup
    Doctor,
    /// Measure how long each stage of recognition takes
    Benchmark,
    /// Find and switch on the infrared camera's light automatically (root)
    IrSetup {
        /// Print only the lines for /etc/faceauth/config.toml (for the installer)
        #[arg(long)]
        toml: bool,
    },
    /// Show the IR-light settings saved by linux-enable-ir-emitter
    IrEmitter {
        /// linux-enable-ir-emitter configuration (default: ~/.config/linux-enable-ir-emitter.toml)
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Print only the lines for /etc/faceauth/config.toml (for scripts)
        #[arg(long)]
        toml: bool,
    },
    /// Print your face checks as they happen (sudo, lock screen, admin prompts)
    Watch,
    /// Print client and daemon versions
    Version,
    /// Exit 0 if the daemon answers (for scripts)
    Ping,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Command::Status) {
        Command::Status => status(),
        Command::Enroll { add, user } => enroll(if add { EnrollMode::Append } else { EnrollMode::Replace }, user),
        Command::Test { user } => test(user),
        Command::Remove { user, yes } => remove(user, yes),
        Command::List => list(),
        Command::Cameras => cameras(),
        Command::Doctor => doctor(),
        Command::Benchmark => benchmark(),
        Command::IrSetup { toml } => ir_setup(toml),
        Command::IrEmitter { file, toml } => ir_emitter_cmd(file, toml),
        Command::Watch => watch(),
        Command::Version => version(),
        Command::Ping => return if ping().is_ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE },
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e:#}", Style::Error.paint("error:"));
            ExitCode::FAILURE
        }
    }
}

// ─── Connection ─────────────────────────────────────────────────────────────

fn connect(timeout: Duration) -> Result<UnixStream> {
    let path = client_socket_path();
    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "cannot reach the FaceAuth service at {}. Is it running? Try: sudo systemctl start faceauth",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn request(req: &Request, timeout: Duration) -> Result<Response> {
    let mut stream = connect(timeout)?;
    send_message(&mut stream, req)?;
    Ok(recv_message(&mut stream)?)
}

fn unexpected(r: Response) -> anyhow::Error {
    match r {
        Response::Denied { reason } => anyhow::anyhow!("not allowed: {reason}"),
        Response::Error { message } => anyhow::anyhow!("{message}"),
        Response::Enroll(EnrollEvent::Failed(f)) => anyhow::anyhow!("{}", f.message()),
        other => anyhow::anyhow!("unexpected reply from the service: {other:?}"),
    }
}

fn fetch_status() -> Result<UserStatus> {
    match request(&Request::Status, Duration::from_secs(5))? {
        Response::Status(s) => Ok(s),
        other => Err(unexpected(other)),
    }
}

fn ping() -> Result<(String, u32)> {
    match request(&Request::Ping, Duration::from_secs(3))? {
        Response::Pong { version, protocol } => Ok((version, protocol)),
        other => Err(unexpected(other)),
    }
}

// ─── Commands ───────────────────────────────────────────────────────────────

fn status() -> Result<ExitCode> {
    let s = fetch_status()?;
    let used = pam::configured_services();
    if s.enrolled && s.model_current {
        println!("{} Face unlock is set up for {}", Style::Ok.paint("●"), Style::Bold.paint(&s.user));
        println!("  Samples   {} of {}", s.samples, s.max_samples);
        if s.created != 0 {
            println!("  Set up    {}", term::date(s.created));
        }
    } else if s.enrolled {
        println!("{} Face unlock is paused for {}", Style::Warn.paint("!"), Style::Bold.paint(&s.user));
        println!("  Your face was set up with a different camera or face model. Set it up again:");
        println!("  Run: faceauth enroll");
    } else {
        println!("{} Face unlock is not set up for {}", Style::Dim.paint("○"), Style::Bold.paint(&s.user));
        println!("  Run: faceauth enroll");
    }
    println!("  Camera    {}", s.camera.as_deref().unwrap_or("none found"));
    println!("  Model     {}", s.recognizer);
    println!("  Used for  {}", if used.is_empty() { "nothing yet (re-run the installer to choose)".into() } else { pam::labels(&used) });
    Ok(ExitCode::SUCCESS)
}

fn enroll(mode: EnrollMode, user: Option<String>) -> Result<ExitCode> {
    let who = user.clone().unwrap_or_else(|| fetch_status().map(|s| s.user).unwrap_or_else(|_| "you".into()));
    let title = match mode {
        EnrollMode::Replace => "Setting up face unlock",
        EnrollMode::Append => "Improving face unlock",
    };
    println!("{} for {}", Style::Bold.paint(title), Style::Bold.paint(&who));

    // A terminal password prompt, used only if no graphical agent is running.
    let _agent = polkit_agent::TtyAgent::spawn();

    let mut stream = connect(Duration::from_secs(150))?;
    send_message(&mut stream, &Request::Enroll { mode, user })?;
    let mut view = term::Progress::new();
    loop {
        let event = match recv_message::<_, Response>(&mut stream) {
            Ok(Response::Enroll(e)) => e,
            Ok(other) => return Err(unexpected(other)),
            Err(e) => bail!("lost connection to the service: {e}"),
        };
        match event {
            EnrollEvent::Authorizing => println!("Confirm with your password to continue…"),
            EnrollEvent::Started { steps } => {
                println!("Camera is on. Follow the prompts; press Ctrl+C to cancel.\n");
                view.start(steps.len());
            }
            EnrollEvent::Frame { .. } => {}
            EnrollEvent::Progress { collected, target, pose, hint } => {
                let detail = if hint == Hint::Good { "" } else { hint.message() };
                view.update(collected, target, pose.instruction(), detail, hint.is_problem());
            }
            EnrollEvent::Completed { samples } => {
                view.finish();
                println!("{} Done. Face unlock is ready ({samples} samples).", Style::Ok.paint("✓"));
                println!("  Try it: faceauth test   or   sudo true");
                return Ok(ExitCode::SUCCESS);
            }
            EnrollEvent::Failed(f) => {
                view.finish();
                bail!("{}", f.message());
            }
        }
    }
}

fn test(user: Option<String>) -> Result<ExitCode> {
    let user = match user {
        Some(u) => u,
        None => fetch_status()?.user,
    };
    let mut stream = connect(Duration::from_secs(20))?;
    send_message(&mut stream, &Request::Authenticate { user: user.clone() })?;
    let mut scanning = false;
    loop {
        match recv_message::<_, Response>(&mut stream)? {
            Response::Scanning => {
                print!("Look at the camera… ");
                std::io::stdout().flush()?;
                scanning = true;
            }
            Response::Auth(outcome) => {
                let ok = outcome == AuthOutcome::Matched;
                let mark = if ok { Style::Ok.paint("✓") } else { Style::Error.paint("✗") };
                if scanning {
                    println!();
                }
                println!("{mark} {}", outcome.message());
                if outcome == AuthOutcome::NotMatched {
                    println!("  Tip: add samples in this lighting with: faceauth enroll --add");
                }
                return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
            }
            other => return Err(unexpected(other)),
        }
    }
}

fn remove(user: Option<String>, yes: bool) -> Result<ExitCode> {
    let who = user.clone().unwrap_or_else(|| "your".into());
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to delete without confirmation; pass --yes");
        }
        let target = if user.is_some() { format!("{who}'s") } else { who.clone() };
        print!("Delete {target} face data? Face unlock will stop working. [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing deleted.");
            return Ok(ExitCode::SUCCESS);
        }
    }
    let _agent = polkit_agent::TtyAgent::spawn();
    match request(&Request::Remove { user }, Duration::from_secs(150))? {
        Response::Removed => {
            println!("{} Face data deleted.", Style::Ok.paint("✓"));
            Ok(ExitCode::SUCCESS)
        }
        other => Err(unexpected(other)),
    }
}

fn list() -> Result<ExitCode> {
    match request(&Request::ListEnrolled, Duration::from_secs(5))? {
        Response::Users(users) if users.is_empty() => println!("No one has set up face unlock."),
        Response::Users(users) => {
            println!("{:<24} {:>7}  UPDATED", "USER", "SAMPLES");
            for u in users {
                let note = if u.model_current { "" } else { "needs setup again (camera or model changed)" };
                println!("{:<24} {:>7}  {:<12} {note}", u.user, u.samples, term::date(u.updated));
            }
        }
        other => return Err(unexpected(other)),
    }
    Ok(ExitCode::SUCCESS)
}

fn cameras() -> Result<ExitCode> {
    match request(&Request::Cameras, Duration::from_secs(10))? {
        Response::Cameras(list) if list.is_empty() => println!("No cameras found."),
        Response::Cameras(list) => {
            for c in &list {
                let mark = if c.selected { Style::Ok.paint("●") } else { " ".into() };
                let ir = if c.infrared { Style::Warn.paint(" [IR]") } else { String::new() };
                println!("{mark} {:<12} {}{ir}", c.path, c.name);
                println!("  {:<12} {}", "", Style::Dim.paint(&c.formats.join(", ")));
            }
            println!("\n● = in use. To choose another, set camera.device in /etc/faceauth/config.toml");
            println!("  (\"auto\", \"ir\" or a path such as \"/dev/video2\"), run: sudo systemctl restart faceauth,");
            println!("  then set up face unlock again (face data from one camera doesn't work with another).");
            if list.iter().any(|c| c.infrared) {
                println!("\n  An IR camera works in the dark if its IR light turns on. If it stays off, see");
                println!("  the \"Infrared cameras\" section of the README.");
            }
        }
        other => return Err(unexpected(other)),
    }
    Ok(ExitCode::SUCCESS)
}

fn benchmark() -> Result<ExitCode> {
    println!("Look at the camera…");
    match request(&Request::Benchmark, Duration::from_secs(60))? {
        Response::Benchmark(b) => {
            println!("Camera       {} ({})", b.camera, b.resolution);
            println!("Recognizer   {}", b.recognizer);
            println!("Capture      {:>7.0} ms  (open, warm-up, frame sequence)", b.capture_ms);
            println!("Detection    {:>7.0} ms  (overlaps capture)", b.detection_ms);
            println!("Recognition  {:>7.0} ms  (all frames; frame 1 overlaps capture, logins stop once the vote is decided)", b.recognition_ms);
            println!("Total        {:>7.0} ms", b.total_ms);
            match b.face_brightness {
                None => println!("Face         {}", Style::Warn.paint("none in view")),
                // An IR camera's own light leaves faces dim but recognisable.
                Some(v) if b.infrared && v < 10.0 => println!(
                    "Face         brightness {v:.0} of 255 — {}",
                    Style::Warn.paint("very dark: the infrared light may be off (see the README)")
                ),
                Some(v) if b.infrared => println!("Face         brightness {v:.0} of 255 (infrared: dim is normal)"),
                Some(v) if v < 40.0 => println!("Face         brightness {v:.0} of 255 — {}", Style::Warn.paint("dark; face unlock may fail")),
                Some(v) => println!("Face         brightness {v:.0} of 255"),
            }
            Ok(ExitCode::SUCCESS)
        }
        other => Err(unexpected(other)),
    }
}

/// Exit codes: 0 the light works (lines printed with --toml; none if it was
/// already on), 1 no setting switched it on, 2 error.
fn ir_setup(toml_only: bool) -> Result<ExitCode> {
    use faceauth_core::IrSetupEvent;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 && std::env::var_os("FACEAUTH_DEV_SKIP_POLKIT").is_none() {
        bail!("this changes camera settings, so run it with sudo: sudo faceauth ir-setup");
    }
    let mut stream = connect(Duration::from_secs(90))?;
    send_message(&mut stream, &Request::SetupIrEmitter)?;
    eprintln!("Switching on the infrared light automatically.");
    eprintln!("Sit in front of the camera, fairly close, and keep still while FaceAuth tries the");
    eprintln!("camera's settings. This takes a few minutes.");
    loop {
        match recv_message::<_, Response>(&mut stream)? {
            Response::IrSetup(IrSetupEvent::Started { total, brightness }) => {
                eprintln!("  {total} settings to try (face brightness now {brightness:.0} of 255)");
            }
            Response::IrSetup(IrSetupEvent::Progress { tried, total }) => {
                eprint!("\r  Tried {tried} of {total}…   ");
                let _ = std::io::stderr().flush();
            }
            Response::IrSetup(IrSetupEvent::Found { controls, before, after }) => {
                eprintln!();
                if controls.is_empty() {
                    eprintln!("{} The infrared light already works (brightness {before:.0}).", Style::Ok.paint("✓"));
                    return Ok(ExitCode::SUCCESS);
                }
                eprintln!("{} Found it: brightness {before:.0} → {after:.0}.", Style::Ok.paint("✓"));
                let found: Vec<ir_emitter::Control> = controls
                    .into_iter()
                    .map(|c| ir_emitter::Control { unit: c.unit, selector: c.selector, control: c.control })
                    .collect();
                let lines = ir_emitter::to_toml(&found);
                if !toml_only {
                    eprintln!("Add these lines to /etc/faceauth/config.toml and run: sudo systemctl restart faceauth");
                    eprintln!("(the installer does this for you)");
                }
                print!("{lines}");
                return Ok(ExitCode::SUCCESS);
            }
            Response::IrSetup(IrSetupEvent::NotFound) => {
                eprintln!();
                eprintln!("{} No setting switched the infrared light on.", Style::Warn.paint("!"));
                return Ok(ExitCode::from(1));
            }
            Response::IrSetup(IrSetupEvent::Failed { message }) => {
                eprintln!();
                eprintln!("{} {message}", Style::Error.paint("error:"));
                return Ok(ExitCode::from(2));
            }
            other => return Err(unexpected(other)),
        }
    }
}

fn ir_emitter_cmd(file: Option<std::path::PathBuf>, toml_only: bool) -> Result<ExitCode> {
    let Some(file) = file.or_else(ir_emitter::tool_config_path) else { bail!("HOME is not set; pass --file") };
    if !file.exists() {
        if !toml_only {
            println!("No linux-enable-ir-emitter settings at {}.", file.display());
            println!();
            println!("Only needed if your infrared camera's light stays off (see \"Infrared cameras\" in");
            println!("the README). To find the command that switches it on:");
            println!("  1. Install linux-enable-ir-emitter: https://github.com/EmixamPP/linux-enable-ir-emitter");
            println!("  2. Run, as yourself (not with sudo):  linux-enable-ir-emitter configure");
            println!("  3. Re-run the FaceAuth installer; it picks up the result.");
        }
        return Ok(ExitCode::FAILURE);
    }
    let found = ir_emitter::find(&file)?;
    let Some(found) = found else {
        if !toml_only {
            println!("{} has no saved settings for a camera connected now.", file.display());
            println!("Run linux-enable-ir-emitter configure (as yourself, not with sudo) and pick your IR camera.");
        }
        return Ok(ExitCode::FAILURE);
    };
    let snippet = ir_emitter::to_toml(&found.controls);
    if toml_only {
        print!("{snippet}");
        return Ok(ExitCode::SUCCESS);
    }
    println!("IR light settings from {}:", file.display());
    println!("  Camera    {}{}", found.node.display(), if found.infrared { " (infrared)" } else { "" });
    println!("  Commands  {}", found.controls.len());
    println!();
    match ir_emitter::configured_in(std::path::Path::new("/etc/faceauth/config.toml")) {
        Some(n) if n > 0 => println!("FaceAuth's config already has {n} IR emitter command(s)."),
        _ => {
            println!("To use them, re-run the FaceAuth installer and choose the infrared camera,");
            println!("or add these lines to /etc/faceauth/config.toml and run: sudo systemctl restart faceauth");
            println!();
            print!("{snippet}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn watch() -> Result<ExitCode> {
    let mut stream = connect(Duration::from_secs(5))?;
    send_message(&mut stream, &Request::Watch)?;
    match recv_message(&mut stream)? {
        Response::Watching => {}
        other => return Err(unexpected(other)),
    }
    stream.set_read_timeout(None)?;
    println!("Watching your face checks. Press Ctrl+C to stop.");
    loop {
        match recv_message::<_, Response>(&mut stream) {
            Ok(Response::AuthEvent(faceauth_core::AuthEvent::Scanning)) => println!("{}  looking…", term::now()),
            Ok(Response::AuthEvent(faceauth_core::AuthEvent::Finished(o))) => println!("{}  {}", term::now(), o.message()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => bail!("the FaceAuth service stopped"),
            Err(e) => return Err(e.into()),
        }
    }
}

fn version() -> Result<ExitCode> {
    println!("faceauth   {VERSION} (protocol {PROTOCOL_VERSION})");
    match ping() {
        Ok((v, p)) => println!("faceauthd  {v} (protocol {p})"),
        Err(_) => println!("faceauthd  not running"),
    }
    Ok(ExitCode::SUCCESS)
}

fn doctor() -> Result<ExitCode> {
    let mut problems = 0;
    let mut check = |ok: Option<bool>, label: &str, detail: String| {
        let mark = match ok {
            Some(true) => Style::Ok.paint("✓"),
            Some(false) => {
                problems += 1;
                Style::Error.paint("✗")
            }
            None => Style::Warn.paint("!"),
        };
        println!("{mark} {label:<16} {detail}");
    };

    match ping() {
        Ok((v, p)) if p == PROTOCOL_VERSION => check(Some(true), "Service", format!("faceauthd {v} is running")),
        Ok((v, p)) => check(Some(false), "Service", format!("faceauthd {v} speaks protocol {p}, this tool {PROTOCOL_VERSION}; reinstall")),
        Err(_) => check(Some(false), "Service", "not running — sudo systemctl start faceauth; logs: journalctl -u faceauth".into()),
    }
    match fetch_status() {
        Ok(s) if s.enrolled && s.model_current => check(Some(true), "Your face", format!("set up ({} samples)", s.samples)),
        Ok(s) if s.enrolled => check(Some(false), "Your face", "camera or face model changed — run: faceauth enroll".into()),
        Ok(_) => check(None, "Your face", "not set up — run: faceauth enroll".into()),
        Err(_) => check(None, "Your face", "unknown (service not reachable)".into()),
    }
    match request(&Request::Cameras, Duration::from_secs(10)) {
        Ok(Response::Cameras(list)) => match list.iter().find(|c| c.selected) {
            Some(c) => check(Some(true), "Camera", format!("{} ({})", c.name, c.path)),
            None if list.is_empty() => check(Some(false), "Camera", "no camera found".into()),
            None => check(Some(false), "Camera", "configured camera not found — see: faceauth cameras".into()),
        },
        _ => check(None, "Camera", "unknown (service not reachable)".into()),
    }
    let models = std::path::Path::new("/usr/share/faceauth/models");
    let has = |stem: &str| [".onnx", "_int8.onnx"].iter().any(|ext| std::fs::metadata(models.join(format!("{stem}{ext}"))).map(|m| m.len() > 0).unwrap_or(false));
    match fetch_status() {
        Ok(s) => check(Some(true), "Models", format!("{} in {}", s.recognizer, models.display())),
        Err(_) if has("det_500m") && (has("arcface") || has("arcface_r50")) => check(None, "Models", format!("found in {} (service not reachable)", models.display())),
        Err(_) => check(Some(false), "Models", format!("missing in {} — re-run the installer", models.display())),
    }
    let services = pam::configured_services();
    if services.is_empty() {
        check(None, "Used for", "nothing yet — re-run the installer and choose where to use face unlock".into());
    } else {
        check(Some(true), "Used for", pam::labels(&services));
    }
    match pam::module_path() {
        Some(p) => check(Some(true), "PAM module", p.display().to_string()),
        None => check(Some(false), "PAM module", "pam_faceauth.so not found next to pam_unix.so".into()),
    }
    let policy = std::path::Path::new("/usr/share/polkit-1/actions/org.faceauth.manage.policy");
    check(Some(policy.exists()), "Polkit policy", if policy.exists() { "installed".into() } else { format!("{} missing — setup cannot ask for your password", policy.display()) });
    // Only relevant when the infrared camera is the one in use.
    if ir_emitter::has_ir_camera() && fetch_status().is_ok_and(|s| s.infrared) {
        let tool = ir_emitter::tool_config_path().filter(|p| p.exists());
        let saved = tool.as_deref().and_then(|p| ir_emitter::find(p).ok().flatten());
        match ir_emitter::configured_in(std::path::Path::new("/etc/faceauth/config.toml")) {
            Some(n) if n > 0 => check(Some(true), "IR light", format!("FaceAuth switches it on ({n} command(s))")),
            _ if saved.is_some() => check(None, "IR light", "linux-enable-ir-emitter settings found but not used — see: faceauth ir-emitter".into()),
            _ => check(None, "IR light", "not switched on by FaceAuth — to search for it: sudo faceauth ir-setup".into()),
        }
    }
    let config = std::path::Path::new("/etc/faceauth/config.toml");
    check(if config.exists() { Some(true) } else { None }, "Config", if config.exists() { config.display().to_string() } else { "not found, using defaults".into() });

    println!();
    if problems == 0 {
        println!("No problems found.");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("{problems} problem(s) found.");
        Ok(ExitCode::FAILURE)
    }
}
