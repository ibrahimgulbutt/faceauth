//! Face Unlock — the FaceAuth setup app. Flows are described in docs/DESIGN.md.

mod client;
mod demo;
mod ring;

use client::{Session, SessionEvent, TestEvent};
use faceauth_core::{pam, AuthOutcome, EnrollMode, Hint, Pose, UserStatus};
use gtk4::prelude::*;
use gtk4::{gio, glib, Align, Box as GBox, Button, Label, Orientation};
use libadwaita as adw;
use libadwaita::prelude::*;
use ring::{FaceRing, Mode};
use std::cell::RefCell;
use std::rc::Rc;

const APP_ID: &str = "org.faceauth.gui";

const CSS: &str = r#"
.hero-badge { background: alpha(@success_color, 0.14); color: @success_color; border-radius: 9999px; padding: 22px; }
.hero-badge.muted { background: alpha(@window_fg_color, 0.07); color: alpha(@window_fg_color, 0.55); }
.capture-title { font-size: 1.55em; font-weight: 800; }
.capture-hint { font-size: 1.05em; color: alpha(@window_fg_color, 0.7); }
.capture-hint.problem { color: @warning_color; font-weight: 600; }
.step-chip { padding: 4px 12px; border-radius: 9999px; font-size: 0.9em; font-weight: 600; color: alpha(@window_fg_color, 0.5); }
.step-chip.current { background: alpha(@accent_bg_color, 0.16); color: @accent_color; }
.step-chip.done { color: @success_color; }
.success-page image.icon { color: @success_color; }
.result-icon.success { color: @success_color; }
.result-icon.failure { color: @error_color; }
.destructive-row { color: @error_color; }
.privacy-note { font-size: 0.9em; color: alpha(@window_fg_color, 0.6); }
"#;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|app| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
            // Development builds: find the app icon in the source tree.
            let assets = concat!(env!("CARGO_MANIFEST_DIR"), "/../assets");
            let theme = gtk4::IconTheme::for_display(&display);
            if !theme.has_icon(APP_ID) && std::path::Path::new(assets).exists() {
                theme.add_search_path(assets);
            }
        }
        // Some displays (remote/off-screen) report no text DPI, which shrinks
        // libadwaita's font-relative layout to a sliver; assume the standard 96.
        if let Some(settings) = gtk4::Settings::default() {
            if settings.gtk_xft_dpi() <= 0 {
                settings.set_gtk_xft_dpi(96 * 1024);
            }
        }
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("win.refresh", &["<Control>r", "F5"]);
    });
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        let app = App::build(app);
        app.window.present();
        dev_hooks(&app);
    });
    app.run()
}

/// Everything the pages need to reach each other.
struct App {
    window: adw::ApplicationWindow,
    nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    home: Home,
    status: RefCell<Option<UserStatus>>,
    session: RefCell<Option<Session>>,
}

impl App {
    fn build(app: &adw::Application) -> Rc<Self> {
        let nav = adw::NavigationView::new();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Face Unlock")
            .default_width(520)
            .default_height(760)
            .width_request(360)
            .height_request(600)
            .content(&toasts)
            .build();

        let home = Home::new();
        nav.add(&home.page);

        let this = Rc::new(Self { window, nav, toasts, home, status: RefCell::new(None), session: RefCell::new(None) });
        this.wire_actions(app);
        this.wire_home();

        // Leaving a capture page (back button, Escape, swipe) cancels its session.
        let weak = Rc::downgrade(&this);
        this.nav.connect_popped(move |_, page| {
            if let Some(app) = weak.upgrade() {
                if page.tag().as_deref() == Some("capture") {
                    app.cancel_session();
                }
            }
        });
        // Everything else holds weak references; the window owns the app state
        // for as long as it is open.
        let keep = this.clone();
        this.window.connect_close_request(move |_| {
            keep.cancel_session();
            glib::Propagation::Proceed
        });
        this.refresh();
        this
    }

    fn wire_actions(self: &Rc<Self>, app: &adw::Application) {
        let quit = gio::SimpleAction::new("quit", None);
        quit.connect_activate(glib::clone!(@weak app => move |_, _| app.quit()));
        app.add_action(&quit);

        let about = gio::SimpleAction::new("about", None);
        about.connect_activate(glib::clone!(@weak self.window as window => move |_, _| show_about(&window)));
        app.add_action(&about);

        let refresh = gio::SimpleAction::new("refresh", None);
        let weak = Rc::downgrade(self);
        refresh.connect_activate(move |_, _| {
            if let Some(app) = weak.upgrade() {
                app.refresh();
            }
        });
        self.window.add_action(&refresh);
    }

