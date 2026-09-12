# niri-activity-rs KWin script

This is the Plasma 6 KWin-side package for the KDE backend. It observes KWin's
script API and sends bounded JSON snapshots and events to the narrow
`niri-activity-rs` session-bus receiver. It does not enumerate windows over
D-Bus, use a foreign-toplevel protocol, scrape logs, or write window data to a
FIFO.

Window titles are sensitive. The package is disabled by default. Start the
`niri-activity-rs` receiver **before** enabling the script. The receiver must
own `io.github.coleleavitt.NiriActivityRs.KWin` without queueing and validate
that messages came from the current unique owner of `org.kde.KWin`.

## Install

Run from this directory:

```sh
kpackagetool6 --type=KWin/Script --install .
```

For an already installed copy, use:

```sh
kpackagetool6 --type=KWin/Script --upgrade .
```

Then open **System Settings → Window Management → KWin Scripts**, enable
**niri-activity-rs Window Activity Bridge**, and click **Apply**. Using the UI
makes the privacy-sensitive enablement explicit; this package does not edit
`kwinrc` during installation.

## Disable or remove

Disable the entry in **System Settings → Window Management → KWin Scripts** and
click **Apply**. To remove the installed package after disabling it:

```sh
kpackagetool6 --type=KWin/Script --remove niri-activity-rs
```

## Test and diagnose

Run the static/package tests before installation:

```sh
python3 tests/validate.py
node tests/main.test.js
```

The Node test uses a fake Plasma 6 workspace. It checks the initial and
periodic recovery snapshots, full-record change/add/remove events, null focus,
deleted-window removal, field and window-count bounds, overflow recovery, and
property-handler cleanup.

For a one-off test without enabling the installed package, first start the
receiver, then load the source file and run the returned script object. Do not
call the scripting manager's `start()` method; it starts the whole configured
script set.

```sh
SCRIPT="$PWD/contents/code/main.js"
ID="$(qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript \
  "$SCRIPT" niri-activity-rs)"
test "$ID" -ge 0
qdbus6 org.kde.KWin "/Scripting/Script$ID" org.kde.kwin.Script.run
```

Exercise native Wayland and XWayland windows: open/close windows, change a
title, switch focus rapidly, and focus the desktop so the active window is
null. The receiver should accept one generation snapshot, then strictly
monotonic events. It must clear state on malformed data, a sequence gap, a
snapshot timeout, bus loss, or KWin restart rather than retaining stale focus.
Unload the ad-hoc test by plugin name (not numeric ID):

```sh
qdbus6 org.kde.KWin /Scripting org.kde.kwin.Scripting.unloadScript niri-activity-rs
```

Do not run the installed and ad-hoc copies together. A script reload creates a
new generation and sends a complete initial snapshot. While running, the
script also repeats its authoritative snapshot every 15 seconds. This bounded
retry lets a restarted receiver recover even if it missed the first snapshot;
receivers must accept same-generation snapshots as authoritative replacement
barriers without requiring `seq` to advance.

## Wire format

Both receiver methods take one JSON string argument:

- `Snapshot(payload)` contains `protocol`, `generation`, `seq`, `complete`,
  bounded `windows`, and nullable `active_uuid`.
- `Event(payload)` contains `protocol`, `generation`, monotonic `seq`, `event`,
  a complete `window` record (nullable only for null activation), and nullable
  `active_uuid`.

A record contains `uuid`, `pid`, `app_id`, `resource_class`, `resource_name`,
`title`, and `active`. The package sends at most 2,048 snapshot records; IDs
are at most 1,024 UTF-16 code units and titles at most 4,096. An oversized
snapshot is marked `complete: false`, which the receiver must reject and treat
as unknown.
