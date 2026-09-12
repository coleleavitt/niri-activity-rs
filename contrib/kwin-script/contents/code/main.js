// SPDX-License-Identifier: GPL-3.0-or-later
// KWin Plasma 6 window metadata bridge for niri-activity-rs.

(function () {
    "use strict";

    var PROTOCOL = 1;
    var SERVICE = "io.github.coleleavitt.NiriActivityRs.KWin";
    var PATH = "/io/github/coleleavitt/NiriActivityRs/KWin";
    var INTERFACE = "io.github.coleleavitt.NiriActivityRs.KWin1";
    var MAX_WINDOWS = 2048;
    var MAX_ID = 1024;
    var MAX_TITLE = 4096;
    var RESNAPSHOT_INTERVAL_MS = 15000;
    var MAX_PAYLOAD_CODE_UNITS = 512 * 1024;

    // A fresh generation lets the receiver discard events from a prior KWin
    // script instance. seq is monotonic within this generation.
    var generation = String(Date.now()) + "-" + String(Math.random()).slice(2);
    var seq = 0;
    var tracked = Object.create(null);
    var trackedCount = 0;
    var overflowed = false;

    function bounded(value, maximum) {
        if (value === null || value === undefined) {
            return "";
        }
        var text = String(value);
        return text.length <= maximum ? text : text.slice(0, maximum);
    }

    function uuidOf(window) {
        var uuid = bounded(window.internalId, MAX_ID).toLowerCase();
        if (uuid.length >= 2 && uuid[0] === "{" && uuid[uuid.length - 1] === "}") {
            uuid = uuid.slice(1, -1);
        }
        return uuid;
    }

    function pidOf(window) {
        var pid = Number(window.pid);
        if (!Number.isFinite(pid) || pid < 0) {
            return 0;
        }
        return Math.min(Math.floor(pid), 2147483647);
    }

    function recordOf(window) {
        var desktopFile = bounded(window.desktopFileName, MAX_ID);
        var resourceClass = bounded(window.resourceClass, MAX_ID);
        var resourceName = bounded(window.resourceName, MAX_ID);
        return {
            uuid: uuidOf(window),
            pid: pidOf(window),
            app_id: desktopFile || resourceClass || resourceName,
            resource_class: resourceClass,
            resource_name: resourceName,
            title: bounded(window.captionNormal, MAX_TITLE),
            active: Boolean(window.active)
        };
    }

    function activeUuid() {
        var active = workspace.activeWindow;
        if (active === null || active === undefined || Boolean(active.deleted)) {
            return null;
        }
        var uuid = uuidOf(active);
        return uuid || null;
    }

    function send(method, payload) {
        var encoded = JSON.stringify(payload);
        if (encoded.length > MAX_PAYLOAD_CODE_UNITS) {
            encoded = JSON.stringify({
                protocol: PROTOCOL,
                generation: generation,
                seq: seq,
                complete: false,
                windows: [],
                active_uuid: null
            });
            method = "Snapshot";
            overflowed = true;
        }
        callDBus(SERVICE, PATH, INTERFACE, method, encoded);
    }

    function nextEvent(kind, windowRecord) {
        seq += 1;
        send("Event", {
            protocol: PROTOCOL,
            generation: generation,
            seq: seq,
            event: kind,
            window: windowRecord,
            active_uuid: activeUuid()
        });
    }

    function trackedUuidOf(window) {
        for (var uuid in tracked) {
            if (Object.prototype.hasOwnProperty.call(tracked, uuid) && tracked[uuid].window === window) {
                return uuid;
            }
        }
        return uuidOf(window);
    }

    function disconnectWindow(uuid) {
        var entry = tracked[uuid];
        if (!entry) {
            return null;
        }
        for (var i = 0; i < entry.connections.length; i += 1) {
            var connection = entry.connections[i];
            try {
                connection.signal.disconnect(connection.handler);
            } catch (error) {
                // KWin may already have destroyed the underlying Window.
            }
        }
        delete tracked[uuid];
        trackedCount -= 1;
        return entry.lastRecord;
    }

    function publishChanged(uuid) {
        var entry = tracked[uuid];
        if (!entry || Boolean(entry.window.deleted)) {
            return;
        }
        entry.lastRecord = recordOf(entry.window);
        nextEvent("window_changed", entry.lastRecord);
    }

    function connectSignal(entry, signal) {
        if (!signal || typeof signal.connect !== "function") {
            return;
        }
        var handler = function () { publishChanged(entry.uuid); };
        signal.connect(handler);
        entry.connections.push({signal: signal, handler: handler});
    }

    function trackWindow(window) {
        if (window === null || window === undefined || Boolean(window.deleted)) {
            return null;
        }
        var uuid = uuidOf(window);
        if (!uuid || tracked[uuid]) {
            return tracked[uuid] ? tracked[uuid].lastRecord : null;
        }
        if (trackedCount >= MAX_WINDOWS) {
            return null;
        }
        var entry = {
            uuid: uuid,
            window: window,
            connections: [],
            lastRecord: recordOf(window)
        };
        tracked[uuid] = entry;
        trackedCount += 1;
        connectSignal(entry, window.captionChanged);
        connectSignal(entry, window.captionNormalChanged);
        connectSignal(entry, window.windowClassChanged);
        connectSignal(entry, window.desktopFileNameChanged);
        return entry.lastRecord;
    }

    function initialSnapshot() {
        var windows = workspace.windowList();
        var records = [];
        var complete = windows.length <= MAX_WINDOWS;
        overflowed = !complete;
        var count = Math.min(windows.length, MAX_WINDOWS);
        for (var i = 0; i < count; i += 1) {
            var record = trackWindow(windows[i]);
            if (record !== null) {
                records.push(record);
            } else if (windows[i] !== null && windows[i] !== undefined && !Boolean(windows[i].deleted)) {
                complete = false;
                overflowed = true;
            }
        }
        send("Snapshot", {
            protocol: PROTOCOL,
            generation: generation,
            seq: seq,
            complete: complete,
            windows: records,
            active_uuid: activeUuid()
        });
    }

    workspace.windowAdded.connect(function (window) {
        var record = trackWindow(window);
        if (record !== null) {
            nextEvent("window_added", record);
        } else if (window !== null && window !== undefined && !Boolean(window.deleted)) {
            overflowed = true;
            // Tell the receiver to fail closed if the bounded state cannot
            // represent another live window.
            send("Snapshot", {
                protocol: PROTOCOL,
                generation: generation,
                seq: seq,
                complete: false,
                windows: [],
                active_uuid: null
            });
        }
    });

    workspace.windowRemoved.connect(function (window) {
        // Prefer object identity so no metadata needs to be read from a Window
        // that KWin has already marked deleted.
        var uuid = trackedUuidOf(window);
        var lastRecord = disconnectWindow(uuid);
        if (lastRecord !== null) {
            // Removal carries the complete last known record; callers need not
            // dereference the Window after KWin has marked it deleted.
            lastRecord.active = false;
            nextEvent("window_removed", lastRecord);
        }
        if (overflowed) {
            // Recover automatically once the complete workspace fits again;
            // until then this repeats an explicit incomplete barrier.
            initialSnapshot();
        }
    });

    workspace.windowActivated.connect(function (window) {
        var record = null;
        if (window !== null && window !== undefined && !Boolean(window.deleted)) {
            record = trackWindow(window);
            if (record === null) {
                send("Snapshot", {
                    protocol: PROTOCOL,
                    generation: generation,
                    seq: seq,
                    complete: false,
                    windows: [],
                    active_uuid: null
                });
                return;
            }
            record = recordOf(window);
            tracked[record.uuid].lastRecord = record;
        }
        nextEvent("window_activated", record);
    });

    initialSnapshot();

    // D-Bus calls are asynchronous and may be dropped while the receiver is
    // absent. Periodic authoritative snapshots provide a bounded recovery
    // path after receiver restarts and also retry an overflow barrier.
    var resnapshotTimer = new QTimer();
    resnapshotTimer.setInterval(RESNAPSHOT_INTERVAL_MS);
    resnapshotTimer.timeout.connect(initialSnapshot);
    resnapshotTimer.start();
}());
