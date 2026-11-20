"""Read ZCode's provider-scoped OAuth identity without exposing plaintext tokens.

The format follows the locally installed official credential cipher. Input and
encrypted snapshots travel through stdin/stdout pipes; logs only receive errors.

Two envelopes exist (reverse-engineered from app.asar
`createSystemCredentialCipher`):

  enc:v1:<b64url iv>.<b64url tag>.<b64url ct>
      key = SHA256(ZCODE_CREDENTIAL_SECRET or the legacy
      "zcode-credential-fallback:{platform}:{home}:{user}" string).

  enc:v2:<source>:<24-hex key fingerprint>:<b64url iv>.<b64url tag>.<b64url ct>
      source "os"  -> 32 random bytes kept by the OS credential store
                      (Windows entry "{sha256(realpath(credentials.json))}"
                       + ".ZCode Credential Encryption", holding their base64url text);
      source "env" -> key = SHA256(ZCODE_CREDENTIAL_SECRET, or the managed
                      credential-secret file next to credentials.json).
      The 24 hex characters are SHA256(key).hexdigest()[:24], so a candidate key
      can be proven correct before use — we never guess.
"""
import base64
import ctypes
import ctypes.wintypes
import hashlib
import json
import os
import pathlib
import sys
import time

V1_PREFIX = 'enc:v1:'
V2_PREFIX = 'enc:v2:'
KEYRING_SUFFIX = '.ZCode Credential Encryption'
# Credentials written by the client live beside telemetry/config in the same v2
# directory; the keyring entry name is derived from this file's real path.
STATE_DIR = 'v2'
CREDENTIAL_FILE = 'credentials.json'
# Set once per helper process by run(); decrypt() then knows which data root it
# is reading, so the fast keyring lookup can point at the right entry.
ROOT = None


class _FILETIME(ctypes.Structure):
    _fields_ = [('dwLowDateTime', ctypes.wintypes.DWORD),
                ('dwHighDateTime', ctypes.wintypes.DWORD)]


class _CREDENTIAL(ctypes.Structure):
    _fields_ = [
        ('Flags', ctypes.wintypes.DWORD), ('Type', ctypes.wintypes.DWORD),
        ('TargetName', ctypes.wintypes.LPWSTR), ('Comment', ctypes.wintypes.LPWSTR),
        ('LastWritten', _FILETIME), ('CredentialBlobSize', ctypes.wintypes.DWORD),
        ('CredentialBlob', ctypes.POINTER(ctypes.c_char)),
        ('Persist', ctypes.wintypes.DWORD), ('AttributeCount', ctypes.wintypes.DWORD),
        ('Attributes', ctypes.c_void_p), ('TargetAlias', ctypes.wintypes.LPWSTR),
        ('UserName', ctypes.wintypes.LPWSTR)]


def default_root():
    return pathlib.Path(os.environ.get('ZCODE_HOME') or (pathlib.Path.home() / '.zcode'))


def legacy_secret():
    return os.environ.get('ZCODE_CREDENTIAL_SECRET') or (
        'zcode-credential-fallback:%s:%s:%s' %
        (sys.platform, pathlib.Path.home(),
         os.environ.get('USERNAME') or os.environ.get('USER') or 'unknown'))


def fingerprint(key):
    return hashlib.sha256(key).hexdigest()[:24]


def _win_cred_read(target):
    """Read one generic credential's password; None when absent or unreadable."""
    if sys.platform != 'win32':
        return None
    advapi = ctypes.WinDLL('advapi32', use_last_error=True)
    pointer = ctypes.POINTER(_CREDENTIAL)()
    advapi.CredReadW.argtypes = [ctypes.wintypes.LPWSTR, ctypes.wintypes.DWORD,
                                 ctypes.wintypes.DWORD,
                                 ctypes.POINTER(ctypes.POINTER(_CREDENTIAL))]
    advapi.CredReadW.restype = ctypes.wintypes.BOOL
    advapi.CredFree.argtypes = [ctypes.c_void_p]
    if not advapi.CredReadW(target, 1, 0, ctypes.byref(pointer)):
        return None
    try:
        credential = pointer.contents
        raw = ctypes.string_at(credential.CredentialBlob, credential.CredentialBlobSize)
        return raw.decode('utf-16-le')
    except Exception:                       # noqa: BLE001 - never echo the blob
        return None
    finally:
        advapi.CredFree(pointer)


def _win_cred_targets():
    """Every credential-store entry that belongs to ZCode."""
    if sys.platform != 'win32':
        return []
    advapi = ctypes.WinDLL('advapi32', use_last_error=True)
    count = ctypes.wintypes.DWORD(0)
    array = ctypes.POINTER(ctypes.POINTER(_CREDENTIAL))()
    advapi.CredEnumerateW.argtypes = [
        ctypes.wintypes.LPWSTR, ctypes.wintypes.DWORD,
        ctypes.POINTER(ctypes.wintypes.DWORD),
        ctypes.POINTER(ctypes.POINTER(ctypes.POINTER(_CREDENTIAL)))]
    advapi.CredEnumerateW.restype = ctypes.wintypes.BOOL
    advapi.CredFree.argtypes = [ctypes.c_void_p]
    if not advapi.CredEnumerateW(None, 0, ctypes.byref(count), ctypes.byref(array)):
        return []
    try:
        names = [(array[i].contents.TargetName or '') for i in range(count.value)]
    finally:
        advapi.CredFree(array)
    return [name for name in names if name.endswith(KEYRING_SUFFIX)]


