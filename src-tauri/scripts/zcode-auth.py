"""Read ZCode's provider-scoped OAuth identity without exposing plaintext tokens.

The format follows the locally installed official credential cipher. Input and
encrypted snapshots travel through stdin/stdout pipes; logs only receive errors.
"""
import base64
import hashlib
import json
import os
import pathlib
import sys

def key():
    secret = os.environ.get('ZCODE_CREDENTIAL_SECRET') or (
        'zcode-credential-fallback:%s:%s:%s' %
        (sys.platform, pathlib.Path.home(),
         os.environ.get('USERNAME') or os.environ.get('USER') or 'unknown'))
    return hashlib.sha256(secret.encode('utf-8')).digest()


def decode(value):
    if not isinstance(value, str):
        raise ValueError('ZCode credential values must be strings')
    if not value.startswith('enc:v1:'):
        return value
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    parts = value[7:].split('.')
    if len(parts) != 3:
        raise ValueError('Invalid encrypted ZCode credential')
    iv, tag, data = [base64.urlsafe_b64decode(p + '=' * (-len(p) % 4)) for p in parts]
    if len(iv) != 12 or len(tag) != 16:
        raise ValueError('Invalid encrypted ZCode credential')
    return AESGCM(key()).decrypt(iv, data + tag, None).decode('utf-8')


def identity(record):
    provider = decode(record.get('oauth:active_provider', ''))
    if provider not in ('bigmodel', 'zai'):
        return {'has_auth': False, 'provider': '', 'account': '', 'user_id': ''}
    access = decode(record.get('oauth:%s:access_token' % provider, ''))
    raw = decode(record.get('oauth:%s:user_info' % provider, ''))
    if not access or not raw:
        return {'has_auth': False, 'provider': provider, 'account': '', 'user_id': ''}
    info = json.loads(raw)
    if not isinstance(info, dict):
        raise ValueError('ZCode account identity is invalid')
    identifier = str(info.get('id') or info.get('user_id') or '').strip()
    if not identifier:
        raise ValueError('ZCode account ID is missing')
    jwt = decode(record.get('zcodejwttoken', ''))
    if jwt:
        parts = jwt.split('.')
        if len(parts) != 3:
            raise ValueError('ZCode login token is invalid')
        claims = json.loads(base64.urlsafe_b64decode(parts[1] + '=' * (-len(parts[1]) % 4)))
        subject = str(claims.get('user_id') or claims.get('sub') or '').strip()
        if subject and subject != identifier:
            raise ValueError('ZCode OAuth and billing identities differ')
    account = str(info.get('displayName') or info.get('name') or info.get('username')
                  or info.get('email') or identifier).strip()
    return {'has_auth': True, 'provider': provider, 'account': account,
            'user_id': 'zcode:%s:%s' % (provider, identifier),
            'provider_user_id': identifier, 'email': str(info.get('email') or '')}


def snapshot(record):
    who = identity(record)
    if not who['has_auth']:
        raise ValueError('ZCode is not logged in')
    provider = who['provider']
    allowed = {'oauth:active_provider', 'oauth:login_attribution', 'zcodejwttoken'}
    allowed.update('oauth:%s:%s' % (provider, item)
                   for item in ('access_token', 'refresh_token', 'user_info'))
    return {'identity': who, 'record': {k: v for k, v in record.items() if k in allowed}}


def merge(current, target=None, selected_provider=None):
    # A shared record may cache both providers. Preserve every other provider's
    # ciphertext, bot credentials and project API configuration byte-for-byte.
    snap = snapshot(target) if target is not None else None
    provider = snap['identity']['provider'] if snap else selected_provider
    if provider not in ('bigmodel', 'zai'):
        raise ValueError('The selected ZCode provider is required')
    if snap is None and decode(current.get('oauth:active_provider', '')) != provider:
        raise ValueError('The active ZCode provider changed; logout was cancelled')
    prefix = 'oauth:%s:' % provider
    result = {k: v for k, v in current.items()
              if not k.startswith(prefix)
              and k not in ('oauth:login_attribution', 'zcodejwttoken')}
    if snap:
        result.update(snap['record'])
    # On logout retain active_provider, pointing at the logged-out provider.
    # Do not silently activate another provider whose credentials remain cached.
    return result


def run(request):
    record = request.get('record')
    if record is None:
        path = pathlib.Path(request['root']) / 'v2' / 'credentials.json'
        record = json.loads(path.read_text(encoding='utf-8')) if path.exists() else {}
    if not isinstance(record, dict):
        raise ValueError('ZCode credential store must be an object')
    mode = request.get('mode', 'identity')
    if mode == 'identity':
        return identity(record)
    if mode == 'snapshot':
        return snapshot(record)
    if mode == 'merge':
        return {'record': merge(record, request.get('target'), request.get('provider'))}
    raise ValueError('Unknown ZCode credential operation')


if __name__ == '__main__':
    try:
        value = run(json.loads(sys.stdin.buffer.read().decode('utf-8')))
        print(json.dumps({'ok': True, 'value': value}, ensure_ascii=False))
    except Exception as error:
        # Cryptographic or JSON errors never include the plaintext input.
        message = str(error) if isinstance(error, ValueError) else 'ZCode credential reader failed'
        print(json.dumps({'ok': False, 'error': message}, ensure_ascii=False))
        sys.exit(1)
