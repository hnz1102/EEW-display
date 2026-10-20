#!/usr/bin/env python3
"""Screenshots of the firmware's Web UI, for the README.

The pages are the firmware's own HTML, extracted from ../../src/httpserver.rs
(the login page, the configuration page with its placeholders filled in from
sample settings, and the map page). They are served by a small local mock of the
device API with SYNTHETIC sample data (made-up earthquakes, a made-up network),
and photographed with headless Firefox.

    python3 capture.py [OUT_DIR]          (default ../../docs/images/webui)

Map tiles: the map page loads Google Static Maps tiles in the browser. Each
distinct view is fetched ONCE with the key from GOOGLE_MAPS_API_KEY (else
maps_api_key in ../../cfg.toml) and cached in ./.tile-cache (gitignored), so
re-running costs no API calls. Without a key a grey placeholder is used.
The key never appears in the pictures.

Needs: Firefox (headless) and Pillow. NOTE: the snap Firefox can only write
under your home directory, so OUT_DIR must be there.
"""
import http.server, json, os, re, subprocess, sys, threading, time, urllib.parse, urllib.request, io
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = open(os.path.join(HERE, '../../src/httpserver.rs'), encoding='utf-8').read()
OUT = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, '../../docs/images/webui'))
CACHE = os.path.join(HERE, '.tile-cache')
PORT = 18080

# ── pull the pages out of the firmware source ────────────────────────────────
def rust_unescape(t):
    return t.replace('{{', '{').replace('}}', '}')

def config_html():
    i = SRC.index('fn build_config_html')
    start = SRC.index('r#"', i) + 3
    end = SRC.index('"#,', start)
    tpl = rust_unescape(SRC[start:end])
    args_end = SRC.index('\n    )\n}', end)
    names = re.findall(r'^\s+(\w+)\s*=', SRC[end:args_end], re.M)
    vals = {n: '' for n in names}
    vals.update(dict(
        wifi_ssid='home-ap', wifi_psk='sample', wps_false='checked', dhcp_chk='checked',
        ip_address='192.168.0.200', subnet_mask='255.255.255.0', gateway='192.168.0.1', dns='1.1.1.1',
        syslog_server='192.168.0.140:514', syslog_false='checked', syslog_host_name='esp32s3',
        syslog_app_name='eew-display', web_user='admin', web_password='sample', screen_timeout='300',
        ntp_server1='time.aws.com', ntp_server2='time.google.com', ntp_server3='time.cloudflare.com',
        ntp_server4='ntp.nict.jp', maps_api_key='', mt_road='selected'))
    return re.sub(r'\{(\w+)\}', lambda m: vals.get(m.group(1), m.group(0)), tpl)

def login_html():
    i = SRC.index('const LOGIN_HTML')
    block = SRC[i:SRC.index('\n);', i)]
    return ''.join(re.findall(r'r#"(.*?)"#', block, re.S))

def map_html():
    i = SRC.index('fn build_map_html')
    start = SRC.index('r##"', i) + 4
    return SRC[start:SRC.index('"##', start)]

# ── synthetic device data ────────────────────────────────────────────────────
Q = lambda t, n, la, lo, d, m, sc, inten, ts: dict(time=t, name=n, lat=la, lon=lo, depth=d, mag=m, scale=sc, intensity=inten, tsunami=ts)
QUAKES = [
    Q('2026/10/12 02:48:00', '茨城県南部', 35.70, 140.85, 40, 4.6, 30, '3', 'None'),
    Q('2026/10/11 23:15:00', '福島県沖', 37.50, 141.60, 50, 5.4, 45, '5弱', 'NonEffective'),
    Q('2026/10/11 17:02:00', '大分県中部', 33.20, 131.70, 10, 3.9, 20, '2', 'None'),
    Q('2026/10/11 09:31:00', '栃木県北部', 36.30, 139.90, 60, 3.3, 20, '2', 'None'),
    Q('2026/10/10 21:40:00', '釧路沖', 43.00, 144.30, 90, 5.0, 40, '4', 'None'),
    Q('2026/10/10 06:12:00', '静岡県西部', 34.90, 138.20, 20, 3.1, 10, '1', 'None'),
    Q('2026/10/09 18:55:00', '宮城県沖', 38.20, 142.90, 30, 6.3, 55, '6弱', 'Watch'),
    Q('2026/10/09 04:20:00', '熊本県熊本地方', 32.10, 130.30, 0, 2.8, 10, '1', 'None'),
]
EEW = dict(time='2026/10/12 03:22:10', name='千葉県南東沖', mag=6.1, depth=30, intensity='6弱',
           areas='千葉県北東部, 千葉県南部, 茨城県南部', view=dict(lat=35.30, lon=139.95, zoom=6))

def api(eew, view):
    return {
        '/api/status': dict(ip='192.168.0.210', ssid='home-ap', rssi=-57, battery_voltage=4.72, chip_temp=41.5,
                            heap_int_free=118000, heap_int_min=71000, heap_psram_free=7400000,
                            heap_psram_min=7100000, version='0.2.1', build='2026-10-12 03:21:47 JST'),
        '/api/quakes': dict(enabled=True, ready=True, eew=EEW if eew else None, quakes=QUAKES),
        '/api/mapinfo': dict(has_map=True, version=1, center_lat=view[0], center_lon=view[1], center_valid=True, zoom=view[2]),
        '/api/mapkey': dict(api_key='sample', map_type='roadmap'),
        '/api/tone': dict(running=True, hz=1000, on=False, min=100, max=15000, voice=True, volume=70,
                          notify_eew=True, notify_new=True),
        '/api/boot_log': dict(entries=['#14 PowerOn', '#13 Software', '#12 PowerOn']),
    }