def _key_from_text(secret):
    if not secret:
        return None
    try:
        key = base64.urlsafe_b64decode(secret + '=' * (-len(secret) % 4))
    except Exception:                       # noqa: BLE001
        return None
    return key if len(key) == 32 else None


def os_key(want):
    """Resolve the OS-store key whose fingerprint is `want`.

    Fast path derives the entry name from credentials.json's real path. The slow
    path enumerates, because a captured login carries its ciphertext into a temp
    or instance directory while the key stays under the original path.
    """
    candidates = []
    if ROOT is not None:
        path = os.path.join(str(ROOT), STATE_DIR, CREDENTIAL_FILE)
        for form in {os.path.realpath(path), os.path.abspath(path)}:
            candidates.append(hashlib.sha256(form.encode('utf-8')).hexdigest() + KEYRING_SUFFIX)
    for target in candidates:
        key = _key_from_text(_win_cred_read(target))
        if key is not None and fingerprint(key) == want:
            return key
    for target in _win_cred_targets():
        key = _key_from_text(_win_cred_read(target))
        if key is not None and fingerprint(key) == want:
            return key
    return None


def env_key(want):
    """Resolve an env-source key: env var, then the managed secret file."""
    texts = [os.environ.get('ZCODE_CREDENTIAL_SECRET')]
    declared = os.environ.get('ZCODE_CREDENTIAL_SECRET_FILE')
    if declared:
        try:
            texts.append(pathlib.Path(declared).read_text(encoding='utf-8'))
        except OSError:
            pass
    if ROOT is not None:
        try:
            texts.append((pathlib.Path(str(ROOT)) / STATE_DIR / 'credential-secret')
                         .read_text(encoding='utf-8'))
        except OSError:
            pass
    for text in texts:
        if not text or not 32 <= len(text.encode('utf-8')) <= 4096:
            continue
        key = hashlib.sha256(text.encode('utf-8')).digest()
        if fingerprint(key) == want:
            return key
    return None


def _open_envelope(body, key):
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    parts = body.split('.')
    if len(parts) != 3:
        raise ValueError('Invalid encrypted ZCode credential')
    iv, tag, data = [base64.urlsafe_b64decode(p + '=' * (-len(p) % 4)) for p in parts]
    if len(iv) != 12 or len(tag) != 16:
        raise ValueError('Invalid encrypted ZCode credential')
    return AESGCM(key).decrypt(iv, data + tag, None).decode('utf-8')


def decode(value):
    if not isinstance(value, str):
        raise ValueError('ZCode credential values must be strings')
    if not value.startswith('enc:'):
        return value
    if value.startswith(V1_PREFIX):
        key = hashlib.sha256(legacy_secret().encode('utf-8')).digest()
        return _open_envelope(value[len(V1_PREFIX):], key)
    if not value.startswith(V2_PREFIX):
        # Never hand an unknown envelope back as if it were plaintext: that is
        # exactly how a format change used to degrade into "not logged in".
        raise ValueError('ZCode credential envelope is not recognised (%s…); '
                         'the desktop app changed its cipher' % value[:12])
    source, want, body = value[len(V2_PREFIX):].split(':', 2)
    if source not in ('os', 'env') or len(want) != 24:
        raise ValueError('Invalid encrypted ZCode credential')
    key = (os_key if source == 'os' else env_key)(want)
    if key is None:
        raise ValueError('ZCode system key unavailable (%s/%s); unlock the OS '
                         'credential store and retry' % (source, want))
    return _open_envelope(body, key)


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


def proxy_credential(record, expected):
    """Explicit, pipe-only export for the selected card; never use another login."""
    who = identity(record)
    if not expected or not who['has_auth'] or who['user_id'] != expected:
        raise ValueError('ZCode proxy account identity does not match the selected card')
    jwt = decode(record.get('zcodejwttoken', '')).strip()
    if not jwt:
        raise ValueError('ZCode Start Plan token is missing; open ZCode and save this login again')
    parts = jwt.split('.')
    claims = json.loads(base64.urlsafe_b64decode(parts[1] + '=' * (-len(parts[1]) % 4)))
    if claims.get('exp') is not None and float(claims['exp']) <= time.time():
        raise ValueError('ZCode Start Plan token expired; sign in again and save the login')
    return {'provider': who['provider'], 'user_id': expected, 'jwt': jwt}


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
    global ROOT
    ROOT = pathlib.Path(request['root']) if request.get('root') else default_root()
    record = request.get('record')
    if record is None:
        path = ROOT / STATE_DIR / CREDENTIAL_FILE
        record = json.loads(path.read_text(encoding='utf-8')) if path.exists() else {}
    if not isinstance(record, dict):
        raise ValueError('ZCode credential store must be an object')
    mode = request.get('mode', 'identity')
    if mode == 'identity':
        return identity(record)
    if mode == 'snapshot':
        return snapshot(record)
    if mode == 'proxy':
        return proxy_credential(record, request.get('expected_user_id', ''))
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
