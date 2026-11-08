"""Synthetic-only credential isolation tests; never reads a local login."""
import base64
import importlib.util
import json
import pathlib
import time
import unittest

script = pathlib.Path(__file__).resolve().parents[1] / 'src-tauri/scripts/zcode-auth.py'
spec = importlib.util.spec_from_file_location('zcode_auth', script)
auth = importlib.util.module_from_spec(spec)
spec.loader.exec_module(auth)


def record(provider='zai', user='one', exp=None):
    payload = {'sub': user}
    if exp is not None:
        payload['exp'] = exp
    jwt = 'test.' + base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip('=') + '.test'
    return {'oauth:active_provider': provider, 'oauth:%s:access_token' % provider: 'synthetic-oauth',
            'oauth:%s:user_info' % provider: json.dumps({'id': user}), 'zcodejwttoken': jwt}


class ProxyCredentialTest(unittest.TestCase):
    def test_only_the_expected_provider_and_account_can_export(self):
        r = record()
        result = auth.run({'mode': 'proxy', 'record': r, 'expected_user_id': 'zcode:zai:one'})
        self.assertEqual(result['jwt'], r['zcodejwttoken'])
        self.assertEqual(set(result), {'provider', 'jwt', 'user_id'})
        for expected in ['', 'zcode:zai:two', 'zcode:bigmodel:one']:
            with self.assertRaises(ValueError):
                auth.proxy_credential(r, expected)

    def test_never_falls_back_to_oauth_when_plan_jwt_is_missing(self):
        r = record()
        del r['zcodejwttoken']
        with self.assertRaisesRegex(ValueError, 'missing'):
            auth.proxy_credential(r, 'zcode:zai:one')

    def test_expired_and_other_subject_tokens_are_rejected(self):
        with self.assertRaisesRegex(ValueError, 'expired'):
            auth.proxy_credential(record(exp=time.time() - 1), 'zcode:zai:one')
        r = record(); r['zcodejwttoken'] = record(user='other')['zcodejwttoken']
        with self.assertRaisesRegex(ValueError, 'differ'):
            auth.proxy_credential(r, 'zcode:zai:one')

    def test_normal_identity_calls_do_not_export_tokens(self):
        who = auth.run({'record': record()})
        self.assertNotIn('jwt', who)
        self.assertNotIn('token', json.dumps(who))


if __name__ == '__main__':
    unittest.main()
