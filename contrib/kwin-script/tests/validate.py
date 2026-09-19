#!/usr/bin/env python3
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
metadata = json.loads((ROOT / "metadata.json").read_text())
assert metadata["KPackageStructure"] == "KWin/Script"
assert metadata["KPlugin"]["Id"] == "niri-activity-rs"
assert metadata["KPlugin"]["EnabledByDefault"] is False
assert metadata["X-Plasma-API"] == "javascript"
script = ROOT / "contents/code/main.js"
assert script.is_file()
text = script.read_text()
for required in (
    "workspace.windowList()", "workspace.windowAdded.connect",
    "workspace.windowRemoved.connect", "workspace.windowActivated.connect",
    "captionChanged", "captionNormalChanged", "windowClassChanged",
    "desktopFileNameChanged", 'send("Snapshot"', 'send("Event"', "new QTimer()",
):
    assert required in text, required
for forbidden in ("wlr-foreign", "zwlr_foreign", "getWindowInfo", "queryWindowInfo"):
    assert forbidden not in text, forbidden
subprocess.run(["node", "--check", str(script)], check=True)
print("KWin package validation passed")
