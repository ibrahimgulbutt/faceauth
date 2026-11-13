// FaceAuth hello face for GNOME Shell.
//
// While FaceAuth checks your face, an animated face looks for you, smiles
// when you're recognized and looks sad when you're not. On the lock screen
// it takes the place of your picture above the password box; for sudo and
// admin prompts it appears in the corner of the screen.
//
// It only listens: faceauthd tells the session about face checks for this
// user over its socket (`"Watch"`), and nothing here can approve or block a
// login. Without faceauthd it does nothing and retries quietly.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const SOCKET_PATH = '/run/faceauth/faceauth.sock';
const MAX_FRAME = 1 << 20;

// Logical pixels.
const LOCK_SIZE = 120;
const POPUP_SIZE = 72;
const POPUP_MARGIN = 24;

const FACE = [0.957, 0.969, 0.98];
const RING = {
    looking: [0.369, 0.784, 0.898],
    ok: [0.298, 0.765, 0.541],
    bad: [0.949, 0.518, 0.361],
};
// Mouth: [left x, end y, control y] in a 100 × 100 box.
const MOUTH = {
    looking: [41, 63, 67],
    ok: [37, 60, 74],
    bad: [41, 67, 59],
};
const TEXT = {
    looking: 'Looking for you…',
    ok: 'Welcome back',
    bad: 'Not recognized',
};
// Outcomes where a face was looked at and not accepted. Others (not set up,
// camera busy, locked out…) just hide the face; PAM shows their message.
const REJECTED = new Set(['NotMatched', 'NotLive', 'NoFace', 'BadLighting']);

const ROUND = 1; // cairo LINE_CAP_ROUND

function quadTo(x0, y0, qx, qy, x1, y1) {
    return [x0 + (2 / 3) * (qx - x0), y0 + (2 / 3) * (qy - y0), x1 + (2 / 3) * (qx - x1), y1 + (2 / 3) * (qy - y1), x1, y1];
}

// Eyes glance left and right while looking: keyframes over 2.4 s.
const GLANCE = [[0, 0], [0.18, 0], [0.30, -4], [0.46, -4], [0.60, 4], [0.76, 4], [0.90, 0], [1, 0]];
function glance(t) {
    const p = (t % 2.4) / 2.4;
    for (let i = 1; i < GLANCE.length; i++) {
        const [p1, v1] = GLANCE[i];
        if (p <= p1) {
            const [p0, v0] = GLANCE[i - 1];
            const k = (p - p0) / (p1 - p0);
            return v0 + (v1 - v0) * (k * k * (3 - 2 * k));
        }
    }
    return 0;
}

// A quick blink every 3.1 s.
function blink(t) {
    const p = (t % 3.1) / 3.1;
    if (p < 0.92)
        return 1;
    return 0.12 + 0.88 * Math.abs(p - 0.95) / 0.05;
}

/** The face, drawn with cairo so it scales cleanly at any size. */
class Face {
    constructor() {
        this.actor = new St.DrawingArea({reactive: false});
        this.actor.connect('repaint', area => this._paint(area));
        this._state = 'looking';
        this._t = 0;
        this._since = 0;
        this._mouth = MOUTH.looking.slice();
        this._ok = 0;
        this._bad = 0;
    }

    setSize(px) {
        this.actor.set_size(px, px);
    }

    setState(state) {
        if (state === 'looking' && this._state !== 'looking') {
            // Start each check from the neutral face.
            this._ok = this._bad = 0;
            this._mouth = MOUTH.looking.slice();
        }
        this._state = state;
        this._since = this._t;
        this.actor.queue_repaint();
    }

    step(dt, animate) {
        this._t += dt;
        const k = animate ? Math.min(1, dt * 10) : 1;
        const target = MOUTH[this._state];
        this._mouth = this._mouth.map((v, i) => v + (target[i] - v) * k);
        this._ok += ((this._state === 'ok' ? 1 : 0) - this._ok) * k;
        this._bad += ((this._state === 'bad' ? 1 : 0) - this._bad) * k;
        this._animate = animate;
        this.actor.queue_repaint();
    }