    fn wire_home(self: &Rc<Self>) {
        let h = &self.home;
        let weak = Rc::downgrade(self);
        let on = move |f: fn(&Rc<App>)| {
            let weak = weak.clone();
            move || {
                if let Some(app) = weak.upgrade() {
                    f(&app);
                }
            }
        };
        let retry = on(|a| a.refresh());
        h.retry.connect_clicked(move |_| retry());
        let setup = on(|a| a.push_intro());
        h.setup.connect_clicked(move |_| setup());
        let test = on(|a| a.push_test());
        h.test_row.connect_activated(move |_| test());
        let improve = on(|a| a.push_capture(EnrollMode::Append));
        h.improve_row.connect_activated(move |_| improve());
        let again = on(|a| a.push_capture(EnrollMode::Replace));
        h.again_row.connect_activated(move |_| again());
        let banner_again = on(|a| a.push_capture(EnrollMode::Replace));
        h.banner.connect_button_clicked(move |_| banner_again());
        let remove = on(|a| a.confirm_remove());
        h.remove_row.connect_activated(move |_| remove());
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::builder().title(text).timeout(4).build());
    }

    fn refresh(self: &Rc<Self>) {
        self.home.show("loading");
        let rx = client::status();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = rx.recv().await.unwrap_or_else(|_| Err("No reply from the FaceAuth service".into()));
            let Some(app) = weak.upgrade() else { return };
            match result {
                Ok(status) => {
                    app.home.fill(&status);
                    *app.status.borrow_mut() = Some(status);
                }
                Err(e) => {
                    app.home.unavailable.set_description(Some(&format!(
                        "{e}.\nStart it with <tt>sudo systemctl start faceauth</tt>, then try again."
                    )));
                    app.home.show("unavailable");
                    *app.status.borrow_mut() = None;
                }
            }
        });
    }

    fn cancel_session(&self) {
        if let Some(session) = self.session.borrow_mut().take() {
            session.cancel();
        }
    }

    // ── Intro ───────────────────────────────────────────────────────────────

    fn push_intro(self: &Rc<Self>) {
        let tips = adw::PreferencesGroup::new();
        for (icon, title, subtitle) in [
            ("display-brightness-symbolic", "Find good light", "Face a window or lamp. Avoid bright light behind you."),
            ("avatar-default-symbolic", "Show your whole face", "Take off sunglasses, hats, or masks."),
            ("system-users-symbolic", "Be the only one in view", "Only your face should be visible to the camera."),
            ("dialog-password-symbolic", "Confirm with your password", "You'll be asked before the camera turns on."),
        ] {
            let row = adw::ActionRow::builder().title(title).subtitle(subtitle).use_markup(false).build();
            row.add_prefix(&gtk4::Image::builder().icon_name(icon).pixel_size(24).build());
            tips.add(&row);
        }
        let start = pill("Start", true);
        let body = GBox::new(Orientation::Vertical, 24);
        body.append(&heading("Before You Start", "Setup takes about a minute. You'll turn your head slowly in a few directions."));
        body.append(&tips);
        body.append(&start);
        let page = adw::NavigationPage::with_tag(&toolbar(&scrolled(&body)), "Set Up Face Unlock", "intro");
        let weak = Rc::downgrade(self);
        start.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.push_capture(EnrollMode::Replace);
            }
        });
        self.nav.push(&page);
    }

    // ── Capture ─────────────────────────────────────────────────────────────

    fn push_capture(self: &Rc<Self>, mode: EnrollMode) {
        self.cancel_session();
        let view = CaptureView::new(mode);
        let page = adw::NavigationPage::with_tag(
            &view.root,
            if mode == EnrollMode::Append { "Improve Recognition" } else { "Set Up Face Unlock" },
            "capture",
        );
        // Wired once per page; "Try Again" restarts the session in place.
        view.cancel.connect_clicked(glib::clone!(@weak self.nav as nav => move |_| { nav.pop(); }));
        view.back.connect_clicked(glib::clone!(@weak self.nav as nav => move |_| { nav.pop(); }));
        let weak = Rc::downgrade(self);
        let retry_view = Rc::downgrade(&view);
        view.retry.connect_clicked(move |_| {
            if let (Some(app), Some(view)) = (weak.upgrade(), retry_view.upgrade()) {
                app.run_session(mode, view);
            }
        });
        self.nav.push(&page);
        self.run_session(mode, view);
    }

    fn run_session(self: &Rc<Self>, mode: EnrollMode, view: Rc<CaptureView>) {
        self.cancel_session();
        view.reset();
        let (session, rx) = match client::enroll(mode) {
            Ok(pair) => pair,
            Err(e) => return view.fail(&e),
        };
        *self.session.borrow_mut() = Some(session);
        let weak = Rc::downgrade(self);
        let v = view;
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = rx.recv().await {
                match event {
                    SessionEvent::Authorizing => v.authorizing(),
                    SessionEvent::Started(steps) => v.started(&steps),
                    SessionEvent::Frame(frame) => v.ring.set_frame(frame),
                    SessionEvent::Progress { collected, target, pose, hint } => v.progress(collected, target, pose, hint),
                    SessionEvent::Completed(samples) => {
                        let Some(app) = weak.upgrade() else { return };
                        app.session.borrow_mut().take();
                        app.show_done(mode, samples);
                        return;
                    }
                    SessionEvent::Failed(message) => {
                        let Some(app) = weak.upgrade() else { return };
                        // A failure after the user left the page is just the cancelled session.
                        if app.nav.visible_page().and_then(|p| p.tag()).as_deref() == Some("capture") {
                            v.fail(&message);
                        }
                        app.session.borrow_mut().take();
                        return;
                    }
                }
            }
        });
    }

    // ── Done ────────────────────────────────────────────────────────────────

    fn show_done(self: &Rc<Self>, mode: EnrollMode, samples: usize) {
        let try_it = pill("Try It Now", true);
        let done = pill("Done", false);
        let buttons = GBox::new(Orientation::Vertical, 12);
        buttons.set_halign(Align::Center);
        buttons.append(&try_it);
        buttons.append(&done);
        let description = if mode == EnrollMode::Append {
            format!("Face unlock now has {samples} samples to compare with.")
        } else {
            "Next time sudo or the login screen asks for your password, just look at your camera.".to_string()
        };
        let status = adw::StatusPage::builder()
            .icon_name("object-select-symbolic")
            .title(if mode == EnrollMode::Append { "Recognition Improved" } else { "You're All Set" })
            .description(description.as_str())
            .child(&buttons)
            .vexpand(true)
            .build();
        status.add_css_class("success-page");
        let page = adw::NavigationPage::with_tag(&toolbar(&status), "Setup Complete", "done");
        let weak = Rc::downgrade(self);
        try_it.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.push_test();
            }
        });
        let weak = Rc::downgrade(self);
        done.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.nav.pop_to_tag("home");
            }
        });
        self.nav.replace(&[self.home.page.clone(), page]);
        self.refresh();
    }

    // ── Test ────────────────────────────────────────────────────────────────

    fn push_test(self: &Rc<Self>) {
        let Some(user) = self.status.borrow().as_ref().map(|s| s.user.clone()) else {
            return self.toast("The FaceAuth service isn't available");
        };
        let ring = FaceRing::new();
        let title = Label::builder().label("Look at the Camera").css_classes(["capture-title"]).wrap(true).justify(gtk4::Justification::Center).build();
        let detail = Label::builder().label("Checking whether the camera recognizes you").css_classes(["capture-hint"]).wrap(true).justify(gtk4::Justification::Center).build();
        let icon = gtk4::Image::builder().pixel_size(72).visible(false).halign(Align::Center).valign(Align::Center).build();
        let again = pill("Try Again", true);
        let done = pill("Done", false);
        again.set_visible(false);
        let buttons = GBox::new(Orientation::Horizontal, 12);
        buttons.set_halign(Align::Center);
        buttons.append(&again);
        buttons.append(&done);
        let body = GBox::new(Orientation::Vertical, 18);
        body.append(&title);
        body.append(&detail);
        body.append(&ring_frame(&ring, Some(&icon)));
        body.append(&buttons);
        let page = adw::NavigationPage::with_tag(&toolbar(&centered(&body)), "Test Face Unlock", "test");

        let run = Rc::new(move |ring: FaceRing, title: Label, detail: Label, icon: gtk4::Image, again: Button, user: String| {
            ring.set_mode(Mode::Idle);
            ring.set_mode(Mode::Waiting);
            icon.set_visible(false);
            again.set_visible(false);
            title.set_label("Look at the Camera");
            detail.set_label("Starting the camera…");
            let rx = client::test(user);
            glib::MainContext::default().spawn_local(async move {
                while let Ok(event) = rx.recv().await {
                    match event {
                        TestEvent::Scanning => detail.set_label("Checking whether the camera recognizes you"),
                        TestEvent::Outcome(outcome) => {
                            let ok = outcome == AuthOutcome::Matched;
                            ring.set_mode(Mode::Result(ok));
                            icon.set_icon_name(Some(if ok { "object-select-symbolic" } else { "window-close-symbolic" }));
                            icon.set_css_classes(&["result-icon", if ok { "success" } else { "failure" }]);
                            icon.set_visible(true);
                            title.set_label(if ok { "Recognized" } else { "Not Recognized" });
                            detail.set_label(&test_advice(outcome));
                            again.set_visible(!ok);
                        }
                        TestEvent::Failed(e) => {
                            ring.set_mode(Mode::Result(false));
                            title.set_label("Couldn't Test");
                            detail.set_label(&e);
                            again.set_visible(true);
                        }
                    }
                }
            });
        });
        let r = run.clone();
        let (ring2, title2, detail2, icon2, again2, user2) = (ring.clone(), title.clone(), detail.clone(), icon.clone(), again.clone(), user.clone());
        again.connect_clicked(move |_| r(ring2.clone(), title2.clone(), detail2.clone(), icon2.clone(), again2.clone(), user2.clone()));
        let weak = Rc::downgrade(self);
        done.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.nav.pop_to_tag("home");
            }
        });
        if self.nav.visible_page().and_then(|p| p.tag()).as_deref() == Some("done") {
            self.nav.replace(&[self.home.page.clone(), page]);
        } else {
            self.nav.push(&page);
        }
        run(ring, title, detail, icon, again, user);
    }

    // ── Remove ──────────────────────────────────────────────────────────────

    fn confirm_remove(self: &Rc<Self>) {
        let dialog = adw::MessageDialog::new(
            Some(&self.window),
            Some("Remove Face Data?"),
            Some("Face unlock will stop working until you set it up again. Your password keeps working."),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("remove", "_Remove")]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "remove" {
                return;
            }
            let Some(app) = weak.upgrade() else { return };
            app.home.remove_row.set_sensitive(false);
            let rx = client::remove();
            let weak = Rc::downgrade(&app);
            glib::MainContext::default().spawn_local(async move {
                let result = rx.recv().await.unwrap_or_else(|_| Err("No reply from the FaceAuth service".into()));
                let Some(app) = weak.upgrade() else { return };
                app.home.remove_row.set_sensitive(true);
                match result {
                    Ok(()) => app.toast("Face data removed"),
                    Err(e) => app.toast(&e),
                }
                app.refresh();
            });
        });
        dialog.present();
    }
}

