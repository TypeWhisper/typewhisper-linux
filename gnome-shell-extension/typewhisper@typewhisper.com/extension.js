// TypeWhisper GNOME Shell extension (spike prototype).
//
// GNOME gives apps no always-on-top overlay and no way to learn the focused app.
// This extension does both for the TypeWhisper app and nothing else:
// - it draws the dictation indicator from the app's StateChanged signal;
// - it reports focus changes to the app with FocusChanged, only while the app runs.
// It only talks to the current owner of com.typewhisper.TypeWhisper.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const APP_NAME = 'com.typewhisper.TypeWhisper';
const APP_PATH = '/com/typewhisper/TypeWhisper';
const SHELL_IFACE = 'com.typewhisper.TypeWhisper.Shell';
const PROTOCOL_VERSION = 1;

export default class TypeWhisperExtension extends Extension {
    enable() {
        this._appOwner = null;

        this._indicator = new St.BoxLayout({style_class: 'tw-indicator', visible: false});
        this._dot = new St.Widget({style_class: 'tw-dot', y_align: Clutter.ActorAlign.CENTER});
        this._label = new St.Label({style_class: 'tw-label', y_align: Clutter.ActorAlign.CENTER});
        this._indicator.add_child(this._dot);
        this._indicator.add_child(this._label);
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
        this._indicator.destroy();
        this._indicator = null;
        this._dot = null;
        this._label = null;
        this._appOwner = null;
    }

    _setState(state, text) {
        if (state === 'idle') {
            this._indicator.hide();
            return;
        }
        this._dot.style_class = `tw-dot tw-${state}`;
        this._label.text = text;
        this._indicator.show();
        const monitor = Main.layoutManager.primaryMonitor;
        const [, natWidth] = this._indicator.get_preferred_width(-1);
        const [, natHeight] = this._indicator.get_preferred_height(-1);
        this._indicator.set_position(
            monitor.x + Math.floor((monitor.width - natWidth) / 2),
            monitor.y + monitor.height - natHeight - 64);
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
                } catch (e) {
                    // The app may have quit between the check and the call.
                }
            });
    }
}
