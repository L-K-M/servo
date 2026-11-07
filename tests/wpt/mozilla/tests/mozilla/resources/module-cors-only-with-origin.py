# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.


# A cacheable module script that, like some content delivery networks, only allows
# CORS when the request has an `Origin` header and otherwise varies on `Origin`.
def main(request, response):
    headers = [
        (b"Content-Type", b"text/javascript"),
        (b"Cache-Control", b"max-age=3600"),
    ]
    if b"origin" in request.headers:
        headers.append((b"Access-Control-Allow-Origin", b"*"))
    else:
        headers.append((b"Vary", b"Origin"))
    return 200, headers, b"window.moduleRan = true;"
