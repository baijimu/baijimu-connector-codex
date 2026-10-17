import copy
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import threading
import unittest
from unittest.mock import patch
import urllib.parse
import uuid

spec = importlib.util.spec_from_file_location('publisher', Path(__file__).parents[1] / 'publish_market.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

MANIFEST = '{\n "schemaVersion": "3.0.0", "appId":"sample", "version":"2.0.5",\n "source":{"type":"github","repo":"example/sample","revision":"v2.0.5"}\n}'
BASE = 'https://owner.example'
PUBLIC = 'https://files.example/releases'
SOURCE = {'application': {'environmentKey': 'test', 'appId': 'sample'}, 'version': '2.0.5'}
APP = m.APPS + '/sample'
VERSION = APP + '/versions/2.0.5'


def envelope(data=None, code='0'):
    return json.dumps({'contractVersion': '1.0.0', 'errorCode': code, 'data': data}).encode()


class Owner(m.Http):
    """HTTP contract fixture; the real client's envelope and absence checks still run."""
    def __init__(self):
        super().__init__(BASE, 'test-only-pat', 71)
        self.objects = {}
        self.content_raw = None
        self.receipt = None
        self.calls = []
        self.fail_after = set()
        self.responses = {}
        self.oss = {'schemaVersion': '2.0.0', 'appId': 'sample', 'version': '2.0.5',
                    'releaseTag': 'v2.0.5', 'artifacts': []}
        self.files = {}
        for platform, arch in [('macos', 'universal'), ('windows', 'x86_64'), ('linux', 'x86_64')]:
            data = ('signed-immutable-bytes-' + platform).encode()
            url = PUBLIC + '/' + platform + '.zip'
            self.files[url] = data
            self.oss['artifacts'].append({'platform': platform, 'arch': arch, 'name': platform + '.zip',
                                         'source': url, 'checksum': 'sha256:' + hashlib.sha256(data).hexdigest()})

    def read(self, url, method='GET', body=None, authenticated=False, binary=False):
        self.calls.append((url, method, authenticated, binary, body))
        if (method, url) in self.responses:
            return self.responses[(method, url)]
        if url in self.files:
            assert not authenticated
            return 200, self.files[url]
        assert authenticated and url.startswith(BASE + "/partner/")
        route = url[len(BASE):]
        if route == APP:
            return 200, envelope({'source': SOURCE['application'], 'workspaceId': 71,
                                  'registered': True, 'applicationType': 'connector'})
        if route.startswith(APP + '/artifacts?'):
            assert method == 'POST' and binary
            name = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)['fileName'][0]
            key = str(uuid.uuid4())
            self.objects[key] = body
            return 200, envelope({'artifactId': key, 'fileName': name, 'sizeBytes': len(body)})
        if route == VERSION:
            if method == 'POST':
                assert self.content_raw is None
                self.content_raw = body.decode()
                if 'freeze' in self.fail_after:
                    raise m.PublishError('lost freeze response')
                return 200, envelope({'created': True, 'frozenVersion': json.loads(self.frozen())})
            return (200, b'{"contractVersion":"1.0.0","errorCode":"0","data":' + self.frozen().encode() + b'}') if self.content_raw else (200, envelope(code=m.NOT_FOUND))
        if route.startswith(VERSION + '/artifacts/'):
            return 200, self.objects[route.rsplit('/', 1)[1]]
        if route == VERSION + '/submit':
            assert method == 'POST' and body == b'{}'
            self.receipt = self.receipt or {'source': SOURCE, 'requestId': str(uuid.uuid4()),
                                            'publicationId': str(uuid.uuid4())}
            self.receipt['state'] = 'PENDING_REVIEW'
            if 'submit' in self.fail_after:
                raise m.PublishError('lost submit response')
            return 200, envelope(self.receipt)
        if route == VERSION + '/publication':
            return 200, envelope(self.receipt) if self.receipt else envelope(code=m.NOT_FOUND)
        raise AssertionError(route)

    def frozen(self):
        return '{"contractVersion":"1.0.0","source":' + json.dumps(SOURCE) + ',"content":' + self.content_raw + '}'

    def publisher(self, manifest=MANIFEST):
        return m.Publisher(self, 'test', 71, PUBLIC, '2.0.5', manifest, self.oss)

    def writes(self):
        return [call for call in self.calls if call[1] == 'POST']


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.env = patch.dict(os.environ, {'GITHUB_REPOSITORY': 'example/sample'})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.owner = Owner()

    def test_publish_and_repeat_preserve_raw_manifest_and_avoid_mutations(self):
        self.assertEqual(self.owner.publisher().run(), 'PENDING_REVIEW')
        self.assertEqual(m.raw_field(self.owner.content_raw, ['manifest']), MANIFEST)
        self.assertEqual(len(self.owner.writes()), 5)
        self.owner.calls.clear()
        self.assertEqual(self.owner.publisher().run(), 'PENDING_REVIEW')
        self.assertEqual(self.owner.writes(), [])

    def test_lost_write_responses_reconcile_exact_version_and_receipt(self):
        self.owner.fail_after = {'freeze', 'submit'}
        self.assertEqual(self.owner.publisher().run(), 'PENDING_REVIEW')
        self.assertEqual(len(self.owner.writes()), 5)

    def test_resume_receiving_keeps_request_identity(self):
        self.owner.publisher().run()
        identity = self.owner.receipt['requestId']
        self.owner.receipt['state'] = 'RECEIVING'
        self.owner.calls.clear()
        self.assertEqual(self.owner.publisher().run(), 'PENDING_REVIEW')
        self.assertEqual(self.owner.receipt['requestId'], identity)
        self.assertEqual(len(self.owner.writes()), 1)

    def test_terminal_receipt_is_never_resubmitted(self):
        self.owner.publisher().run()
        for state in ('REJECTED', 'WITHDRAWN', 'UNKNOWN'):
            self.owner.receipt['state'] = state
            self.owner.calls.clear()
            with self.assertRaisesRegex(m.PublishError, 'explicit action'):
                self.owner.publisher().run()
            self.assertEqual(self.owner.writes(), [])

    def test_public_checksum_mismatch_prevents_all_writes(self):
        self.owner.files[next(iter(self.owner.files))] = b'changed'
        with self.assertRaisesRegex(m.PublishError, 'checksum mismatch'):
            self.owner.publisher().run()
        self.assertEqual(self.owner.writes(), [])

    def test_wrong_owner_prevents_downloads_and_writes(self):
        self.owner.responses[('GET', BASE + APP)] = 200, envelope({'workspaceId': 999})
        with self.assertRaisesRegex(m.PublishError, 'owner/source'):
            self.owner.publisher().run()
        self.assertEqual(len(self.owner.calls), 1)

    def test_existing_version_must_match_raw_manifest_and_bytes(self):
        self.owner.publisher().run()
        with self.assertRaisesRegex(m.PublishError, 'manifest/revision'):
            self.owner.publisher(json.dumps(json.loads(MANIFEST))).run()
        self.owner.objects[next(iter(self.owner.objects))] = b'changed'
        with self.assertRaisesRegex(m.PublishError, 'artifact bytes'):
            self.owner.publisher().run()

    def test_wrong_receipt_identity_prevents_submission(self):
        self.owner.publisher().run()
        self.owner.receipt['source'] = dict(SOURCE, version='9.0.0')
        self.owner.calls.clear()
        with self.assertRaisesRegex(m.PublishError, 'receipt identity'):
            self.owner.publisher().run()
        self.assertEqual(self.owner.writes(), [])

    def test_absence_requires_exact_code_and_eligible_http_status(self):
        for status, code in [(401, m.NOT_FOUND), (503, m.NOT_FOUND), (200, 'LOCAL_APP_SERVICE_FORBIDDEN'),
                             (404, 'PUBLIC_OPENRESTY_NOT_FOUND')]:
            self.owner.responses[('GET', BASE + VERSION)] = status, envelope(code=code)
            with self.assertRaises(m.ApiError):
                self.owner.publisher().run()
        self.assertEqual(self.owner.writes(), [])

    def test_current_structured_failure_contract_is_supported(self):
        detail = {'message': 'Invalid request', 'retryable': False,
                  'violations': [{'path': 'manifest', 'reason': 'INVALID_FIELD'}]}
        self.owner.responses[('GET', BASE + APP)] = 200, envelope(detail, 'LOCAL_APP_SERVICE_INVALID_REQUEST')
        with self.assertRaises(m.ApiError) as context:
            self.owner.publisher().run()
        self.assertEqual(context.exception.code, 'LOCAL_APP_SERVICE_INVALID_REQUEST')

    def test_malformed_envelopes_fail_closed(self):
        for body in [b'{}', b'{"contractVersion":"2.0.0","errorCode":"0","data":{}}',
                     envelope({'retryable': False}, 'LOCAL_APP_SERVICE_NOT_FOUND'),
                     b'{"contractVersion":"1.0.0","errorCode":"0","data":{},"data":{}}']:
            self.owner.responses[('GET', BASE + APP)] = 200, body
            with self.assertRaises(m.PublishError):
                self.owner.publisher().run()
        self.assertEqual(self.owner.writes(), [])

    def test_artifact_authority_and_duplicate_target_fail_before_io(self):
        for url in ['https://files.example.attacker/releases/a.zip', 'https://files.example/releases/../a.zip',
                    'https://user:pass@files.example/releases/a.zip']:
            self.owner.oss['artifacts'][0]['source'] = url
            with self.assertRaises(m.PublishError):
                self.owner.publisher()
        self.assertEqual(self.owner.calls, [])
        self.owner = Owner()
        self.owner.oss['artifacts'].append(copy.deepcopy(self.owner.oss['artifacts'][0]))
        with self.assertRaises(m.PublishError):
            self.owner.publisher()


class MarketAcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.env = patch.dict(os.environ, {'GITHUB_REPOSITORY': 'example/sample'})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.owner = Owner()
        self.owner.publisher().run()
        self.owner.receipt['state'] = 'PUBLISHED'
        self.listing_id = str(uuid.uuid4())
        self.query = '?workspaceId=71'
        self.page_url = BASE + m.MARKET + self.query
        self.exact_url = BASE + m.MARKET + '/' + self.listing_id + '/versions/2.0.5'
        self.listing = {'contractVersion': '3.0.0', 'listingId': self.listing_id,
                        'frozenVersion': json.loads(self.owner.frozen())}
        self.set_page([self.listing])
        # Preserve the opaque manifest bytes, exactly as the owner does.
        raw = '{"contractVersion":"3.0.0","listingId":' + json.dumps(self.listing_id) + ',"frozenVersion":' + self.owner.frozen() + '}'
        self.owner.responses[('GET', self.exact_url + self.query)] = 200, b'{"contractVersion":"1.0.0","errorCode":"0","data":' + raw.encode() + b'}'
        for key, data in self.owner.objects.items():
            self.owner.responses[('GET', self.exact_url + '/artifacts/' + key + self.query)] = 200, data
        self.owner.calls.clear()

    def set_page(self, items, cursor=None, url=None):
        self.owner.responses[('GET', url or self.page_url)] = 200, envelope({
            'items': items, 'nextCursor': cursor})

    def test_client_catalog_and_all_three_market_downloads_are_read_only(self):
        self.assertEqual(self.owner.publisher().verify_market(), 'MARKET_VERIFIED')
        self.assertEqual(self.owner.writes(), [])
        downloads = [c for c in self.owner.calls if c[3]]
        self.assertEqual(len(downloads), 3)
        self.assertTrue(all('/local-app-market/listings/' in c[0] for c in downloads))

    def test_published_receipt_with_older_catalog_pin_is_not_acceptance(self):
        self.listing['frozenVersion']['source']['version'] = '2.0.4'
        self.set_page([self.listing])
        with self.assertRaisesRegex(m.PublishError, 'different version'):
            self.owner.publisher().verify_market()
        self.assertEqual(self.owner.writes(), [])

    def test_pending_review_and_missing_listing_fail_without_mutations(self):
        self.owner.receipt['state'] = 'PENDING_REVIEW'
        with self.assertRaisesRegex(m.PublishError, 'not independently approved'):
            self.owner.publisher().verify_market()
        self.owner.receipt['state'] = 'PUBLISHED'
        self.set_page([])
        with self.assertRaisesRegex(m.PublishError, 'exactly once'):
            self.owner.publisher().verify_market()
        self.assertEqual(self.owner.writes(), [])

    def test_other_environment_cannot_satisfy_catalog_match(self):
        self.listing['frozenVersion']['source']['application']['environmentKey'] = 'other'
        self.set_page([self.listing])
        with self.assertRaisesRegex(m.PublishError, 'exactly once'):
            self.owner.publisher().verify_market()

    def test_paginated_catalog_is_checked(self):
        other = copy.deepcopy(self.listing)
        other['listingId'] = str(uuid.uuid4())
        other['frozenVersion']['source']['application']['appId'] = 'other'
        self.set_page([other], other['listingId'])
        self.set_page([self.listing], url=self.page_url + '&after=' + other['listingId'])
        self.assertEqual(self.owner.publisher().verify_market(), 'MARKET_VERIFIED')

    def test_duplicate_listing_and_pagination_cycle_fail(self):
        self.set_page([self.listing, self.listing])
        with self.assertRaisesRegex(m.PublishError, 'exactly once'):
            self.owner.publisher().verify_market()
        self.set_page([self.listing], self.listing_id)
        self.set_page([self.listing], self.listing_id, self.page_url + '&after=' + self.listing_id)
        with self.assertRaisesRegex(m.PublishError, 'pagination'):
            self.owner.publisher().verify_market()

    def test_exact_market_manifest_and_artifact_corruption_fail(self):
        raw = self.owner.responses[('GET', self.exact_url + self.query)][1]
        self.owner.responses[('GET', self.exact_url + self.query)] = 200, raw.replace(b'"repo":"example/sample"', b'"repo":"example/wrong"')
        with self.assertRaisesRegex(m.PublishError, 'differs'):
            self.owner.publisher().verify_market()
        self.owner.responses[('GET', self.exact_url + self.query)] = 200, raw
        key = next(iter(self.owner.objects))
        self.owner.responses[('GET', self.exact_url + '/artifacts/' + key + self.query)] = 200, b'changed'
        with self.assertRaisesRegex(m.PublishError, 'artifact bytes mismatch'):
            self.owner.publisher().verify_market()



class TransportTests(unittest.TestCase):
    def test_headers_and_redirect_rejection(self):
        calls = []

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                calls.append((self.path, dict(self.headers)))
                if self.path == '/redirect':
                    self.send_response(302)
                    self.send_header('Location', '/leak')
                    self.end_headers()
                else:
                    self.send_response(200)
                    self.end_headers()
                    self.wfile.write(envelope({'ok': True}))

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        worker = threading.Thread(target=server.serve_forever)
        worker.start()
        try:
            base = f'http://127.0.0.1:{server.server_port}'
            http = m.Http(base, 'test-only-pat', 71)
            http.api('/owner')
            self.assertEqual(calls[0][1]['Authorization'], 'Bearer test-only-pat')
            self.assertEqual(calls[0][1]['X-Workspace-Id'], '71')
            http.read(base + '/public', binary=True)
            self.assertNotIn('Authorization', calls[1][1])
            self.assertNotIn('X-Workspace-Id', calls[1][1])
            with self.assertRaisesRegex(m.PublishError, 'redirect rejected'):
                http.api('/redirect')
            self.assertFalse(any(call[0] == '/leak' for call in calls))
        finally:
            server.shutdown()
            server.server_close()
            worker.join()


if __name__ == '__main__':
    unittest.main()