    _paint(area) {
        const cr = area.get_context();
        try {
            this._draw(cr, ...area.get_surface_size());
        } catch (e) {
            logError(e, 'FaceAuth hello face');
        } finally {
            cr.$dispose();
        }
    }

    _draw(cr, w, h) {
        const animate = this._animate ?? true;
        const t = this._t;
        const since = t - this._since;
        const s = Math.min(w, h) / 100;

        // A small pop on success, a short shake on failure.
        let pop = 1;
        let shake = 0;
        if (animate && this._state === 'ok' && since < 0.45)
            pop = 1 + 0.08 * Math.sin(Math.PI * since / 0.45);
        if (animate && this._state === 'bad' && since < 0.5)
            shake = 6 * Math.sin(since * 38) * (1 - since / 0.5);

        cr.translate(w / 2 + shake * s, h / 2);
        cr.scale(s * pop * 0.92, s * pop * 0.92);
        cr.translate(-50, -50);
        cr.setLineCap(ROUND);

        // Ring: a turning arc while looking, full when there's a result.
        cr.setLineWidth(3.5);
        cr.setSourceRGBA(FACE[0], FACE[1], FACE[2], 0.12);
        cr.arc(50, 50, 44, 0, 2 * Math.PI);
        cr.stroke();
        const ring = RING[this._state];
        cr.setSourceRGBA(ring[0], ring[1], ring[2], 1);
        if (this._state === 'looking') {
            const a = animate ? t * 2 * Math.PI / 1.4 : -Math.PI / 2;
            cr.arc(50, 50, 44, a, a + 2 * Math.PI * 0.224);
        } else {
            cr.arc(50, 50, 44, 0, 2 * Math.PI);
        }
        cr.stroke();

        // Head.
        cr.setSourceRGBA(FACE[0], FACE[1], FACE[2], 1);
        cr.setLineWidth(3);
        cr.arc(50, 50, 32, 0, 2 * Math.PI);
        cr.stroke();

        // Eyes: open (glancing, blinking) → happy arcs, or lowered when sad.
        const looking = animate && this._state === 'looking';
        const gx = looking ? glance(t) : 0;
        const open = (looking ? blink(t) : 1) * (1 - this._ok);
        for (const cx of [39, 61]) {
            if (open > 0.05) {
                cr.save();
                cr.translate(cx + gx, 45 + 2 * this._bad);
                cr.scale(3.6 * (1 - 0.15 * this._bad), 5 * open * (1 - 0.15 * this._bad));
                cr.arc(0, 0, 1, 0, 2 * Math.PI);
                cr.restore();
                cr.fill();
            }
            if (this._ok > 0.05) {
                cr.setSourceRGBA(FACE[0], FACE[1], FACE[2], this._ok);
                cr.moveTo(cx - 5, 47);
                cr.curveTo(...quadTo(cx - 5, 47, cx, 40, cx + 5, 47));
                cr.stroke();
                cr.setSourceRGBA(FACE[0], FACE[1], FACE[2], 1);
            }
        }

        // Sad brows.
        if (this._bad > 0.05) {
            cr.setSourceRGBA(FACE[0], FACE[1], FACE[2], this._bad);
            cr.setLineWidth(2.6);
            cr.moveTo(33, 38);
            cr.lineTo(43, 35);
            cr.stroke();
            cr.moveTo(57, 35);
            cr.lineTo(67, 38);
            cr.stroke();
            cr.setSourceRGBA(FACE[0], FACE[1], FACE[2], 1);
            cr.setLineWidth(3);
        }

        // Mouth.
        const [lx, ey, cy] = this._mouth;
        cr.moveTo(lx, ey);
        cr.curveTo(...quadTo(lx, ey, 50, cy, 100 - lx, ey));
        cr.stroke();
    }
}

