"""Explicit live vision smoke, run via agsync. One small request uses an existing
authorized distribution key. No client configuration changes; normal gateway
usage counters apply. Credentials and upstream response bodies are never printed.
"""
import base64
import json
import os
from pathlib import Path
import random
import re
import struct
import sys
import urllib.error
import urllib.request
import zlib


def fixture():
    code = ''.join(random.SystemRandom().choice('0123456789') for _ in range(4))
    width, height = 350, 130
    pixels = bytearray([255]) * (width * height * 3)
    segments = ['ab cdef'.replace(' ', ''), 'bc', 'abged', 'abgcd', 'fgbc', 'afgcd', 'afgecd', 'abc', 'abcdefg', 'abfgcd']
    rects = {'a': (15, 10, 55, 20), 'b': (55, 20, 65, 60), 'c': (55, 70, 65, 110),
             'd': (15, 110, 55, 120), 'e': (5, 70, 15, 110), 'f': (5, 20, 15, 60), 'g': (15, 60, 55, 70)}
    for i, char in enumerate(code):
        for segment in segments[int(char)]:
            x1, y1, x2, y2 = rects[segment]
            for y in range(y1, y2):
                for x in range(x1 + 15 + i * 80, x2 + 15 + i * 80):
                    offset = (y * width + x) * 3
                    pixels[offset:offset + 3] = b'\x10\x10\x10'
    raw = b''.join(b'\0' + pixels[y * width * 3:(y + 1) * width * 3] for y in range(height))
    def chunk(kind, value):
        return struct.pack('!I', len(value)) + kind + value + struct.pack('!I', zlib.crc32(kind + value))
    png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', width, height, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')
    return code, 'data:image/png;base64,' + base64.b64encode(png).decode()


def main():
    root = Path(os.environ.get('AGENT_COCKPIT_HOME') or Path.home() / '.agent-cockpit')
    pointer = Path.home() / '.agent-cockpit-root'
    if not os.environ.get('AGENT_COCKPIT_HOME') and pointer.exists():
        root = Path(pointer.read_text(encoding='utf-8-sig').strip())
    cfg = json.loads((root / 'config.json').read_text(encoding='utf-8-sig'))['proxy']
    base = 'http://127.0.0.1:' + str(cfg.get('port') or 8788)
    # Never use environment proxy configuration for the local secret-bearing request.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    def request(path, key, body=None):
        req = urllib.request.Request(base + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json', 'User-Agent': 'AgentCockpit vision smoke'})
        with opener.open(req, timeout=120) as response:
            return json.load(response)
    chosen = None
    for dist in cfg.get('dist', []):
        if dist.get('enabled') is False or not dist.get('key'):
            continue
        rows = request('/v1/models', dist['key']).get('data', [])
        row = next((m for m in rows if m['id'] == 'trae/deepseek-v4.1-flash'), None)
        if row:
            chosen = row, dist['key']
            break
    if chosen is None:
        raise RuntimeError('No authorized trae/deepseek-v4.1-flash model is available')
    row, key = chosen
    if row.get('native_vision') is not True or 'image' not in row.get('input_modalities', []):
        raise RuntimeError('Gateway model catalog does not advertise native image input')
    code, url = fixture()
    response = request('/v1/chat/completions', key, {'model': row['id'], 'stream': False, 'max_tokens': 32,
        'messages': [{'role': 'user', 'content': [{'type': 'text', 'text': 'Read the four-digit code in this image. Respond with only the four digits.'}, {'type': 'image_url', 'image_url': {'url': url}}]}]})
    content = response['choices'][0]['message']['content']
    if re.sub(r'\D', '', content) != code:
        raise RuntimeError('Image reading did not match the randomly generated four-digit fixture')
    print(json.dumps({'result': 'PASS', 'model': row['id'], 'native_vision': True,
        'context_window': row.get('context_window'), 'max_output_tokens': row.get('max_output_tokens'),
        'check': 'four random digits correctly read through the real local gateway'}, ensure_ascii=False))


if __name__ == '__main__':
    try:
        main()
    except urllib.error.HTTPError as error:
        print(json.dumps({'result': 'FAIL', 'http_status': error.code}))
        sys.exit(1)
    except Exception as error:
        # Exception messages from network libraries may include private addresses.
        print(json.dumps({'result': 'FAIL', 'kind': type(error).__name__,
            'detail': str(error) if isinstance(error, RuntimeError) else 'Local smoke check could not complete'}))
        sys.exit(1)
