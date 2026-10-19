#!/usr/bin/env python3
"""Generate voice.bin: the spoken parts the firmware strings together for EEW /
new-earthquake announcements (see ../../src/voice.rs).

  GOOGLE_TTS_API_KEY=... python3 gen_voice.py [--out voice.bin] [--voice ja-JP-Neural2-B]
  python3 gen_voice.py --list-voices      # voices this key can use
  python3 gen_voice.py --dry-run          # no API: tone bursts, to test the pipeline

Each part is synthesized once with Google Cloud Text-to-Speech (REST, LINEAR16
16 kHz) and cached in ./cache, so re-running costs no API calls unless the text
changed. Needs the "Cloud Text-to-Speech API" enabled for the key. Output is
mono 16 kHz IMA-ADPCM; flash it to the `voice` partition:

  esptool.py --chip esp32s3 write_flash 0x700000 voice.bin
  (or: espflash write-bin 0x700000 voice.bin)

voice.bin layout (little endian):
  "VOX1", u32 count, u32 sample_rate,
  count x { u8 keylen, key(utf-8), u32 offset (from file start), u32 nsamples },
  clip data: { i16 predictor, u8 step index, u8 0, packed nibbles (low first) }
"""
import argparse, base64, hashlib, json, math, os, re, struct, sys, time, urllib.request, urllib.error

HERE = os.path.dirname(os.path.abspath(__file__))
RATE = 16000

# ── IMA ADPCM ────────────────────────────────────────────────────────────────
STEP = [7,8,9,10,11,12,13,14,16,17,19,21,23,25,28,31,34,37,41,45,50,55,60,66,73,80,88,97,107,118,130,143,157,173,190,209,230,253,279,307,337,371,408,449,494,544,598,658,724,796,876,963,1060,1166,1282,1411,1552,1707,1878,2066,2272,2499,2749,3024,3327,3660,4026,4428,4871,5358,5894,6484,7132,7845,8630,9493,10442,11487,12635,13899,15289,16818,18500,20350,22385,24623,27086,29794,32767]
INDEX = [-1,-1,-1,-1,2,4,6,8]

def adpcm_encode(samples):
    pred = samples[0] if samples else 0
    idx = 0
    out = bytearray(struct.pack('<hBB', pred, idx, 0))
    nib = []
    p, i = pred, idx
    for s in samples[1:] + ([samples[-1]] if samples else []):
        step = STEP[i]
        diff = s - p
        code = 0
        if diff < 0:
            code = 8; diff = -diff
        d = step >> 3
        if diff >= step: code |= 4; diff -= step; d += step
        step >>= 1
        if diff >= step: code |= 2; diff -= step; d += step
        step >>= 1
        if diff >= step: code |= 1; d += step
        p = max(-32768, min(32767, p - d if code & 8 else p + d))
        i = max(0, min(88, i + INDEX[code & 7]))
        nib.append(code)
    for k in range(0, len(nib), 2):
        out.append(nib[k] | ((nib[k + 1] if k + 1 < len(nib) else 0) << 4))
    return bytes(out)