# The page's fetch() calls get their answers from this, synchronously resolved, so
# everything is on the page by the time the screenshot is taken.
def mock_script(data):
    return ('<script>(function(){var D=%s;window.fetch=function(u){var p=String(u).split("?")[0];'
            'var b=D[p];return Promise.resolve(new Response(JSON.stringify(b===undefined?{}:b),'
            '{status:b===undefined?404:200,headers:{"Content-Type":"application/json"}}));};})();</script>'
            % json.dumps(data))

# ── Google tiles, fetched once per view ──────────────────────────────────────
def api_key():
    k = os.environ.get('GOOGLE_MAPS_API_KEY', '').strip()
    if k: return k
    try:
        for line in open(os.path.join(HERE, '../../cfg.toml'), encoding='utf-8'):
            if line.split('=')[0].strip() == 'maps_api_key':
                return line.split('=', 1)[1].strip().strip('"')
    except OSError:
        pass
    return ''

def tile(q):
    center = q['center'][0]; zoom = q['zoom'][0]; size = q['size'][0]; mt = q.get('maptype', ['roadmap'])[0]
    f = os.path.join(CACHE, '%s_%s_%s_%s.jpg' % (center.replace(',', '_'), zoom, size, mt))
    if os.path.exists(f): return open(f, 'rb').read()
    key = api_key()
    if key:
        url = ('https://maps.googleapis.com/maps/api/staticmap?center=%s&zoom=%s&size=%s&format=jpg&maptype=%s&key=%s'
               % (center, zoom, size, mt, key))
        try:
            b = urllib.request.urlopen(url, timeout=20).read()
            os.makedirs(CACHE, exist_ok=True); open(f, 'wb').write(b)
            return b
        except Exception as e:
            print('tile fetch failed:', type(e).__name__)
    w, h = (int(v) for v in size.split('x'))
    buf = io.BytesIO(); Image.new('RGB', (w, h), (205, 210, 195)).save(buf, 'JPEG'); return buf.getvalue()

# ── mock server ──────────────────────────────────────────────────────────────
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        u = urllib.parse.urlparse(self.path); q = urllib.parse.parse_qs(u.query)
        eew = q.get('eew', ['0'])[0] == '1'
        if u.path == '/tile':
            return self.send(200, 'image/jpeg', tile(q))
        if u.path == '/delay':
            time.sleep(1.5)
            return self.send(200, 'image/gif', b'GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;')
        if u.path in ('/', '/login', '/map'):
            if u.path == '/login': html = login_html(); data = {}
            elif u.path == '/': html = config_html(); data = api(eew, (36.5, 137.5, 4))
            else:
                html = map_html().replace('https://maps.googleapis.com/maps/api/staticmap', '/tile')
                data = api(eew, (36.5, 137.5, 4))
            html = html.replace('<head>', '<head>' + mock_script(data), 1)
            # Emoji that default to text presentation get no glyph in the page's font
            # stack; force emoji presentation (VS16). Screenshot-only: the firmware's
            # HTML is untouched.
            for cp in ('1F5E3', '1F5FA', '1F6F0'):
                html = html.replace('&#x%s;' % cp, '&#x%s;&#xFE0F;' % cp).replace('\\u{%s}' % cp, '\\u{%s}\\uFE0F' % cp)
            # Hold the load event back for a moment (a hidden image the server answers
            # late), so the page's async fetch() calls and the map tile are in place
            # when the screenshot is taken.
            html = html.replace('<body>', '<body><img src="/delay" width="1" height="1" style="position:absolute;opacity:0">', 1)
            return self.send(200, 'text/html; charset=utf-8', html.encode())
        self.send(404, 'text/plain', b'not found')
    def send(self, code, ct, body):
        self.send_response(code); self.send_header('Content-Type', ct)
        self.send_header('Content-Length', str(len(body))); self.end_headers(); self.wfile.write(body)

def shoot(url, name, w, h):
    path = os.path.join(OUT, name)
    subprocess.run(['firefox', '--headless', '--screenshot', path, '--window-size=%d,%d' % (w, h), url],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=180)
    return path

def crop_content(path, bg_tol=6, pad=12):
    """Cut the empty page background off the bottom of a tall screenshot."""
    im = Image.open(path).convert('RGB'); w, h = im.size; bg = im.getpixel((2, h - 2)); last = 0
    px = im.load()
    for y in range(h - 1, -1, -1):
        if any(max(abs(a - b) for a, b in zip(px[x, y], bg)) > bg_tol for x in range(0, w, 3)):
            last = y; break
    im.crop((0, 0, w, min(h, last + pad))).save(path)

def main():
    os.makedirs(OUT, exist_ok=True)
    srv = http.server.ThreadingHTTPServer(('127.0.0.1', PORT), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    base = 'http://127.0.0.1:%d' % PORT
    shots = [
        ('/login', '01-login.png', 900, 520, False),
        ('/', '02-config.png', 900, 4200, True),         # True: cut the empty bottom off
        ('/?eew=1', '03-config-eew.png', 900, 4200, 820),   # top of the page: status + quakes with the EEW banner
        ('/map', '04-map-overview.png', 900, 820, True),
        ('/map?lat=37.5&lon=141.6&zoom=8', '05-map-epicenter.png', 900, 820, True),
        ('/map?lat=35.3&lon=139.95&zoom=6&eew=1', '06-map-eew.png', 900, 820, True),
    ]
    for path, name, w, h, crop in shots:
        p = shoot(base + path, name, w, h)
        if crop is True: crop_content(p)
        elif crop:                                        # an int: keep that many rows from the top
            im = Image.open(p); im.crop((0, 0, im.width, crop)).save(p)
        print('wrote', p, Image.open(p).size)
    srv.shutdown()

if __name__ == '__main__':
    main()
