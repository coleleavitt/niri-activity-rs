# niri-activity GNOME Shell window bridge

This GNOME Shell extension exposes only the currently focused window to
`niri-activity-rs`. It is an ESM extension for GNOME Shell 45 and exports a
read-only, versioned API on the session D-Bus:

- bus name: `io.github.coleleavitt.NiriActivity.Gnome`
- object path: `/io/github/coleleavitt/NiriActivity/Gnome`
- interface: `io.github.coleleavitt.NiriActivity.Gnome1`
- method: `GetActiveWindow() -> (t generation, a{sv} window)`
- signal: `ActiveWindowChanged(t generation, a{sv} window)`

An empty `window` dictionary means that no window is focused. The extension
never exposes a full window list, `Eval`, or window-control methods.

## Privacy and trust boundary

Window titles can contain document names, URLs, and message text. Any process
in the same login session can normally call this session-bus API or receive its
signals. The session bus is not an authorization boundary between processes of
the same user. Install and enable this extension only if that exposure is
acceptable. The extension does not log titles.

## Supported Shell version

`metadata.json` declares only GNOME Shell 45. The ESM extension form and every
Mutter/Shell API used here were source-audited at the GNOME 45 tag. Later GNOME
majors are not declared until they receive a nested-Shell or manual smoke test;
do not disable GNOME's extension-version validation to bypass this gate.

## Install

Run these commands from this directory:

```sh
uuid=niri-activity@coleleavitt.github.io
destination="$HOME/.local/share/gnome-shell/extensions/$uuid"
mkdir -p "$destination"
cp extension.js metadata.json io.github.coleleavitt.NiriActivity.Gnome1.xml "$destination/"
```

Log out and back in after the first install. A Shell restart with `Alt+F2`, `r`
is available only in an X11 session, not a Wayland session.

Installation does **not** enable the extension. Enable it explicitly:

```sh
gnome-extensions enable niri-activity@coleleavitt.github.io
```

Disable it with:

```sh
gnome-extensions disable niri-activity@coleleavitt.github.io
```

Remove it after disabling:

```sh
rm -rf "$HOME/.local/share/gnome-shell/extensions/niri-activity@coleleavitt.github.io"
```

## Test the installed extension

Confirm Shell accepted and enabled it:

```sh
gnome-extensions info niri-activity@coleleavitt.github.io
gnome-extensions list --enabled | grep -Fx niri-activity@coleleavitt.github.io
```

Check the protocol version and current snapshot:

```sh
gdbus call --session \
  --dest io.github.coleleavitt.NiriActivity.Gnome \
  --object-path /io/github/coleleavitt/NiriActivity/Gnome \
  --method org.freedesktop.DBus.Properties.Get \
  io.github.coleleavitt.NiriActivity.Gnome1 ProtocolVersion

gdbus call --session \
  --dest io.github.coleleavitt.NiriActivity.Gnome \
  --object-path /io/github/coleleavitt/NiriActivity/Gnome \
  --method io.github.coleleavitt.NiriActivity.Gnome1.GetActiveWindow
```

Monitor focus and title-only changes:

```sh
gdbus monitor --session \
  --dest io.github.coleleavitt.NiriActivity.Gnome \
  --object-path /io/github/coleleavitt/NiriActivity/Gnome
```

Then switch between native Wayland and XWayland windows, change a focused
window's title, close the focused window, and disable/re-enable the extension.
Expect a strictly advancing generation during one bus-owner lifetime and an
empty dictionary when there is no focused window. The client must invalidate
its observation when the bus owner disappears and start sequence handling
again when a new unique owner appears.

If loading fails, inspect structural extension errors (titles are never logged):

```sh
journalctl --user -b -o cat /usr/bin/gnome-shell
```