/// Development hooks for screenshots and UI work:
/// `FACEAUTH_GUI_START=intro|capture|improve|done|test|remove` opens a page,
/// `FACEAUTH_GUI_SNAPSHOT=<file.png>` saves the window to a PNG every second.
fn dev_hooks(app: &Rc<App>) {
    if let Ok(page) = std::env::var("FACEAUTH_GUI_START") {
        let weak = Rc::downgrade(app);
        glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
            let Some(app) = weak.upgrade() else { return };
            match page.as_str() {
                "intro" => app.push_intro(),
                "capture" => app.push_capture(EnrollMode::Replace),
                "improve" => app.push_capture(EnrollMode::Append),
                "done" => app.show_done(EnrollMode::Replace, 10),
                "test" => app.push_test(),
                "remove" => app.confirm_remove(),
                _ => {}
            }
        });
    }
    if let Ok(path) = std::env::var("FACEAUTH_GUI_SNAPSHOT") {
        // Off-screen displays don't advance animations; show final states.
        if let Some(settings) = gtk4::Settings::default() {
            settings.set_gtk_enable_animations(false);
        }
        let window = app.window.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(1000), move || {
            // Dialogs are separate windows; capture the topmost one.
            let target: gtk4::Window = window
                .application()
                .and_then(|a| a.active_window())
                .unwrap_or_else(|| window.clone().upcast());
            if !snapshot(&target, &path) {
                eprintln!("snapshot failed");
            }
            glib::ControlFlow::Continue
        });
    }
}

