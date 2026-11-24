"""Optional external GTK image source and wlroots pointer for the private Sway fixture."""
import os
from pathlib import Path
import select
import subprocess
import sys
import time


def drag_image(probe, image, env, launch, command, wait_for):
    env["GDK_BACKEND"] = "wayland"
    source = launch(sys.executable, __file__, image)
    try:
        swaymsg = Path(os.environ.get("SWAY_BIN", "sway")).with_name("swaymsg")
        wait_for("the external GTK drag source", lambda:
                 b"OX drag fixture" in command(str(swaymsg), "-t", "get_tree"))
        commands = (
            "move 95 150 1280 900", "press", "move 104 154 1280 900", "move 135 180 1280 900",
            "move 250 300 1280 900", "move 400 550 1280 900", "move 600 730 1280 900",
            "move 645 763 1280 900", "move 675 765 1280 900", "release",
        )
        with subprocess.Popen([probe, "wayland-pointer"], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, env=env, text=True) as pointer:
            try:
                for instruction in commands:
                    pointer.stdin.write(instruction + "\n")
                    pointer.stdin.flush()
                    if not select.select([pointer.stdout], [], [], 5)[0] or pointer.stdout.readline() != "ok\n":
                        raise RuntimeError(f"Pointer did not acknowledge {instruction!r}")
                    # GTK needs to cross its drag threshold inside the source before
                    # the compositor offers its file to the receiving window.
                    time.sleep(.15)
            finally:
                pointer.stdin.close()
                if pointer.poll() is None:
                    pointer.terminate()
    finally:
        source.terminate()
        source.wait(timeout=4)


def source(image):
    import gi
    gi.require_version("Gtk", "3.0")
    gi.require_version("Gdk", "3.0")
    from gi.repository import Gdk, Gtk

    window = Gtk.Window(title="OX drag fixture")
    window.set_default_size(180, 110)
    surface = Gtk.EventBox()
    surface.add(Gtk.Label(label="Drag image"))
    surface.drag_source_set(Gdk.ModifierType.BUTTON1_MASK,
                            [Gtk.TargetEntry.new("text/uri-list", 0, 0)], Gdk.DragAction.COPY)
    surface.connect("drag-data-get", lambda widget, context, data, kind, when:
                    data.set_uris([image.absolute().as_uri()]))
    window.add(surface)
    window.show_all()
    Gtk.main()


if __name__ == "__main__":
    source(Path(sys.argv[1]))
