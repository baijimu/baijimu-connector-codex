#!/usr/bin/env python3
"""Publish immutable release bytes through the source owner's Partner contract."""
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

APPS = '/partner/v1/local-app-service/api/local-apps'
NOT_FOUND = 'LOCAL_APP_SERVICE_NOT_FOUND'
MAX_JSON = 8 * 1024 * 1024
MAX_ARTIFACT = 512 * 1024 * 1024


class PublishError(Exception):
    pass


class ApiError(PublishError):
    def __init__(self, code, status):
        super().__init__(f'Partner API failed: HTTP {status}, errorCode={code}')
        self.code = code
        self.status = status


def require(condition, message):
    if not condition:
        raise PublishError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, 'duplicate JSON field')
        result[key] = value
    return result


def decode(raw):
    return json.loads(raw, object_pairs_hook=unique_object)


def raw_field(raw, fields):
    """Select a JSON value without normalizing the owner's opaque manifest bytes."""
    decoder = json.JSONDecoder()
    raw = raw.strip()
    if not fields:
        return raw
    require(raw.startswith('{'), 'expected JSON object')
    offset = 1
    while True:
        while raw[offset].isspace() or raw[offset] == ',':
            offset += 1
        key, offset = decoder.raw_decode(raw, offset)
        while raw[offset].isspace():
            offset += 1
        require(raw[offset] == ':', 'invalid JSON object')
        offset += 1
        while raw[offset].isspace():
            offset += 1
        start = offset
        _, offset = decoder.raw_decode(raw, offset)
        if key == fields[0]:
            return raw_field(raw[start:offset], fields[1:])
        if raw[offset:].lstrip().startswith('}'):
            raise PublishError('missing JSON field: ' + fields[0])


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Http:
    def __init__(self, base, token, workspace):
        self.base = base.rstrip('/')
        self.token = token
        self.workspace = workspace
        self.opener = urllib.request.build_opener(NoRedirect())

    def read(self, url, method='GET', body=None, authenticated=False, binary=False):
        headers = {'Accept': 'application/octet-stream' if binary else 'application/json'}
        if authenticated:
            require(url.startswith(self.base + '/'), 'PAT destination outside configured API')
            headers.update({'Authorization': 'Bearer ' + self.token,
                            'X-Workspace-Id': str(self.workspace)})
        if body is not None:
            headers['Content-Type'] = 'application/octet-stream' if binary else 'application/json'
        request = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            response = self.opener.open(request, timeout=120)
        except urllib.error.HTTPError as error:
            response = error
        except (OSError, urllib.error.URLError) as error:
            raise PublishError(f'{method} transport failed ({type(error).__name__}); rerun to reconcile') from None
        with response:
            status = response.code
            limit = MAX_ARTIFACT if binary and status == 200 else MAX_JSON
            raw = response.read(limit + 1)
        require(len(raw) <= limit, 'response exceeds size limit')
        require(not 300 <= status < 400, 'HTTP redirect rejected')
        return status, raw

    def api(self, route, method='GET', body=None, binary=False):
        status, raw = self.read(self.base + route, method, body, True, binary)
        if binary and method == 'GET' and status == 200:
            return raw
        try:
            envelope = decode(raw)
        except (ValueError, UnicodeError):
            raise PublishError(f'{method} Partner API returned invalid JSON (HTTP {status})') from None
        require(isinstance(envelope, dict) and envelope.get('contractVersion') == '1.0.0'
                and isinstance(envelope.get('errorCode'), str) and 'data' in envelope,
                f'invalid CModel envelope (HTTP {status})')
        code = envelope['errorCode']
        if code != '0':
            detail = envelope['data']
            require(detail is None or (isinstance(detail, dict)
                    and isinstance(detail.get('message'), str) and 1 <= len(detail['message']) <= 512
                    and isinstance(detail.get('retryable'), bool)), 'invalid CModel failure data')
            # Only contract codes are logged, never arbitrary server messages or response bodies.
            require(re.fullmatch(r'[A-Z0-9_]+', code), 'invalid CModel error code')
            raise ApiError(code, status)
        require(status == 200 and envelope['data'] is not None, 'invalid CModel success response')
        return envelope['data'], raw.decode('utf-8')

    def optional(self, route):
        try:
            return self.api(route)
        except ApiError as error:
            if error.code == NOT_FOUND and error.status in (200, 404):
                return None
            raise


def https_base(value):
    parsed = urllib.parse.urlsplit(value)
    require(parsed.scheme == 'https' and parsed.hostname and not parsed.username
            and not parsed.password and not parsed.query and not parsed.fragment,
            'configured base URL must be an explicit HTTPS authority/path')
    require(not any(x in value for x in ('\\', '\r', '\n')), 'invalid base URL')
    return value.rstrip('/')


def valid_id(value):
    try:
        return uuid.UUID(value).int != 0
    except (ValueError, TypeError, AttributeError):
        return False