fn snapshot(window: &gtk4::Window, path: &str) -> bool {
    let (w, h) = (window.width(), window.height());
    if w == 0 || h == 0 {
        return false;
    }
    let paintable = gtk4::WidgetPaintable::new(Some(window));
    let snap = gtk4::Snapshot::new();
    paintable.snapshot(snap.upcast_ref::<gtk4::gdk::Snapshot>(), w as f64, h as f64);
    let Some(node) = snap.to_node() else {
        eprintln!("snapshot: empty render node");
        return false;
    };
    let Some(renderer) = window.native().map(|n| n.renderer()) else { return false };
    let texture = renderer.render_texture(&node, Some(&gtk4::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
    match texture.save_to_png(path) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("snapshot: {e}");
            false
        }
    }
}

fn test_advice(outcome: AuthOutcome) -> String {
    match outcome {
        AuthOutcome::Matched => "Face unlock is working.".into(),
        AuthOutcome::NotMatched => "Try better lighting, or use Improve Recognition to add samples in this light.".into(),
        AuthOutcome::NoFace => "Make sure your face is in front of the camera and well lit.".into(),
        other => other.message(),
    }
}

// ─── Home ───────────────────────────────────────────────────────────────────

struct Home {
    page: adw::NavigationPage,
    stack: gtk4::Stack,
    unavailable: adw::StatusPage,
    retry: Button,
    setup: Button,
    banner: adw::Banner,
    badge: GBox,
    summary_title: Label,
    summary_detail: Label,
    used_for: adw::PreferencesGroup,
    used_rows: RefCell<Vec<adw::ActionRow>>,
    test_row: adw::ActionRow,
    improve_row: adw::ActionRow,
    again_row: adw::ActionRow,
    remove_row: adw::ActionRow,
    camera: Label,
}

