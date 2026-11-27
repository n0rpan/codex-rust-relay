"""Private freedesktop notification server for the paired-window acceptance test."""
import json
import sys

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

INTERFACE = "org.freedesktop.Notifications"

class Server(dbus.service.Object):
    def __init__(self, bus):
        super().__init__(dbus.service.BusName(INTERFACE, bus), "/org/freedesktop/Notifications")
        self.count = 0

    @dbus.service.method(INTERFACE, in_signature="", out_signature="as")
    def GetCapabilities(self):
        return ["actions", "body"]

    @dbus.service.method(INTERFACE, in_signature="", out_signature="ssss")
    def GetServerInformation(self):
        return ("OX fixture", "OX", "1", "1.2")

    @dbus.service.method(INTERFACE, in_signature="susssasa{sv}i", out_signature="u")
    def Notify(self, app, replaces, icon, title, body, actions, hints, timeout):
        assert app == "OX Desktop" and hints["desktop-entry"] == "dev.ox.desktop"
        self.count += 1
        if self.count == 1:
            # More than zbus's default signal queue. The client must drain
            # signals from other notifications while it waits for this reply.
            for other in range(1000, 1128):
                self.NotificationClosed(other, 1)
        number = int(replaces) or self.count
        with open(sys.argv[1], "a", encoding="utf-8") as out:
            out.write(json.dumps({"id":number, "title":str(title), "body":str(body)}) + "\n")
        return number

    @dbus.service.signal(INTERFACE, signature="uu")
    def NotificationClosed(self, number, reason):
        pass

    @dbus.service.method(INTERFACE, in_signature="u", out_signature="")
    def CloseNotification(self, number):
        self.NotificationClosed(number, 3)

    @dbus.service.signal(INTERFACE, signature="us")
    def ActionInvoked(self, notification, action):
        pass

    @dbus.service.method("dev.ox.Fixture", in_signature="u", out_signature="")
    def Open(self, notification):
        self.ActionInvoked(notification, "default")

DBusGMainLoop(set_as_default=True)
service = Server(dbus.SessionBus())
GLib.MainLoop().run()
