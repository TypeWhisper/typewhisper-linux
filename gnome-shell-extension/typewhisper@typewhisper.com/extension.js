// TypeWhisper GNOME Shell extension.
//
// GNOME gives apps no always-on-top overlay and no way to learn the focused app.
// This extension does both for the TypeWhisper app and nothing else:
// - it draws the dictation indicator from the app's StateChanged signal;
// - it reports focus changes to the app with FocusChanged, only while the app runs.
// It only talks to the current owner of com.typewhisper.TypeWhisper.

import Atk from 'gi://Atk';
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import St from 'gi://St';

import * as Animation from 'resource:///org/gnome/shell/ui/animation.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const APP_NAME = 'com.typewhisper.TypeWhisper';
const APP_PATH = '/com/typewhisper/TypeWhisper';
const SHELL_IFACE = 'com.typewhisper.TypeWhisper.Shell';
const PROTOCOL_VERSION = 1;

// Distance from the bottom of the work area, and the pulse of the recording dot.
const EDGE_INSET = 20;
const PULSE_MS = 1000;
const STATES = ['recording', 'processing', 'inserting', 'error'];

export default class TypeWhisperExtension extends Extension {
    enable() {
        this._appOwner = null;
        this._timerId = 0;
        this._recordingSince = 0;

        // Left: what is happening (dot, spinner or icon) and, while recording,
        // the elapsed time. Right: the step the app names while it works.
        this._indicator = new St.BoxLayout({
            style_class: 'tw-indicator',
            visible: false,
            accessible_role: Atk.Role.NOTIFICATION,
        });
        this._dot = new St.Widget({style_class: 'tw-dot', y_align: Clutter.ActorAlign.CENTER});
        this._spinner = new Animation.Spinner(16);
        this._spinner.y_align = Clutter.ActorAlign.CENTER;
        this._spinner.play();
        this._icon = new St.Icon({style_class: 'tw-icon', y_align: Clutter.ActorAlign.CENTER});
        this._timer = new St.Label({style_class: 'tw-timer', y_align: Clutter.ActorAlign.CENTER});
        this._message = new St.Label({
            style_class: 'tw-message',
            y_align: Clutter.ActorAlign.CENTER,
            x_expand: true,
        });
        this._spacer = new St.Widget({x_expand: true});
        this._phase = new St.Label({style_class: 'tw-phase', y_align: Clutter.ActorAlign.CENTER});
        for (const child of [this._dot, this._spinner, this._icon, this._timer, this._message,
            this._spacer, this._phase])
            this._indicator.add_child(child);
        // Top chrome stays above normal windows, like the shell's own OSDs.
        Main.layoutManager.addTopChrome(this._indicator);

        this._watchId = Gio.bus_watch_name(Gio.BusType.SESSION, APP_NAME,
            Gio.BusNameWatcherFlags.NONE,
            (_c, _name, owner) => {
                this._appOwner = owner;
                this._reportFocus();
            },
            () => {
                this._appOwner = null;
                this._setState('idle', '');
            });

        // Passing the well-known name as sender makes GDBus accept only signals
        // from its current owner.
        this._stateSub = Gio.DBus.session.signal_subscribe(APP_NAME, SHELL_IFACE, 'StateChanged',
            APP_PATH, null, Gio.DBusSignalFlags.NONE,
            (_c, _sender, _path, _iface, _signal, params) => {
                const [version, state, text] = params.deep_unpack();
                if (version === PROTOCOL_VERSION)
                    this._setState(state, text);
            });

        this._focusId = global.display.connect('notify::focus-window', () => this._reportFocus());
    }

    disable() {
        global.display.disconnect(this._focusId);
        Gio.DBus.session.signal_unsubscribe(this._stateSub);
        Gio.bus_unwatch_name(this._watchId);
        this._stopTimer();
        this._indicator.destroy();
        this._indicator = null;
        this._dot = null;
        this._spinner = null;
        this._icon = null;
        this._timer = null;
        this._message = null;
        this._spacer = null;
        this._phase = null;
        this._appOwner = null;
    }

    _setState(state, text) {
        this._stopTimer();
        this._dot.remove_all_transitions();
        if (state === 'idle') {
            this._indicator.hide();
            return;
        }
        // A state from a newer app shows its text, like a step of processing.
        const known = STATES.includes(state);
        const recording = state === 'recording';
        const feedback = state === 'inserting' || state === 'error';

        this._indicator.style_class = `tw-indicator tw-state-${known ? state : 'processing'}`;
        this._indicator.accessible_name = text;
        this._dot.visible = recording;
        this._spinner.visible = !recording && !feedback;
        this._icon.visible = feedback;
        this._icon.icon_name = state === 'error' ? 'dialog-error-symbolic' : 'object-select-symbolic';
        this._icon.style_class = `tw-icon tw-${state}`;
        this._timer.visible = recording;
        this._timer.text = '0:00';
        this._message.visible = state === 'error';
        this._message.text = text;
        this._spacer.visible = state !== 'error';
        this._phase.visible = !recording && !feedback;
        this._phase.text = text;

        this._indicator.show();
        const monitor = Main.layoutManager.primaryMonitor;
        const area = Main.layoutManager.getWorkAreaForMonitor(monitor.index);
        const [, natWidth] = this._indicator.get_preferred_width(-1);
        const [, natHeight] = this._indicator.get_preferred_height(natWidth);
        this._indicator.set_position(
            area.x + Math.floor((area.width - natWidth) / 2),
            area.y + area.height - natHeight - EDGE_INSET);

        // The pulse only runs on a mapped actor, so it starts after show().
        if (recording)
            this._startTimer();
    }

    _startTimer() {
        this._recordingSince = GLib.get_monotonic_time();
        this._updateTimer();
        this._timerId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
            this._updateTimer();
            return GLib.SOURCE_CONTINUE;
        });

        this._dot.opacity = 255;
        if (St.Settings.get().enable_animations) {
            this._dot.ease({
                opacity: 140,
                duration: PULSE_MS,
                mode: Clutter.AnimationMode.EASE_IN_OUT_QUAD,
                repeatCount: -1,
                autoReverse: true,
            });
        }
    }

    _stopTimer() {
        if (this._timerId) {
            GLib.source_remove(this._timerId);
            this._timerId = 0;
        }
    }

    _updateTimer() {
        const seconds = Math.floor((GLib.get_monotonic_time() - this._recordingSince) / 1000000);
        const text = `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
        if (this._timer.text !== text)
            this._timer.text = text;
    }

    _reportFocus() {
        if (!this._appOwner)
            return;
        const window = global.display.focus_window;
        const app = window ? Shell.WindowTracker.get_default().get_window_app(window) : null;
        const appId = app?.get_id() ?? window?.get_wm_class() ?? '';
        const title = window?.get_title() ?? '';
        Gio.DBus.session.call(this._appOwner, APP_PATH, SHELL_IFACE, 'FocusChanged',
            new GLib.Variant('(uss)', [PROTOCOL_VERSION, appId, title]), null,
            Gio.DBusCallFlags.NO_AUTO_START, 1000, null,
            (connection, result) => {
                try {
                    connection.call_finish(result);
                } catch {
                    // The app may have quit between the check and the call.
                }
            });
    }
}