class Publisher:
    def __init__(self, http, environment, workspace, public_base, version, manifest_raw, oss):
        self.http = http
        self.manifest_raw = manifest_raw.strip()
        self.manifest = decode(manifest_raw)
        self.version = version
        self.workspace = workspace
        self.public_base = public_base
        self.oss = oss
        # Strict SemVer, including the ban on leading zeroes in numeric prereleases.
        number = r'(?:0|[1-9][0-9]*)'
        identifier = r'(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
        require(re.fullmatch(number + r'\.' + number + r'\.' + number
                             + '(?:-' + identifier + '(?:\\.' + identifier + ')*)?'
                             + r'(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?', version), 'invalid SemVer')
        app = self.manifest.get('appId')
        require(isinstance(app, str) and re.fullmatch(r'[a-zA-Z0-9_-]+', app), 'invalid appId')
        require(self.manifest.get('schemaVersion') == '3.0.0'
                and self.manifest.get('version') == version
                and self.manifest.get('source', {}).get('type') == 'github'
                and self.manifest['source'].get('revision') == 'v' + version,
                'manifest release identity mismatch')
        repository = os.environ.get('GITHUB_REPOSITORY')
        require(not repository or self.manifest['source'].get('repo') == repository,
                'manifest repository mismatch')
        self.source = {'application': {'environmentKey': environment, 'appId': app}, 'version': version}
        self.app_route = APPS + '/' + urllib.parse.quote(app, safe='')
        self.version_route = self.app_route + '/versions/' + urllib.parse.quote(version, safe='')
        require(oss.get('schemaVersion') == '2.0.0' and oss.get('appId') == app
                and oss.get('version') == version and oss.get('releaseTag') == 'v' + version,
                'OSS manifest identity mismatch')
        artifacts = oss.get('artifacts', [])
        require(isinstance(artifacts, list) and artifacts, 'missing release artifacts')
        targets, names = set(), set()
        for artifact in artifacts:
            target = (artifact['platform'], artifact['arch'])
            require(all(re.fullmatch(r'[A-Za-z0-9_-]{1,64}', item) for item in target)
                    and target not in targets, 'duplicate or invalid artifact target')
            targets.add(target)
            name = artifact['name']
            require(re.fullmatch(r'[A-Za-z0-9_-][A-Za-z0-9_.-]{0,190}', name)
                    and name not in names, 'duplicate or unsafe artifact file name')
            names.add(name)
            require(re.fullmatch(r'sha256:[0-9a-f]{64}', artifact['checksum']), 'invalid artifact digest')
            url = artifact['source']
            require(url.startswith(public_base + '/') and https_base(url) == url,
                    'artifact URL outside configured public distribution base')
            require(not any(part in ('.', '..') for part in urllib.parse.unquote(
                urllib.parse.urlsplit(url).path).split('/')), 'unsafe artifact URL path')

    def verify_frozen(self, frozen, raw):
        require(frozen.get('contractVersion') == '1.0.0' and frozen.get('source') == self.source,
                'frozen version source mismatch')
        content = frozen['content']
        require(content.get('applicationType') == 'connector'
                and content.get('sourceRevision') == 'v' + self.version
                and raw_field(raw, ['data', 'content', 'manifest']) == self.manifest_raw,
                'immutable version manifest/revision mismatch')
        expected = {(a['platform'], a['arch']): a for a in self.oss['artifacts']}
        artifacts = content['artifacts']
        require(len(artifacts) == len(expected), 'immutable artifact count mismatch')
        seen = set()
        for artifact in artifacts:
            target = (artifact['platform'], artifact['architecture'])
            require(target in expected and target not in seen, 'immutable artifact target mismatch')
            seen.add(target)
            item = expected[target]
            require(valid_id(artifact['artifactId']) and artifact['fileName'] == item['name'],
                    'immutable artifact identity mismatch')
            data = self.http.api(self.version_route + '/artifacts/' + artifact['artifactId'], binary=True)
            require(len(data) == artifact['sizeBytes'] and len(data) > 0
                    and 'sha256:' + hashlib.sha256(data).hexdigest() == item['checksum'],
                    'immutable artifact bytes mismatch')

    def verify_receipt(self, receipt):
        require(receipt.get('source') == self.source and valid_id(receipt.get('requestId'))
                and valid_id(receipt.get('publicationId')), 'publication receipt identity mismatch')
        require(receipt.get('state') in ('RECEIVING', 'PENDING_REVIEW', 'PUBLISHED'),
                'publication requires explicit action: ' + str(receipt.get('state')))

    def run(self):
        print('Checking source application ownership', flush=True)
        app, _ = self.http.api(self.app_route)
        require(app.get('registered') is True and app.get('applicationType') == 'connector'
                and app.get('source') == self.source['application']
                and app.get('workspaceId') == self.workspace, 'application owner/source mismatch')
        existing = self.http.optional(self.version_route)
        if existing is None:
            # Verify all public bytes before the first owner mutation.
            downloads = []
            for artifact in self.oss['artifacts']:
                status, data = self.http.read(artifact['source'], binary=True)
                require(status == 200 and data and 'sha256:' + hashlib.sha256(data).hexdigest()
                        == artifact['checksum'], 'public artifact checksum mismatch')
                downloads.append((artifact, data))
            print('Uploading verified immutable artifacts to source owner', flush=True)
            uploaded = []
            for artifact, data in downloads:
                route = self.app_route + '/artifacts?' + urllib.parse.urlencode({'fileName': artifact['name']})
                result, _ = self.http.api(route, 'POST', data, binary=True)
                require(valid_id(result.get('artifactId')) and result.get('fileName') == artifact['name']
                        and result.get('sizeBytes') == len(data), 'uploaded artifact identity mismatch')
                uploaded.append(dict(result, platform=artifact['platform'], architecture=artifact['arch']))
            # ManifestDocument is opaque RawValue in the shared owner contract.
            body = ('{"applicationType":"connector","manifest":' + self.manifest_raw
                    + ',"sourceRevision":' + json.dumps('v' + self.version)
                    + ',"artifacts":' + json.dumps(uploaded) + '}').encode('utf-8')
            print('Freezing source version', flush=True)
            try:
                self.http.api(self.version_route, 'POST', body)
            except PublishError:
                # An uncertain write is reconciled by reading its exact immutable identity.
                existing = self.http.optional(self.version_route)
                if existing is None:
                    raise
            else:
                existing = self.http.api(self.version_route)
        print('Verifying frozen manifest and artifact bytes', flush=True)
        self.verify_frozen(*existing)
        route = self.version_route + '/publication'
        publication = self.http.optional(route)
        if publication is not None:
            self.verify_receipt(publication[0])
        if publication is None or publication[0]['state'] == 'RECEIVING':
            print('Submitting source snapshot to market', flush=True)
            # submit reuses the owner's requestId and resumes an interrupted transfer.
            try:
                self.http.api(self.version_route + '/submit', 'POST', b'{}')
            except PublishError:
                publication = self.http.optional(route)
                if publication is None:
                    raise
        for attempt in range(20):
            publication = self.http.optional(route)
            if publication is not None:
                receipt, _ = publication
                self.verify_receipt(receipt)
                state = receipt['state']
                if state in ('PENDING_REVIEW', 'PUBLISHED'):
                    return state
                require(state == 'RECEIVING', 'publication requires explicit action: ' + str(state))
            if attempt < 19:
                time.sleep(3)
        raise PublishError('market receipt did not reach PENDING_REVIEW or PUBLISHED; rerun to reconcile')


