import time


def main(request, response):
    # Send the headers now and the body a moment later, so the frame's new document
    # exists, and is reachable from its parent, before any of its body has arrived.
    response.headers.set(b"Content-Type", b"text/html")
    response.headers.set(b"Cache-Control", b"no-store")
    response.write_status_headers()
    time.sleep(1)
    response.writer.write_content(b"<!doctype html><p>body</p>")
