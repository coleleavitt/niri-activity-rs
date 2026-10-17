#!/usr/bin/env node
"use strict";

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const vm = require("vm");

class Signal {
    constructor() { this.handlers = []; }
    connect(handler) { this.handlers.push(handler); }
    disconnect(handler) {
        const index = this.handlers.indexOf(handler);
        if (index < 0) throw new Error("not connected");
        this.handlers.splice(index, 1);
    }
    emit(value) { for (const handler of [...this.handlers]) handler(value); }
}

function windowOf(overrides = {}) {
    return Object.assign({
        internalId: "11111111-1111-4111-8111-111111111111",
        pid: 42,
        desktopFileName: "org.kde.konsole",
        resourceClass: "konsole",
        resourceName: "konsole",
        captionNormal: "shell",
        active: true,
        deleted: false,
        captionChanged: new Signal(),
        captionNormalChanged: new Signal(),
        windowClassChanged: new Signal(),
        desktopFileNameChanged: new Signal()
    }, overrides);
}

const first = windowOf();
const added = windowOf({
    internalId: "22222222-2222-4222-8222-222222222222",
    pid: Number.MAX_SAFE_INTEGER,
    desktopFileName: "",
    resourceClass: "firefox",
    captionNormal: "x".repeat(5000),
    active: false
});
const workspace = {
    activeWindow: first,
    windows: [first],
    windowList() { return this.windows; },
    windowAdded: new Signal(),
    windowRemoved: new Signal(),
    windowActivated: new Signal()
};
const calls = [];
const timers = [];
class FakeTimer {
    constructor() { this.interval = 0; this.timeout = new Signal(); this.running = false; timers.push(this); }
    setInterval(interval) { this.interval = interval; }
    start() { this.running = true; }
    fire() { assert(this.running); this.timeout.emit(); }
}
const context = {
    workspace,
    callDBus(service, objectPath, iface, method, payload) {
        calls.push({service, objectPath, iface, method, payload: JSON.parse(payload)});
    },
    Date: class extends Date { static now() { return 123456789; } },
    Math,
    QTimer: FakeTimer,
    console
};
const source = fs.readFileSync(path.join(__dirname, "../contents/code/main.js"), "utf8");
vm.runInNewContext(source, context, {filename: "main.js"});

assert.equal(calls.length, 1);
assert.equal(calls[0].method, "Snapshot");
assert.equal(calls[0].payload.protocol, 1);
assert.equal(calls[0].payload.seq, 0);
assert.equal(calls[0].payload.complete, true);
assert.equal(calls[0].payload.windows.length, 1);
assert.equal(calls[0].payload.active_uuid, first.internalId);
assert.equal(timers.length, 1);
assert.equal(timers[0].interval, 15000);
assert.equal(timers[0].running, true);
assert.deepEqual(Object.keys(calls[0].payload.windows[0]).sort(),
    ["active", "app_id", "pid", "resource_class", "resource_name", "title", "uuid"].sort());

first.captionNormal = "edited";
first.captionNormalChanged.emit();
assert.equal(calls.at(-1).payload.event, "window_changed");
assert.equal(calls.at(-1).payload.window.title, "edited");
assert.equal(calls.at(-1).payload.seq, 1);

workspace.windows.push(added);
workspace.windowAdded.emit(added);
assert.equal(calls.at(-1).payload.event, "window_added");
assert.equal(calls.at(-1).payload.window.app_id, "firefox");
assert.equal(calls.at(-1).payload.window.pid, 2147483647);
assert.equal(calls.at(-1).payload.window.title.length, 4096);
assert.equal(added.captionChanged.handlers.length, 1);
assert.equal(added.captionNormalChanged.handlers.length, 1);
assert.equal(added.windowClassChanged.handlers.length, 1);
assert.equal(added.desktopFileNameChanged.handlers.length, 1);

workspace.activeWindow = null;
workspace.windowActivated.emit(null);
assert.equal(calls.at(-1).payload.event, "window_activated");
assert.equal(calls.at(-1).payload.window, null);
assert.equal(calls.at(-1).payload.active_uuid, null);

added.deleted = true;
workspace.windows = [first];
workspace.windowRemoved.emit(added);
assert.equal(calls.at(-1).payload.event, "window_removed");
assert.equal(calls.at(-1).payload.window.uuid, added.internalId);
assert.equal(calls.at(-1).payload.window.active, false);
assert.equal(added.captionChanged.handlers.length, 0);
assert.equal(added.captionNormalChanged.handlers.length, 0);
assert.equal(added.windowClassChanged.handlers.length, 0);
assert.equal(added.desktopFileNameChanged.handlers.length, 0);

// A receiver that missed the first snapshot can recover without reloading KWin.
const lastEventSeq = calls.at(-1).payload.seq;
timers[0].fire();
assert.equal(calls.at(-1).method, "Snapshot");
assert.equal(calls.at(-1).payload.complete, true);
assert.equal(calls.at(-1).payload.seq, lastEventSeq);
assert.equal(calls.at(-1).payload.windows.length, 1);

let expectedEventSeq = 0;
for (const call of calls) {
    assert.equal(call.payload.generation, calls[0].payload.generation);
    if (call.method === "Event") {
        expectedEventSeq += 1;
        assert.equal(call.payload.seq, expectedEventSeq);
    }
}
assert(calls.every(call => call.service === "io.github.coleleavitt.NiriActivityRs.KWin"));
assert(calls.every(call => call.objectPath === "/io/github/coleleavitt/NiriActivityRs/KWin"));
assert(calls.every(call => call.iface === "io.github.coleleavitt.NiriActivityRs.KWin1"));

// An oversized initial state fails closed, then resnapshots completely once a
// removal makes every live window representable.
const manyWindows = [];
for (let i = 0; i < 2049; i += 1) {
    manyWindows.push(windowOf({
        internalId: `00000000-0000-4000-8000-${String(i).padStart(12, "0")}`,
        active: false
    }));
}
const overflowWorkspace = {
    activeWindow: null,
    windows: manyWindows,
    windowList() { return this.windows; },
    windowAdded: new Signal(),
    windowRemoved: new Signal(),
    windowActivated: new Signal()
};
const overflowCalls = [];
const overflowTimers = [];
class OverflowTimer {
    constructor() { this.timeout = new Signal(); overflowTimers.push(this); }
    setInterval(interval) { this.interval = interval; }
    start() {}
}
vm.runInNewContext(source, {
    workspace: overflowWorkspace,
    callDBus(service, objectPath, iface, method, payload) {
        overflowCalls.push({method, payload: JSON.parse(payload)});
    },
    Date: context.Date,
    Math,
    QTimer: OverflowTimer,
    console
}, {filename: "main-overflow.js"});
assert.equal(overflowCalls[0].method, "Snapshot");
assert.equal(overflowCalls[0].payload.complete, false);
assert.equal(overflowCalls[0].payload.windows.length, 2048);
const removedFromOverflow = manyWindows[0];
removedFromOverflow.deleted = true;
overflowWorkspace.windows = manyWindows.slice(1);
overflowWorkspace.windowRemoved.emit(removedFromOverflow);
assert.equal(overflowCalls.at(-1).method, "Snapshot");
assert.equal(overflowCalls.at(-1).payload.complete, true);
assert.equal(overflowCalls.at(-1).payload.windows.length, 2048);

console.log("KWin script behavior tests passed");