impl Home {
    fn new() -> Self {
        let stack = gtk4::Stack::builder().transition_type(gtk4::StackTransitionType::Crossfade).vexpand(true).build();

        // Loading
        let spinner = gtk4::Spinner::builder().spinning(true).width_request(32).height_request(32).halign(Align::Center).valign(Align::Center).build();
        stack.add_named(&spinner, Some("loading"));

        // Service unavailable
        let retry = pill("Try Again", true);
        let unavailable = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title("FaceAuth Isn't Running")
            .child(&retry)
            .build();
        stack.add_named(&unavailable, Some("unavailable"));

        // Not set up
        let setup = pill("Set Up…", true);
        let note = Label::builder()
            .label("Your face is stored on this computer as encrypted numbers, never as a photo.")
            .css_classes(["privacy-note"])
            .wrap(true)
            .justify(gtk4::Justification::Center)
            .max_width_chars(42)
            .build();
        let setup_box = GBox::new(Orientation::Vertical, 18);
        setup_box.set_halign(Align::Center);
        setup_box.append(&setup);
        setup_box.append(&note);
        let welcome = adw::StatusPage::builder()
            .icon_name(APP_ID)
            .title("Set Up Face Unlock")
            .description("Use your face instead of typing your password for sudo, the login screen, and more.")
            .child(&setup_box)
            .build();
        stack.add_named(&welcome, Some("setup"));

        // Set up
        let badge = GBox::new(Orientation::Vertical, 0);
        badge.add_css_class("hero-badge");
        badge.set_halign(Align::Center);
        badge.append(&gtk4::Image::builder().icon_name("object-select-symbolic").pixel_size(44).build());
        let summary_title = Label::builder().css_classes(["title-1"]).wrap(true).justify(gtk4::Justification::Center).build();
        let summary_detail = Label::builder().css_classes(["dim-label"]).wrap(true).justify(gtk4::Justification::Center).build();
        let hero = GBox::new(Orientation::Vertical, 10);
        hero.set_margin_top(12);
        hero.append(&badge);
        hero.append(&summary_title);
        hero.append(&summary_detail);

        let used_for = adw::PreferencesGroup::builder().title("Used For").build();

        let recognition = adw::PreferencesGroup::builder().title("Recognition").build();
        let test_row = nav_row("camera-web-symbolic", "Test Face Unlock", "Check that the camera recognizes you");
        let improve_row = nav_row("list-add-symbolic", "Improve Recognition", "Add samples in different lighting or with glasses");
        let again_row = nav_row("view-refresh-symbolic", "Set Up Again", "Replace your face data with a fresh setup");
        recognition.add(&test_row);
        recognition.add(&improve_row);
        recognition.add(&again_row);

        let danger = adw::PreferencesGroup::new();
        let remove_row = adw::ActionRow::builder().title("Remove Face Data").subtitle("Turn off face unlock and delete your samples").activatable(true).use_markup(false).build();
        remove_row.add_css_class("destructive-row");
        remove_row.add_prefix(&gtk4::Image::from_icon_name("user-trash-symbolic"));
        danger.add(&remove_row);

        let camera = Label::builder().css_classes(["privacy-note"]).wrap(true).justify(gtk4::Justification::Center).build();

        let content = GBox::new(Orientation::Vertical, 28);
        content.append(&hero);
        content.append(&used_for);
        content.append(&recognition);
        content.append(&danger);
        content.append(&camera);
        stack.add_named(&scrolled(&content), Some("ready"));

        let banner = adw::Banner::builder()
            .title("The camera or face model changed. Set up face unlock again to use it")
            .button_label("Set Up Again")
            .revealed(false)
            .build();

        let header = adw::HeaderBar::new();
        let menu = gio::Menu::new();
        menu.append(Some("_Refresh"), Some("win.refresh"));
        menu.append(Some("_About Face Unlock"), Some("app.about"));
        header.pack_end(&gtk4::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Main Menu").primary(true).build());
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.add_top_bar(&banner);
        view.set_content(Some(&stack));
        let page = adw::NavigationPage::with_tag(&view, "Face Unlock", "home");

        Self {
            page,
            stack,
            unavailable,
            retry,
            setup,
            banner,
            badge,
            summary_title,
            summary_detail,
            used_for,
            used_rows: RefCell::new(Vec::new()),
            test_row,
            improve_row,
            again_row,
            remove_row,
            camera,
        }
    }

    fn show(&self, name: &str) {
        self.stack.set_visible_child_name(name);
        if name != "ready" {
            self.banner.set_revealed(false);
        }
    }

    fn fill(&self, s: &UserStatus) {
        if !s.enrolled {
            self.show("setup");
            return;
        }
        self.summary_title.set_label(if s.model_current { "Face Unlock Is On" } else { "Face Unlock Is Paused" });
        if s.model_current {
            self.badge.remove_css_class("muted");
        } else {
            self.badge.add_css_class("muted");
        }
        let since = glib::DateTime::from_unix_local(s.created as i64)
            .ok()
            .filter(|_| s.created > 0)
            .and_then(|d| d.format("%B %-e, %Y").ok())
            .map(|d| format!(" · set up {d}"))
            .unwrap_or_default();
        self.summary_detail.set_label(&format!("{} samples{since}", s.samples));
        self.banner.set_revealed(!s.model_current);

        let full = s.samples + 6 > s.max_samples;
        self.improve_row.set_sensitive(!full && s.model_current);
        self.improve_row.set_subtitle(if full { "Maximum samples reached — use Set Up Again" } else { "Add samples in different lighting or with glasses" });
        self.test_row.set_sensitive(s.model_current);

        for row in self.used_rows.borrow_mut().drain(..) {
            self.used_for.remove(&row);
        }
        let services = pam::configured_services();
        let mut labels: Vec<&str> = Vec::new();
        for s in &services {
            let l = pam::label(s);
            if !labels.contains(&l) {
                labels.push(l);
            }
        }
        let rows: Vec<adw::ActionRow> = if labels.is_empty() {
            vec![info_row("dialog-information-symbolic", "Not turned on anywhere yet", "Run the installer again and choose sudo, the login screen, and more")]
        } else {
            labels.iter().map(|l| info_row(used_icon(l), &capitalize(l), "")).collect()
        };
        for row in &rows {
            self.used_for.add(row);
        }
        *self.used_rows.borrow_mut() = rows;

        // Say "infrared" unless the camera's own name already does.
        let ir = |c: &str| if s.infrared && !c.contains("IR") { " · infrared" } else { "" };
        self.camera.set_label(&match &s.camera {
            Some(c) => format!("Camera: {c}{} · {} recognition", ir(c), s.recognizer),
            None => format!("No camera found · {} recognition", s.recognizer),
        });
        self.show("ready");
    }
}

fn used_icon(label: &str) -> &'static str {
    match label {
        "sudo" | "su" | "console login" => "utilities-terminal-symbolic",
        "admin prompts" => "dialog-password-symbolic",
        "lock screen" => "system-lock-screen-symbolic",
        _ => "computer-symbolic",
    }
}

