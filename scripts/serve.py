#!/usr/bin/env python3
# minimal static server with http range support — the stdlib `http.server` ignores
# Range and returns the whole file, which would defeat map.html's per-viewport cell
# fetches. serves the repo root so / → map.html. usage: python3 serve.py [port]
import http.server, os, re, socketserver, sys

class H(http.server.SimpleHTTPRequestHandler):
    def send_head(self):
        self._range = None
        rng = self.headers.get('Range')
        path = self.translate_path(self.path)
        if not rng or not os.path.isfile(path):
            return super().send_head()
        m = re.match(r'bytes=(\d+)-(\d*)', rng)
        size = os.path.getsize(path)
        a = int(m.group(1)); b = int(m.group(2)) if m.group(2) else size - 1
        b = min(b, size - 1)
        f = open(path, 'rb'); f.seek(a)
        self._range = b - a + 1
        self.send_response(206)
        self.send_header('Content-Type', self.guess_type(path))
        self.send_header('Content-Range', f'bytes {a}-{b}/{size}')
        self.send_header('Content-Length', str(self._range))
        self.send_header('Accept-Ranges', 'bytes')
        self.end_headers()
        return f

    def copyfile(self, src, dst):
        if self._range is None:
            return super().copyfile(src, dst)
        left = self._range
        while left > 0:
            chunk = src.read(min(1 << 16, left))
            if not chunk: break
            dst.write(chunk); left -= len(chunk)

class S(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True

port = int(sys.argv[1]) if len(sys.argv) > 1 else 8000
print(f"serving {os.getcwd()} on http://localhost:{port}/web/map.html  (Range-capable)")
S(('', port), H).serve_forever()