/** The face and its caption, placed on the lock screen or in the corner. */
class HelloOverlay {
    constructor() {
        this._face = new Face();
        this._label = new St.Label({style_class: 'faceauth-hello-label', x_align: Clutter.ActorAlign.CENTER});
        this._box = new St.BoxLayout({
            vertical: true,
            style_class: 'faceauth-hello',
            reactive: false,
            opacity: 0,
            visible: false,
        });
        this._box.add_child(this._face.actor);
        this._box.add_child(this._label);
        // Above windows and the lock screen; never takes clicks or focus.
        Main.layoutManager.addTopChrome(this._box, {affectsInputRegion: false});

        this._tickId = 0;
        this._hideId = 0;
        this._safetyId = 0;
        this._last = 0;
        this._hiddenWell = null;
        this._modeId = Main.sessionMode.connect('updated', () => this._modeChanged());
    }

    destroy() {
        Main.sessionMode.disconnect(this._modeId);
        this._clear('_tickId');
        this._clear('_hideId');
        this._clear('_safetyId');
        this._restoreWell();
        Main.layoutManager.removeChrome(this._box);
        this._box.destroy();
    }

    /** Handle one `AuthEvent` from faceauthd. */
    event(event) {
        try {
            if (event === 'Scanning') {
                this._show('looking');
                return;
            }
            if (event && event.Finished !== undefined) {
                if (!this._box.visible)
                    return; // a check that never reached the camera
                const outcome = event.Finished;
                const name = typeof outcome === 'string' ? outcome : Object.keys(outcome)[0];
                if (name === 'Matched')
                    this._result('ok', 1400);
                else if (REJECTED.has(name))
                    this._result('bad', 2200);
                else
                    this._hide();
            }
        } catch (e) {
            logError(e, 'FaceAuth hello face');
        }
    }

    _show(state) {
        this._clear('_hideId');
        const locked = Main.sessionMode.isLocked;
        const scale = St.ThemeContext.get_for_stage(global.stage).scale_factor;
        this._box.remove_style_class_name(locked ? 'faceauth-hello-popup' : 'faceauth-hello-lock');
        this._box.add_style_class_name(locked ? 'faceauth-hello-lock' : 'faceauth-hello-popup');
        this._face.setSize((locked ? LOCK_SIZE : POPUP_SIZE) * scale);
        this._face.setState(state);
        this._label.text = TEXT[state];
        const appearing = !this._box.visible;
        this._box.show();
        this._place();
        if (appearing) {
            this._box.remove_all_transitions();
            this._box.ease({opacity: 255, duration: 200, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
        }
        if (!this._tickId) {
            this._last = GLib.get_monotonic_time();
            this._tickId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 16, () => this._tick());
        }
        // Never stay up if the result doesn't arrive.
        this._clear('_safetyId');
        this._safetyId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 25, () => {
            this._safetyId = 0;
            this._hide();
            return GLib.SOURCE_REMOVE;
        });
    }