/// Sentence case for row titles; command names stay as typed.
fn capitalize(s: &str) -> String {
    if matches!(s, "sudo" | "su") {
        return s.to_string();
    }
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

// ─── Capture view ───────────────────────────────────────────────────────────

struct CaptureView {
    root: adw::ToolbarView,
    stack: gtk4::Stack,
    title: Label,
    hint: Label,
    ring: FaceRing,
    waiting: GBox,
    waiting_label: Label,
    chips: GBox,
    chip_labels: RefCell<Vec<(Pose, Label)>>,
    steps: RefCell<Vec<Pose>>,
    cancel: Button,
    error: adw::StatusPage,
    retry: Button,
    back: Button,
    mode: EnrollMode,
}

impl CaptureView {
    fn new(mode: EnrollMode) -> Rc<Self> {
        let title = Label::builder().css_classes(["capture-title"]).wrap(true).justify(gtk4::Justification::Center).build();
        let hint = Label::builder().css_classes(["capture-hint"]).wrap(true).justify(gtk4::Justification::Center).lines(2).build();
        let ring = FaceRing::new();
        let waiting_label = Label::builder().css_classes(["capture-hint"]).wrap(true).justify(gtk4::Justification::Center).max_width_chars(18).build();
        let waiting = GBox::new(Orientation::Vertical, 8);
        waiting.set_halign(Align::Center);
        waiting.set_valign(Align::Center);
        waiting.append(&gtk4::Image::builder().icon_name("dialog-password-symbolic").pixel_size(40).css_classes(["dim-label"]).build());
        waiting.append(&waiting_label);
        let chips = GBox::new(Orientation::Horizontal, 4);
        chips.set_halign(Align::Center);
        let cancel = pill("Cancel", false);

        let live = GBox::new(Orientation::Vertical, 16);
        live.append(&title);
        live.append(&hint);
        live.append(&ring_frame(&ring, Some(&waiting)));
        live.append(&chips);
        live.append(&cancel);

        let retry = pill("Try Again", true);
        let back = pill("Back", false);
        let buttons = GBox::new(Orientation::Vertical, 12);
        buttons.set_halign(Align::Center);
        buttons.append(&retry);
        buttons.append(&back);
        let error = adw::StatusPage::builder().icon_name("dialog-warning-symbolic").title("Setup Didn't Finish").child(&buttons).build();

        let stack = gtk4::Stack::builder().transition_type(gtk4::StackTransitionType::Crossfade).vexpand(true).build();
        stack.add_named(&centered(&live), Some("live"));
        stack.add_named(&error, Some("error"));
        let root = toolbar(&stack);

        Rc::new(Self {
            root,
            stack,
            title,
            hint,
            ring,
            waiting,
            waiting_label,
            chips,
            chip_labels: RefCell::new(Vec::new()),
            steps: RefCell::new(Vec::new()),
            cancel,
            error,
            retry,
            back,
            mode,
        })
    }

    fn reset(&self) {
        self.stack.set_visible_child_name("live");
        self.ring.set_mode(Mode::Idle);
        self.ring.set_mode(Mode::Waiting);
        self.title.set_label("Getting Ready");
        self.set_hint("Connecting to the camera…", false);
        self.waiting.set_visible(false);
        self.chips.set_visible(false);
    }

    fn authorizing(&self) {
        self.title.set_label("Confirm It's You");
        self.set_hint("Enter your password in the dialog to continue", false);
        self.waiting_label.set_label("Waiting for your password…");
        self.waiting.set_visible(true);
    }

    fn started(&self, steps: &[Pose]) {
        self.waiting.set_visible(false);
        self.ring.set_mode(Mode::Capturing);
        *self.steps.borrow_mut() = steps.to_vec();
        while let Some(child) = self.chips.first_child() {
            self.chips.remove(&child);
        }
        let mut labels = Vec::new();
        for pose in steps {
            if labels.iter().any(|(p, _)| p == pose) {
                continue;
            }
            let chip = Label::builder().label(pose.label()).css_classes(["step-chip"]).build();
            self.chips.append(&chip);
            labels.push((*pose, chip));
        }
        *self.chip_labels.borrow_mut() = labels;
        self.chips.set_visible(true);
        if let Some(first) = steps.first() {
            self.title.set_label(first.instruction());
        }
        self.set_hint("Looking for your face…", false);
    }

    fn progress(&self, collected: usize, target: usize, pose: Pose, hint: Hint) {
        let problem = hint.is_problem();
        self.ring.set_progress(collected, target, pose, problem);
        self.title.set_label(pose.instruction());
        let text = match hint {
            Hint::Good => "Hold that…".to_string(),
            other => other.message().to_string(),
        };
        self.set_hint(&text, problem);

        // A pose chip is done once every sample for that pose is collected.
        let steps = self.steps.borrow();
        for (chip_pose, chip) in self.chip_labels.borrow().iter() {
            let last_index = steps.iter().rposition(|p| p == chip_pose).unwrap_or(0);
            chip.remove_css_class("current");
            chip.remove_css_class("done");
            if collected > last_index {
                chip.add_css_class("done");
            } else if *chip_pose == pose {
                chip.add_css_class("current");
            }
        }
    }

    fn set_hint(&self, text: &str, problem: bool) {
        self.hint.set_label(text);
        if problem {
            self.hint.add_css_class("problem");
        } else {
            self.hint.remove_css_class("problem");
        }
    }

    fn fail(&self, message: &str) {
        self.ring.set_mode(Mode::Idle);
        let keep = match self.mode {
            EnrollMode::Replace => " Any previous setup is unchanged.",
            EnrollMode::Append => " Your existing samples are unchanged.",
        };
        self.error.set_description(Some(&format!("{}.{keep}", message.trim_end_matches('.'))));
        self.stack.set_visible_child_name("error");
    }
}

// ─── Small builders ─────────────────────────────────────────────────────────

fn pill(label: &str, suggested: bool) -> Button {
    let b = Button::builder().label(label).halign(Align::Center).use_underline(true).build();
    b.add_css_class("pill");
    if suggested {
        b.add_css_class("suggested-action");
    }
    b
}

fn heading(title: &str, subtitle: &str) -> GBox {
    let b = GBox::new(Orientation::Vertical, 8);
    b.append(&Label::builder().label(title).css_classes(["title-1"]).wrap(true).justify(gtk4::Justification::Center).build());
    b.append(&Label::builder().label(subtitle).css_classes(["dim-label"]).wrap(true).justify(gtk4::Justification::Center).build());
    b
}

fn nav_row(icon: &str, title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).activatable(true).use_markup(false).build();
    row.add_prefix(&gtk4::Image::from_icon_name(icon));
    row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    row
}