def adpcm_decode(data, n):
    pred, idx, _ = struct.unpack('<hBB', data[:4])
    out = [pred]
    for k in range(n - 1):
        b = data[4 + k // 2]
        code = (b >> 4) if k & 1 else (b & 15)
        step = STEP[idx]
        d = step >> 3
        if code & 4: d += step
        if code & 2: d += step >> 1
        if code & 1: d += step >> 2
        pred = max(-32768, min(32767, pred - d if code & 8 else pred + d))
        idx = max(0, min(88, idx + INDEX[code & 7]))
        out.append(pred)
    return out

# ── Japanese readings ────────────────────────────────────────────────────────
ONES = ['', 'いち', 'に', 'さん', 'よん', 'ご', 'ろく', 'なな', 'はち', 'きゅう']
def yomi(n):
    """Hiragana reading of 0..799 (as used for depths in km)."""
    if n == 0: return 'れい'
    s = ''
    h, n = divmod(n, 100)
    if h: s += {1:'ひゃく',3:'さんびゃく',6:'ろっぴゃく',8:'はっぴゃく'}.get(h, ONES[h] + 'ひゃく')
    t, o = divmod(n, 10)
    if t: s += ('' if t == 1 else ONES[t]) + 'じゅう'
    if o: s += ONES[o]
    return s

INTENS = [  # key, text, reading
    ('1', '震度1', 'しんどいち'), ('2', '震度2', 'しんどに'), ('3', '震度3', 'しんどさん'),
    ('4', '震度4', 'しんどよん'), ('5-', '震度5弱', 'しんどごじゃく'), ('5+', '震度5強', 'しんどごきょう'),
    ('6-', '震度6弱', 'しんどろくじゃく'), ('6+', '震度6強', 'しんどろっきょう'), ('7', '震度7', 'しんどなな'),
]

# (key, ssml-inner, tail pause seconds)
def phrases():
    P = []
    def add(key, inner, pause=0.12): P.append((key, inner, pause))
    add('p:eew',  '緊急地震速報。', 0.35)
    add('p:info', '地震情報。', 0.35)
    add('p:hypo', '震源は', 0.08)
    add('p:desu', 'です。', 0.35)
    add('p:mag',  'マグニチュード', 0.08)
    add('p:point', '<sub alias="てん">点</sub>', 0.05)
    add('p:max',  '最大', 0.05)
    add('p:forecast', 'と予測されます。', 0.4)
    add('p:warn', '強い揺れに警戒してください。', 0.3)
    add('p:depth', '深さ', 0.08)
    add('p:km', 'キロ。', 0.35)
    add('p:shallow', 'ごく浅い。', 0.35)
    add('p:end', '以上です。', 0.2)
    add('p:no_tsunami', 'この地震による津波の心配はありません。', 0.3)
    add('p:minor_tsunami', '若干の海面変動があるかもしれませんが、被害の心配はありません。', 0.3)
    add('p:tsunami', '津波に関する情報に注意してください。', 0.3)
    for k, t, y in INTENS:
        add('i:' + k, '<sub alias="%s">%s</sub>。' % (y, t), 0.3)
    for n in range(100):
        add('n:%d' % n, '<sub alias="%s">%d</sub>' % (yomi(n), n), 0.05)
    for h in range(100, 800, 100):
        add('h:%d' % h, '<sub alias="%s">%d</sub>' % (yomi(h), h), 0.03)
    seen = set()
    for line in open(os.path.join(HERE, 'tokens.txt'), encoding='utf-8'):
        line = line.rstrip('\n')
        if not line.strip() or line.lstrip().startswith('#'): continue
        parts = line.split('\t')
        tok = parts[0].strip()
        if tok in seen: continue
        seen.add(tok)
        inner = '<sub alias="%s">%s</sub>' % (parts[1].strip(), tok) if len(parts) > 1 and parts[1].strip() else tok
        add('t:' + tok, inner, 0.04)
    return P

# ── synthesis ────────────────────────────────────────────────────────────────
def plain(ssml):
    """SSML <sub alias="よみ">表記</sub> -> よみ (plain text with the same reading)."""
    return re.sub(r'<sub alias="([^"]*)">[^<]*</sub>', r'\1', ssml)

def tts_request(inp, voice, rate, key):
    body = json.dumps({
        'input': inp,
        'voice': {'languageCode': 'ja-JP', 'name': voice},
        'audioConfig': {'audioEncoding': 'LINEAR16', 'sampleRateHertz': RATE, 'speakingRate': rate},
    }).encode()
    req = urllib.request.Request(
        'https://texttospeech.googleapis.com/v1/text:synthesize?key=' + key,
        data=body, headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=30) as r:
        wav = base64.b64decode(json.load(r)['audioContent'])
    n = (len(wav) - 44) // 2   # strip the 44-byte WAV header
    return list(struct.unpack('<%dh' % n, wav[44:44 + n * 2]))

def surface(ssml):
    """SSML <sub alias="よみ">表記</sub> -> 表記."""
    return re.sub(r'<sub alias="[^"]*">([^<]*)</sub>', r'\1', ssml)

_use_plain = False
def synth_google(clipkey, ssml, voice, rate, key):
    """Try the SSML, then the same reading in other spellings. A voice sometimes
    rejects one particular input (HTTP 400) while accepting its neighbours, so each
    variant is tried twice. Returns None if every variant fails."""
    global _use_plain
    p, w = plain(ssml), surface(ssml)
    variants = [('plain', {'text': p}), ('plain+。', {'text': p.rstrip('。') + '。'}),
                ('surface', {'text': w}), ('plain+、', {'text': p.rstrip('。') + '、'})]
    if not _use_plain:
        variants.insert(0, ('ssml', {'ssml': '<speak>%s</speak>' % ssml}))
    last = None
    for name, inp in variants:
        for attempt in range(2):
            try:
                s = tts_request(inp, voice, rate, key)
                if name == 'plain' and not _use_plain:
                    print('note: %s rejected SSML; using plain text from here on' % voice)
                    _use_plain = True
                if name not in ('ssml', 'plain'):
                    print('note: part %r only accepted as %s: %s' % (clipkey, name, json.dumps(inp, ensure_ascii=False)))
                return s
            except urllib.error.HTTPError as e:
                last = (e.code, e.read().decode('utf-8', 'replace')[:200])
                if e.code in (401, 403, 429) or e.code >= 500 and attempt:
                    sys.exit('TTS request failed: HTTP %d %s' % last)
            except urllib.error.URLError as e:
                last = (0, str(e))
            time.sleep(1.0)
    print('WARNING: part %r (%s) rejected in every form: HTTP %s' % (clipkey, plain(ssml), last))
    return None

def list_voices(key):
    url = 'https://texttospeech.googleapis.com/v1/voices?languageCode=ja-JP&key=' + key
    try:
        with urllib.request.urlopen(url, timeout=30) as r:
            for v in json.load(r).get('voices', []):
                print(v['name'], v.get('ssmlGender', ''))
    except urllib.error.HTTPError as e:
        sys.exit('voices.list failed: HTTP %d %s' % (e.code, e.read().decode('utf-8', 'replace')[:300]))

def synth_dry(key, ssml):
    f = 300 + int(hashlib.md5(key.encode()).hexdigest()[:4], 16) % 1500
    n = int(RATE * (0.25 + 0.04 * len(ssml) % 0.6))
    return [int(9000 * math.sin(2 * math.pi * f * i / RATE) * min(1, i / 160, (n - i) / 160)) for i in range(n)]

def trim_norm(s, pause):
    thr = 300
    a = 0
    while a < len(s) and abs(s[a]) < thr: a += 1
    b = len(s)
    while b > a and abs(s[b - 1]) < thr: b -= 1
    s = s[max(0, a - int(0.02 * RATE)):b]
    peak = max((abs(x) for x in s), default=1) or 1
    g = 0.9 * 32767 / peak
    s = [int(x * g) for x in s]
    return s + [0] * int(pause * RATE)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--out', default=os.path.join(HERE, 'voice.bin'))
    ap.add_argument('--voice', default='ja-JP-Neural2-B')
    ap.add_argument('--rate', type=float, default=1.0, help='speaking rate')
    ap.add_argument('--skip-failed', action='store_true', help='leave out parts the voice rejects instead of stopping')
    ap.add_argument('--list-voices', action='store_true', help='print the ja-JP voices the key can use, and exit')
    ap.add_argument('--dry-run', action='store_true', help='tone bursts instead of speech (no API)')
    a = ap.parse_args()
    key = os.environ.get('GOOGLE_TTS_API_KEY', '')
    if a.list_voices:
        if not key: sys.exit('Set GOOGLE_TTS_API_KEY.')
        list_voices(key); return
    if not a.dry_run and not key:
        sys.exit('Set GOOGLE_TTS_API_KEY (Cloud Text-to-Speech API enabled), or use --dry-run.')
    cache = os.path.join(HERE, 'cache'); os.makedirs(cache, exist_ok=True)
    clips = []
    P = phrases()
    calls = 0
    failed = []
    for i, (k, ssml, pause) in enumerate(P):
        h = hashlib.sha1(('%s|%s|%s|%s' % (a.voice, a.rate, ssml, 'dry' if a.dry_run else 'g')).encode()).hexdigest()[:16]
        f = os.path.join(cache, h + '.pcm')
        if os.path.exists(f):
            raw = open(f, 'rb').read()
            s = list(struct.unpack('<%dh' % (len(raw) // 2), raw))
        else:
            s = synth_dry(k, ssml) if a.dry_run else synth_google(k, ssml, a.voice, a.rate, key)
            if s is None:
                failed.append(k)
                if not a.skip_failed:
                    sys.exit('Stopped at part %r. Parts done so far are cached; re-run to retry, or use --skip-failed.' % k)
                continue
            open(f, 'wb').write(struct.pack('<%dh' % len(s), *s))
            calls += 1
        s = trim_norm(s, pause)
        clips.append((k, len(s), adpcm_encode(s)))
        if (i + 1) % 50 == 0: print('%d/%d' % (i + 1, len(P)), flush=True)
    # layout
    index_size = 12 + sum(1 + len(k.encode()) + 8 for k, _, _ in clips)
    off = (index_size + 3) & ~3
    index = bytearray(b'VOX1' + struct.pack('<II', len(clips), RATE))
    data = bytearray()
    for k, n, d in clips:
        kb = k.encode()
        index += struct.pack('<B', len(kb)) + kb + struct.pack('<II', off + len(data), n)
        data += d
        data += b'\0' * ((-len(data)) & 3)
    blob = bytes(index) + b'\0' * (off - len(index)) + bytes(data)
    open(a.out, 'wb').write(blob)
    secs = sum(n for _, n, _ in clips) / RATE
    if failed: print('LEFT OUT (rejected by the voice): %s' % ', '.join(failed))
    print('%d clips, %.0f s of audio, %d bytes -> %s (%d TTS calls)' % (len(clips), secs, len(blob), a.out, calls))

if __name__ == '__main__':
    main()
