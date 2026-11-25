"""Take a screenshot of the headless session through the Screenshot portal.

  screenshot.py <output.png>"""
import os
import shutil
import sys
from urllib.parse import unquote, urlparse

from gi.repository import GLib

from portal_common import Portal, fail, log

portal = Portal(os.environ.get("TW_APP_ID", "com.typewhisper.TypeWhisper.Spike"))
code, results = portal.request("org.freedesktop.portal.Screenshot", "Screenshot", "(sa{sv})", ("",),
                               {"interactive": GLib.Variant("b", False)}, timeout=60)
if code != 0:
    fail(f"Screenshot response {code}")
path = unquote(urlparse(results["uri"]).path)
shutil.move(path, sys.argv[1])
log("saved", sys.argv[1])