fn info_row(icon: &str, title: &str, subtitle: &str) -> adw::ActionRow {
    // Plain text: labels such as "Login & lock screen" are not markup.
    let row = adw::ActionRow::builder().title(title).use_markup(false).build();
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row.add_prefix(&gtk4::Image::from_icon_name(icon));
    row
}

/// Header bar on top, `content` below.
fn toolbar(content: &impl IsA<gtk4::Widget>) -> adw::ToolbarView {
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(content));
    view
}

/// Scrollable, width-clamped page body (no header bar).
fn scrolled(body: &GBox) -> gtk4::ScrolledWindow {
    body.set_margin_top(24);
    body.set_margin_bottom(32);
    body.set_margin_start(18);
    body.set_margin_end(18);
    let clamp = adw::Clamp::builder().maximum_size(560).child(body).build();
    gtk4::ScrolledWindow::builder().hscrollbar_policy(gtk4::PolicyType::Never).child(&clamp).vexpand(true).build()
}

/// Vertically centred, width-clamped body (no scrolling: it must fit).
fn centered(body: &GBox) -> adw::Clamp {
    body.set_valign(Align::Center);
    body.set_margin_top(12);
    body.set_margin_bottom(24);
    body.set_margin_start(18);
    body.set_margin_end(18);
    adw::Clamp::builder().maximum_size(440).child(body).vexpand(true).build()
}

