"""enc:v2 信封的回归测试：全合成数据，绝不读本机真实登录。

2026-11-15 的真实故障：ZCode 桌面端把凭据从 enc:v1（本地派生密钥）升级成
enc:v2:os:（密钥寄存在系统凭据库），读取器只认 enc:v1、遇到别的信封**原样返回**，
于是「格式不认识」被伪装成「登录态读不到」，界面上只看到一句 ZCode 尚未登录。
这组测试钉住三件事：
  1. enc:v2:env: 能用 S HA256(secret) 解开（指纹自校验）；
  2. 认不出的信封必须抛错，不许当明文放行；
  3. 指纹对不上的密钥不许硬用。
"""
import base64
import hashlib
import importlib.util
import json
import os
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


auth = load('zcode_auth', ROOT / 'src-tauri/scripts/zcode-auth.py')


def seal(plain, key):
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    iv = os.urandom(12)
    cipher = AESGCM(key)
    body = cipher.encrypt(iv, plain.encode('utf-8'), None)
    tag, data = body[-16:], body[:-16]
    parts = [iv, tag, data]
    return '.'.join(base64.urlsafe_b64encode(p).decode().rstrip('=') for p in parts)


def env_record(secret, provider='zai', user='synthetic'):
    """按官方 v2(env) 格式造一条记录：指纹就是 SHA256(key)[:24]。"""
    key = hashlib.sha256(secret.encode('utf-8')).digest()
    want = hashlib.sha256(key).hexdigest()[:24]
    jwt = 'h.' + base64.urlsafe_b64encode(
        json.dumps({'sub': user}).encode()).decode().rstrip('=') + '.t'
    values = {'oauth:active_provider': provider,
              'oauth:%s:access_token' % provider: 'synthetic-oauth',
              'oauth:%s:user_info' % provider: json.dumps({'id': user}),
              'zcodejwttoken': jwt}
    return {k: 'enc:v2:env:%s:%s' % (want, seal(v, key)) for k, v in values.items()}


class EnvelopeV2Test(unittest.TestCase):
    def setUp(self):
        self.previous = os.environ.get('ZCODE_CREDENTIAL_SECRET')
        os.environ['ZCODE_CREDENTIAL_SECRET'] = 'synthetic-managed-secret-0123456789abcdef'
        self.record = env_record(os.environ['ZCODE_CREDENTIAL_SECRET'])
        auth.ROOT = None

    def tearDown(self):
        if self.previous is None:
            os.environ.pop('ZCODE_CREDENTIAL_SECRET', None)
        else:
            os.environ['ZCODE_CREDENTIAL_SECRET'] = self.previous

    def test_v2_env_envelope_round_trips(self):
        who = auth.run({'record': self.record})
        self.assertTrue(who['has_auth'])
        self.assertEqual(who['user_id'], 'zcode:zai:synthetic')
        exported = auth.run({'mode': 'proxy', 'record': self.record,
                             'expected_user_id': 'zcode:zai:synthetic'})
        # 导出的是解出来的明文 JWT，不是信封本身
        self.assertTrue(exported['jwt'].startswith('h.'))
        self.assertEqual(exported['jwt'], auth.decode(self.record['zcodejwttoken']))

    def test_every_value_is_decrypted_not_passed_through(self):
        for key in self.record:
            plain = auth.decode(self.record[key])
            self.assertFalse(plain.startswith('enc:'), '%s 未被解密' % key)

    def test_unknown_envelope_raises_instead_of_looking_like_a_logout(self):
        # 这才是 11-15 那次故障的形态：未知信封被原样返回，active_provider 变成
        # 一串密文，于是 identity() 认为「没登录」。
        record = dict(self.record)
        record['oauth:active_provider'] = 'enc:v3:whatever:abcdefghijklmnopqrstuvwx:aaa.bbb.ccc'
        with self.assertRaisesRegex(ValueError, 'not recognised'):
            auth.identity(record)

    def test_missing_key_is_reported_as_a_key_problem(self):
        os.environ['ZCODE_CREDENTIAL_SECRET'] = 'a-different-secret-that-cannot-match-00'
        with self.assertRaisesRegex(ValueError, 'system key unavailable'):
            auth.identity(self.record)

    def test_malformed_fingerprint_is_rejected(self):
        record = dict(self.record)
        source_body = record['oauth:active_provider'].split(':', 4)[4]
        record['oauth:active_provider'] = 'enc:v2:env:tooshort:' + source_body
        with self.assertRaisesRegex(ValueError, 'Invalid encrypted ZCode credential'):
            auth.identity(record)

    def test_legacy_v1_envelope_still_reads(self):
        secret = os.environ['ZCODE_CREDENTIAL_SECRET']
        key = hashlib.sha256(secret.encode('utf-8')).digest()
        plain = {'oauth:active_provider': 'zai',
                 'oauth:zai:access_token': 'synthetic-oauth',
                 'oauth:zai:user_info': json.dumps({'id': 'synthetic'}),
                 'zcodejwttoken': 'h.' + base64.urlsafe_b64encode(
                     json.dumps({'sub': 'synthetic'}).encode()).decode().rstrip('=') + '.t'}
        record = {k: 'enc:v1:' + seal(v, key) for k, v in plain.items()}
        who = auth.identity(record)
        self.assertEqual(who['user_id'], 'zcode:zai:synthetic')


class AgenthubParityTest(unittest.TestCase):
    """agenthub 侧的解密必须和读取器同一套规矩，否则额度仍然 401。"""

    @classmethod
    def setUpClass(cls):
        cls.hub = load('agenthub_vendor', ROOT / 'src-tauri/vendor/agenthub.py')

    def setUp(self):
        self.previous = os.environ.get('ZCODE_CREDENTIAL_SECRET')
        os.environ['ZCODE_CREDENTIAL_SECRET'] = 'synthetic-managed-secret-0123456789abcdef'
        self.record = env_record(os.environ['ZCODE_CREDENTIAL_SECRET'])

    def tearDown(self):
        if self.previous is None:
            os.environ.pop('ZCODE_CREDENTIAL_SECRET', None)
        else:
            os.environ['ZCODE_CREDENTIAL_SECRET'] = self.previous

    def test_agenthub_reads_v2_env(self):
        key = self.hub._zcode_key()
        for value in self.record.values():
            plain = self.hub._zcode_decrypt(value, key)
            self.assertFalse(plain.startswith('enc:'))

    def test_agenthub_rejects_unknown_envelope(self):
        with self.assertRaises(ValueError):
            self.hub._zcode_decrypt('enc:v9:future:abc', self.hub._zcode_key())

    def test_agenthub_reports_missing_system_key(self):
        os.environ['ZCODE_CREDENTIAL_SECRET'] = 'unrelated-secret-that-cannot-match-000'
        with self.assertRaisesRegex(ValueError, '取不到系统密钥'):
            self.hub._zcode_decrypt(self.record['zcodejwttoken'], self.hub._zcode_key())


if __name__ == '__main__':
    unittest.main()
