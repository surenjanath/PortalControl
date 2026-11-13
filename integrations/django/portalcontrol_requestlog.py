"""Request log for PortalControl's attack detection (Django).

Django's own server prints one short line per request, without the address
of the computer that sent it. This middleware prints one line per request
with everything PortalControl needs to recognise an attack and to
investigate it: who (address, signed-in user, browser), what (method, path
with its query string, host), and the outcome (status, time taken).

Set-up, two steps:

1. Put this file next to your project's ``manage.py``.
2. Add it to ``MIDDLEWARE`` in ``settings.py``, as the first entry so that it
   also sees requests other middleware refuses::

       MIDDLEWARE = [
           "portalcontrol_requestlog.RequestLogMiddleware",
           ...
       ]

Restart the target. Nothing else is needed: PortalControl reads the lines
from the process output. Request bodies, cookies and headers other than the
ones named above are never printed, so passwords typed into forms are not
logged. Query strings are; do not put secrets in them.

Behind a reverse proxy the proxy's address is the direct sender; the
``X-Forwarded-For`` header is printed as well and PortalControl uses it when
the direct sender is this computer itself.
"""

import json
import sys
import time


class RequestLogMiddleware:
    def __init__(self, get_response):
        self.get_response = get_response

    def __call__(self, request):
        started = time.monotonic()
        status = 500
        try:
            response = self.get_response(request)
            status = response.status_code
            return response
        finally:
            try:
                self._print(request, status, started)
            except Exception:
                # Logging must never break a request.
                pass

    @staticmethod
    def _print(request, status, started):
        meta = request.META
        user = getattr(request, "user", None)
        name = ""
        if user is not None and getattr(user, "is_authenticated", False):
            name = user.get_username()
        entry = {
            "ip": meta.get("REMOTE_ADDR", ""),
            "xff": meta.get("HTTP_X_FORWARDED_FOR", "")[:200],
            "m": request.method,
            "p": request.get_full_path()[:2000],
            "s": status,
            "ms": int((time.monotonic() - started) * 1000),
            "u": name,
            "ua": meta.get("HTTP_USER_AGENT", "")[:300],
            "ref": meta.get("HTTP_REFERER", "")[:300],
            "host": meta.get("HTTP_HOST", "")[:200],
            "len": meta.get("CONTENT_LENGTH") or "0",
        }
        sys.stdout.write("PCREQ " + json.dumps(entry, separators=(",", ":")) + "\n")
        sys.stdout.flush()