/// The ring in a square frame, with an optional overlay centred on it.
fn ring_frame(ring: &FaceRing, overlay: Option<&impl IsA<gtk4::Widget>>) -> gtk4::AspectFrame {
    let stack = gtk4::Overlay::new();
    stack.set_child(Some(&ring.widget));
    if let Some(o) = overlay {
        stack.add_overlay(o);
    }
    gtk4::AspectFrame::builder().ratio(1.0).obey_child(false).child(&stack).vexpand(true).build()
}

fn show_about(window: &adw::ApplicationWindow) {
    let about = adw::AboutWindow::builder()
        .transient_for(window)
        .application_name("Face Unlock")
        .application_icon(APP_ID)
        .developer_name("FaceAuth contributors")
        .version(faceauth_core::VERSION)
        .website("https://github.com/ibrahimgulbutt/faceauth")
        .issue_url("https://github.com/ibrahimgulbutt/faceauth/issues")
        .license_type(gtk4::License::MitX11)
        .comments("Face unlock for sudo, the login screen, the lock screen and admin prompts.")
        .build();
    about.add_legal_section(
        "Face models",
        None,
        gtk4::License::Custom,
        Some("The ScrFD and ArcFace models come from InsightFace and are available for non-commercial research purposes only."),
    );
    about.present();
}
