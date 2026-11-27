"""Finder layout in points, aligned to the 640 × 352 background."""
from pathlib import Path

app = Path(defines["app"])
files = [str(app)]
symlinks = {"Applications": "/Applications"}
icon_locations = {app.name: (166, 210), "Applications": (474, 210)}
icon_size = 80
text_size = 13
window_rect = ((160, 160), (640, 352))
background = str(Path(defines["background"]))
format = "UDZO"
filesystem = "HFS+"

# The release job replaces the ad hoc bundle with the signed, stapled bundle.
shrink = False