    _result(state, holdMs) {
        this._face.setState(state);
        this._label.text = TEXT[state];
        this._place();
        this._clear('_hideId');
        this._hideId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, holdMs, () => {
            this._hideId = 0;
            this._hide();
            return GLib.SOURCE_REMOVE;
        });
    }

    _hide() {
        this._clear('_hideId');
        this._clear('_safetyId');
        if (!this._box.visible)
            return;
        this._box.remove_all_transitions();
        this._box.ease({
            opacity: 0,
            duration: 250,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
            onComplete: () => {
                this._box.hide();
                this._clear('_tickId');
                this._restoreWell();
            },
        });
    }

    _tick() {
        try {
            const now = GLib.get_monotonic_time();
            const dt = Math.min(0.1, (now - this._last) / 1e6);
            this._last = now;
            this._face.step(dt, St.Settings.get().enable_animations);
            // The lock screen's prompt slides in; keep the face on the picture.
            if (Main.sessionMode.isLocked && this._box.visible)
                this._place();
            return GLib.SOURCE_CONTINUE;
        } catch (e) {
            // Never disturb the lock screen: stop animating and step aside.
            logError(e, 'FaceAuth hello face');
            this._tickId = 0;
            this._box.hide();
            this._restoreWell();
            return GLib.SOURCE_REMOVE;
        }
    }

    _place() {
        const [, , width, height] = this._box.get_preferred_size();
        const monitor = Main.layoutManager.primaryMonitor;
        if (!monitor)
            return;
        if (Main.sessionMode.isLocked) {
            // Over the picture above the password box, which is hidden while
            // the face is up. If it isn't there, a fixed spot instead.
            const well = Main.screenShield?._dialog?._authPrompt?._userWell;
            if (well && well.is_mapped()) {
                const [x, y] = well.get_transformed_position();
                const [w, h] = well.get_transformed_size();
                this._box.set_position(Math.round(x + (w - width) / 2), Math.round(y + (h - height) / 2));
                if (this._hiddenWell !== well) {
                    this._restoreWell();
                    this._hiddenWell = well;
                    well.ease({opacity: 0, duration: 150, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
                }
                return;
            }
            this._box.set_position(
                Math.round(monitor.x + (monitor.width - width) / 2),
                Math.round(monitor.y + monitor.height * 0.32 - height / 2));
            return;
        }
        const scale = St.ThemeContext.get_for_stage(global.stage).scale_factor;
        this._box.set_position(
            Math.round(monitor.x + monitor.width - width - POPUP_MARGIN * scale),
            Math.round(monitor.y + monitor.height - height - POPUP_MARGIN * scale));
    }

    _restoreWell() {
        const well = this._hiddenWell;
        this._hiddenWell = null;
        try {
            well?.ease({opacity: 255, duration: 150, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
        } catch (e) {
            // The lock screen is already gone.
        }
    }

    _modeChanged() {
        try {
            if (!Main.sessionMode.isLocked)
                this._restoreWell();
            if (this._box.visible)
                this._place();
        } catch (e) {
            logError(e, 'FaceAuth hello face');
        }
    }

    _clear(field) {
        if (this[field]) {
            GLib.source_remove(this[field]);
            this[field] = 0;
        }
    }
}

/** Follows faceauthd's face-check events for this user, reconnecting as needed. */
class DaemonWatcher {
    constructor(onEvent) {
        this._onEvent = onEvent;
        this._delay = 2;
        this._retryId = 0;
        this._conn = null;
        this._buf = new Uint8Array(0);
    }

    start() {
        this._stopped = false;
        this._connect();
    }

    stop() {
        this._stopped = true;
        this._cancellable?.cancel();
        if (this._retryId) {
            GLib.source_remove(this._retryId);
            this._retryId = 0;
        }
        this._close();
    }

    _connect() {
        if (this._stopped)
            return;
        this._cancellable = new Gio.Cancellable();
        const path = GLib.getenv('FACEAUTH_SOCKET') || SOCKET_PATH;
        const client = new Gio.SocketClient();
        client.connect_async(Gio.UnixSocketAddress.new(path), this._cancellable, (c, res) => {
            try {
                this._conn = c.connect_finish(res);
                this._buf = new Uint8Array(0);
                this._send('"Watch"');
            } catch (e) {
                this._closed();
                return;
            }
            this._read();
        });
    }

    _send(json) {
        const body = new TextEncoder().encode(json);
        const frame = new Uint8Array(4 + body.length);
        new DataView(frame.buffer).setUint32(0, body.length);
        frame.set(body, 4);
        this._conn.get_output_stream().write_all(frame, null);
    }

    _read() {
        const input = this._conn?.get_input_stream();
        if (!input)
            return;
        input.read_bytes_async(4096, GLib.PRIORITY_DEFAULT, this._cancellable, (stream, res) => {
            let chunk;
            try {
                chunk = stream.read_bytes_finish(res).toArray();
            } catch (e) {
                this._closed();
                return;
            }
            if (chunk.length === 0 || !this._take(chunk)) {
                this._closed();
                return;
            }
            this._read();
        });
    }

    // Append bytes and handle every complete length-prefixed JSON message.
    _take(chunk) {
        const merged = new Uint8Array(this._buf.length + chunk.length);
        merged.set(this._buf);
        merged.set(chunk, this._buf.length);
        this._buf = merged;
        while (this._buf.length >= 4) {
            const len = new DataView(this._buf.buffer, this._buf.byteOffset, 4).getUint32(0);
            if (len > MAX_FRAME)
                return false;
            if (this._buf.length < 4 + len)
                break;
            const text = new TextDecoder().decode(this._buf.subarray(4, 4 + len));
            this._buf = this._buf.slice(4 + len);
            let msg;
            try {
                msg = JSON.parse(text);
            } catch (e) {
                return false;
            }
            if (msg === 'Watching')
                this._delay = 2;
            else if (msg && msg.AuthEvent !== undefined)
                this._onEvent(msg.AuthEvent);
            else if (msg && msg.Denied)
                return false; // too many watchers; try again later
        }
        return true;
    }

    _close() {
        try {
            this._conn?.close(null);
        } catch (e) {
            // Already closed.
        }
        this._conn = null;
    }

    _closed() {
        this._close();
        if (this._stopped || this._retryId)
            return;
        this._retryId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, this._delay, () => {
            this._retryId = 0;
            this._connect();
            return GLib.SOURCE_REMOVE;
        });
        this._delay = Math.min(this._delay * 2, 30);
    }
}

/**
 * Development only (FACEAUTH_SHELL_DEMO=1): play the states without
 * faceauthd. FACEAUTH_SHELL_SHOTS=DIR also saves a screenshot of each.
 */
class Demo {
    constructor(overlay) {
        this._overlay = overlay;
        this._step = 0;
        this._count = 0;
        this._id = 0;
        this._next();
    }

    stop() {
        if (this._id)
            GLib.source_remove(this._id);
        this._id = 0;
    }

    _next() {
        const steps = [
            ['Scanning', 2600, 'looking'],
            [{Finished: 'Matched'}, 2400, 'ok'],
            ['Scanning', 2600, 'looking'],
            [{Finished: 'NotMatched'}, 3000, 'bad'],
        ];
        const [event, ms, name] = steps[this._step];
        this._step = (this._step + 1) % steps.length;
        const dialog = Main.screenShield?._dialog;
        if (Main.sessionMode.isLocked && dialog && dialog._activePage !== dialog._promptBox) {
            // Like pressing a key: wake the faded lock screen and open the
            // prompt (once: each time it opens, a new login check starts).
            Main.screenShield._wakeUpScreen?.();
            dialog._showPrompt?.();
        }
        this._overlay.event(event);
        const shots = GLib.getenv('FACEAUTH_SHELL_SHOTS');
        if (shots) {
            const tag = `${String(this._count++).padStart(2, '0')}-${Main.sessionMode.isLocked ? 'lock' : 'popup'}-${name}`;
            GLib.timeout_add(GLib.PRIORITY_DEFAULT, name === 'looking' ? 900 : 500, () => {
                saveScreenshot(`${shots}/${tag}.png`);
                return GLib.SOURCE_REMOVE;
            });
        }
        this._id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
            this._next();
            return GLib.SOURCE_REMOVE;
        });
    }
}

function saveScreenshot(path) {
    try {
        const stream = Gio.File.new_for_path(path).replace(null, false, Gio.FileCreateFlags.NONE, null);
        new Shell.Screenshot().screenshot(false, stream, (shooter, res) => {
            try {
                shooter.screenshot_finish(res);
            } catch (e) {
                logError(e, 'FaceAuth demo screenshot');
            }
            stream.close(null);
        });
    } catch (e) {
        logError(e, 'FaceAuth demo screenshot');
    }
}

export default class FaceAuthHelloExtension extends Extension {
    enable() {
        this._overlay = new HelloOverlay();
        if (GLib.getenv('FACEAUTH_SHELL_DEMO')) {
            this._demo = new Demo(this._overlay);
        } else {
            this._watcher = new DaemonWatcher(event => this._overlay.event(event));
            this._watcher.start();
        }
    }

    disable() {
        this._demo?.stop();
        this._watcher?.stop();
        this._overlay?.destroy();
        this._demo = null;
        this._watcher = null;
        this._overlay = null;
    }
}
