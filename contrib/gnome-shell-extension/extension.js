import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.github.coleleavitt.NiriActivity.Gnome';
const OBJECT_PATH = '/io/github/coleleavitt/NiriActivity/Gnome';
const PROTOCOL_VERSION = 1;
const MAX_TITLE_CODEPOINTS = 4096;
const MAX_IDENTITY_CODEPOINTS = 512;

function sanitizeString(value, maxCodepoints) {
    if (typeof value !== 'string')
        return null;

    // D-Bus strings cannot contain NUL. Other control characters have no useful
    // identity semantics and can corrupt diagnostics produced by clients.
    const sanitized = value
        .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/gu, ' ');
    return [...sanitized].slice(0, maxCodepoints).join('');
}

function optionalString(value, maxCodepoints = MAX_IDENTITY_CODEPOINTS) {
    const sanitized = sanitizeString(value, maxCodepoints);
    return sanitized ? sanitized : null;
}

function uint64Variant(value) {
    if (typeof value === 'bigint' && value >= 0n && value <= 0xffffffffffffffffn)
        return new GLib.Variant('t', value);

    // GJS versions that expose guint64 as Number are safe for Mutter window IDs
    // only while the value remains exactly representable.
    if (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0)
        return new GLib.Variant('t', value);

    return null;
}

class ActiveWindowBridge {
    constructor(extension) {
        this._extension = extension;
        this._generation = 0n;
        this._published = false;
        this._snapshot = {};
        this._exportedObject = null;
        this._displaySignalId = 0;
        this._titleSignalId = 0;
        this._unmanagedSignalId = 0;
        this._focusedWindow = null;
        this._windowTracker = Shell.WindowTracker.get_default();
    }

    get ProtocolVersion() {
        return PROTOCOL_VERSION;
    }

    GetActiveWindow() {
        return [this._generation, this._snapshot];
    }

    start() {
        this._displaySignalId = global.display.connect(
            'notify::focus-window', () => this._syncFocusedWindow());
        this._syncFocusedWindow();
    }

    export(connection) {
        if (this._exportedObject)
            return;

        const xmlFile = this._extension.dir.get_child(
            'io.github.coleleavitt.NiriActivity.Gnome1.xml');
        const [loaded, contents] = xmlFile.load_contents(null);
        if (!loaded)
            throw new Error('failed to load D-Bus interface XML');
        const xml = new TextDecoder().decode(contents);
        this._exportedObject = Gio.DBusExportedObject.wrapJSObject(xml, this);
        this._exportedObject.export(connection, OBJECT_PATH);
    }

    stop() {
        if (this._displaySignalId) {
            global.display.disconnect(this._displaySignalId);
            this._displaySignalId = 0;
        }
        this._disconnectFocusedWindow();
        if (this._exportedObject) {
            this._exportedObject.unexport();
            this._exportedObject = null;
        }
        this._snapshot = {};
        this._windowTracker = null;
        this._extension = null;
    }

    unexport() {
        if (this._exportedObject) {
            this._exportedObject.unexport();
            this._exportedObject = null;
        }
    }

    _disconnectFocusedWindow() {
        if (this._focusedWindow && this._titleSignalId)
            this._focusedWindow.disconnect(this._titleSignalId);
        if (this._focusedWindow && this._unmanagedSignalId)
            this._focusedWindow.disconnect(this._unmanagedSignalId);
        this._titleSignalId = 0;
        this._unmanagedSignalId = 0;
        this._focusedWindow = null;
    }

    _syncFocusedWindow() {
        const window = global.display.get_focus_window();
        if (window !== this._focusedWindow) {
            this._disconnectFocusedWindow();
            this._focusedWindow = window;
            if (window) {
                this._titleSignalId = window.connect('notify::title', () => {
                    if (window === this._focusedWindow)
                        this._publish(window);
                });
                this._unmanagedSignalId = window.connect('unmanaged', () => {
                    if (window === this._focusedWindow) {
                        this._disconnectFocusedWindow();
                        this._publish(null);
                    }
                });
            }
        }
        this._publish(window);
    }

    _publish(window) {
        // Generation zero is the initial no-focus snapshot. Increment before
        // every later publication so a client can de-duplicate snapshot/signal
        // races without missing the initial state.
        if (this._published)
            this._generation = BigInt.asUintN(64, this._generation + 1n);
        else
            this._published = true;
        this._snapshot = window ? this._windowPayload(window) : {};
        if (this._exportedObject) {
            this._exportedObject.emit_signal(
                'ActiveWindowChanged',
                new GLib.Variant('(ta{sv})', [this._generation, this._snapshot]));
        }
    }

    _windowPayload(window) {
        const payload = {};
        const title = sanitizeString(window.get_title(), MAX_TITLE_CODEPOINTS);
        if (title !== null)
            payload.title = new GLib.Variant('s', title);

        const shellAppId = optionalString(
            this._windowTracker.get_window_app(window)?.get_id());
        const sandboxedAppId = optionalString(window.get_sandboxed_app_id());
        const gtkApplicationId = optionalString(window.get_gtk_application_id());
        const wmClass = optionalString(window.get_wm_class());

        let appId = null;
        let identitySource = null;
        if (shellAppId) {
            appId = shellAppId;
            identitySource = 'shell-app-id';
        } else if (sandboxedAppId) {
            appId = sandboxedAppId;
            identitySource = 'sandboxed-app-id';
        } else if (gtkApplicationId) {
            appId = gtkApplicationId;
            identitySource = 'gtk-application-id';
        } else if (wmClass) {
            appId = wmClass;
            identitySource = 'wm-class';
        }

        if (appId) {
            payload['app-id'] = new GLib.Variant('s', appId);
            payload['identity-source'] = new GLib.Variant('s', identitySource);
        }
        if (wmClass)
            payload['wm-class'] = new GLib.Variant('s', wmClass);
        if (sandboxedAppId)
            payload['sandboxed-app-id'] = new GLib.Variant('s', sandboxedAppId);
        if (gtkApplicationId)
            payload['gtk-application-id'] = new GLib.Variant('s', gtkApplicationId);

        const windowId = uint64Variant(window.get_id());
        if (windowId)
            payload['window-id'] = windowId;

        const clientType = Number(window.get_client_type());
        if (Number.isInteger(clientType) && clientType >= 0 && clientType <= 0xffffffff)
            payload['client-type'] = new GLib.Variant('u', clientType);

        return payload;
    }
}

export default class NiriActivityExtension extends Extension {
    enable() {
        this._bridge = new ActiveWindowBridge(this);
        this._bridge.start();
        this._nameOwnerId = Gio.bus_own_name(
            Gio.BusType.SESSION,
            BUS_NAME,
            Gio.BusNameOwnerFlags.NONE,
            null,
            connection => this._bridge?.export(connection),
            () => {
                // Never leave the API exported by only a unique bus name if the
                // well-known name is unavailable or is taken from us.
                this._bridge?.unexport();
                console.error(`${this.uuid}: failed to own ${BUS_NAME}`);
            });
    }

    disable() {
        if (this._nameOwnerId) {
            Gio.bus_unown_name(this._nameOwnerId);
            this._nameOwnerId = 0;
        }
        this._bridge?.stop();
        this._bridge = null;
    }
}