def main():
    require(len(sys.argv) == 4, 'usage: publish-market.sh <version> <connector-manifest> <oss-manifest>')
    keys = ('LOCAL_APP_API_BASE_URL', 'LOCAL_APP_SOURCE_ENVIRONMENT_KEY',
            'LOCAL_APP_PUBLIC_ARTIFACT_BASE_URL', 'LOCAL_APP_OWNER_WORKSPACE_ID',
            'LOCAL_APP_MARKET_PUBLISH_TOKEN', 'MARKET_PUBLICATION_STATUS_FILE')
    for key in keys:
        require(bool(os.environ.get(key)), key + ' is required')
    env = os.environ
    require(re.fullmatch(r'[1-9][0-9]*', env['LOCAL_APP_OWNER_WORKSPACE_ID']), 'invalid workspace ID')
    workspace = int(env['LOCAL_APP_OWNER_WORKSPACE_ID'])
    http = Http(https_base(env['LOCAL_APP_API_BASE_URL']), env['LOCAL_APP_MARKET_PUBLISH_TOKEN'], workspace)
    status_file = Path(env['MARKET_PUBLICATION_STATUS_FILE'])
    status_file.unlink(missing_ok=True)
    publisher = Publisher(http, env['LOCAL_APP_SOURCE_ENVIRONMENT_KEY'], workspace,
                          https_base(env['LOCAL_APP_PUBLIC_ARTIFACT_BASE_URL']), sys.argv[1],
                          Path(sys.argv[2]).read_text(), decode(Path(sys.argv[3]).read_bytes()))
    state = publisher.run()
    status_file.write_text(state + '\n')
    print(f'submitted {publisher.source["application"]["appId"]} {publisher.version}; state={state}')


if __name__ == '__main__':
    try:
        main()
    except (PublishError, KeyError, ValueError, TypeError, OSError) as error:
        # Avoid leaking credentials even if an upstream failure includes arbitrary text.
        message = str(error).replace(os.environ.get('LOCAL_APP_MARKET_PUBLISH_TOKEN') or '\0', '[REDACTED]')
        print('publication failed: ' + message, file=sys.stderr)
        sys.exit(1)
