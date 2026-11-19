"""Exercise the packaged Camel route against an owned HTTPS GitLab stand-in."""
from http import client as http_client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
import threading
import time
from urllib import error, request


class GitLab(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def handle_operation(self):
        assert self.headers['PRIVATE-TOKEN'] == 'synthetic-token'
        length = int(self.headers.get('Content-Length', 0))
        body = json.loads(self.rfile.read(length)) if length else None
        self.server.received.append((self.command, self.path, body))
        if self.server.scripted:
            status, headers, payload = self.server.scripted.pop(0)
            self.send_response(status)
            for k, v in headers.items():
                self.send_header(k, v)
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            if payload:
                try:
                    self.wfile.write(payload)
                except OSError:
                    pass
            return
        if self.command == 'DELETE':
            self.send_response(204)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        payload = b'{"id":17}'
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    do_GET = do_PUT = do_DELETE = do_POST = handle_operation


def raw_post(port, data, headers=None):
    req = request.Request(
        f'http://127.0.0.1:{port}/exchange',
        data=data,
        headers=headers or {'Content-Type': 'application/json'},
    )
    try:
        with request.urlopen(req, timeout=8) as res:
            return res.status, dict(res.headers.items()), res.read()
    except error.HTTPError as err:
        return err.code, dict(err.headers.items()), err.read()


def main():
    connector = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory() as temp:
        cwd = Path(temp)
        subprocess.run(
            ['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
             '-subj', '/CN=localhost', '-addext', 'subjectAltName=IP:127.0.0.1',
             '-keyout', 'key.pem', '-out', 'cert.pem'],
            cwd=cwd, capture_output=True, check=True,
        )
        subprocess.run(
            ['keytool', '-importcert', '-noprompt', '-alias', 'fixture', '-file', 'cert.pem',
             '-keystore', 'trust.p12', '-storepass', 'synthetic'],
            cwd=cwd, capture_output=True, check=True,
        )
        with ThreadingHTTPServer(('127.0.0.1', 0), GitLab) as upstream:
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(cwd / 'cert.pem', cwd / 'key.pem')
            upstream.socket = context.wrap_socket(upstream.socket, server_side=True)
            upstream.received = []
            upstream.scripted = []
            threading.Thread(target=upstream.serve_forever, daemon=True).start()
            with socket.socket() as s:
                s.bind(('127.0.0.1', 0))
                port = s.getsockname()[1]
            stderr_log = cwd / 'connector.stderr.log'
            with stderr_log.open('wb') as err_file:
                proc = subprocess.Popen(
                    ['java', '-Djavax.net.ssl.trustStore=' + str(cwd / 'trust.p12'),
                     '-Djavax.net.ssl.trustStorePassword=synthetic', '-cp', 'target/classes:target/dependency/*',
                     'io.ventic.venticode.Connector'],
                    cwd=connector,
                    stderr=err_file,
                    env={**os.environ, 'GITLAB_ORIGIN': f'https://127.0.0.1:{upstream.server_port}',
                         'CONNECTOR_PORT': str(port)},
                )
                try:
                    for _ in range(100):
                        if proc.poll() is not None:
                            raise RuntimeError('Connector failed to start')
                        try:
                            with socket.create_connection(('127.0.0.1', port), .25):
                                break
                        except OSError:
                            time.sleep(.1)

                    with request.urlopen(f'http://127.0.0.1:{port}/healthz', timeout=4) as res:
                        assert res.status == 200 and res.read() == b'ok'

                    def call(op, **fields):
                        body = {
                            'kind': 'gitlab', 'operation': op, 'destination': '42',
                            'credentials': {'token': 'synthetic-token', 'webhook_secret': 'fixture'},
                            **fields,
                        }
                        req = request.Request(
                            f'http://127.0.0.1:{port}/exchange',
                            data=json.dumps(body).encode(),
                            headers={'Content-Type': 'application/json'},
                        )
                        with request.urlopen(req, timeout=8) as res:
                            return json.load(res)

                    def call_raw(op, **fields):
                        body = {
                            'kind': 'gitlab', 'operation': op, 'destination': '42',
                            'credentials': {'token': 'synthetic-token', 'webhook_secret': 'fixture'},
                            **fields,
                        }
                        return raw_post(port, json.dumps(body).encode())

                    # 1. Existing operations
                    assert call('identify') == {'id': 17}
                    assert call('hooks', page='2') == {'id': 17}
                    assert call('install', hook_id=3, webhook_url='https://fixture.invalid/hook') == {'id': 17}
                    assert call('remove', hook_id='3') == {}
                    assert call('reply', object_type='issues', iid='4', thread='a123', text='Synthetic reply') == {'id': 17}
                    assert [(verb, path) for verb, path, _ in upstream.received] == [
                        ('GET', '/api/v4/user'),
                        ('GET', '/api/v4/projects/42/hooks?per_page=100&page=2'),
                        ('PUT', '/api/v4/projects/42/hooks/3'),
                        ('DELETE', '/api/v4/projects/42/hooks/3'),
                        ('POST', '/api/v4/projects/42/issues/4/discussions/a123/notes'),
                    ], [(v, p) for v, p, _ in upstream.received]
                    assert upstream.received[-1][2] == {'body': 'Synthetic reply'}
                    status, _, _ = call_raw('unsupported')
                    assert status == 400, f'Expected 400 for unsupported op, got {status}'

                    # 2. Malformed / empty / non-object JSON -> 400 (not 502)
                    for bad_payload in (b'', b'   ', b'{not-json', b'[1,2,3]', b'"scalar"', b'null'):
                        st, _, raw = raw_post(port, bad_payload)
                        assert st == 400, f'Expected 400 for {bad_payload!r}, got {st} ({raw!r})'

                    # 3. Inbound numeric Long overflow and Telegram 4096-char text limit vs longer GitLab notes
                    st, _, _ = call_raw('inspect', destination='99999999999999999999')
                    assert st == 400, f'Expected 400 for 20-digit destination, got {st}'
                    st, _, _ = call_raw('remove', hook_id='9223372036854775808')
                    assert st == 400, f'Expected 400 for Long.MAX_VALUE+1 hook_id, got {st}'

                    tg_too_long = {
                        'kind': 'telegram',
                        'operation': 'reply',
                        'destination': '12345',
                        'thread': '99',
                        'text': 'a' * 4097,
                        'credentials': {'token': '123456:ABCDEF_ghijk'},
                    }
                    st, _, _ = raw_post(port, json.dumps(tg_too_long).encode())
                    assert st == 400, f'Expected 400 for Telegram >4096 chars, got {st}'

                    long_gitlab_text = 'g' * 70_000
                    assert call('reply', object_type='issues', iid='4', thread='root', text=long_gitlab_text) == {'id': 17}
                    assert upstream.received[-1][2] == {'body': long_gitlab_text}

                    # 4. Non-JSON upstream 429 preserves 429, forwards sanitized Retry-After, strips other headers
                    upstream.scripted.append((
                        429,
                        {'Content-Type': 'text/html', 'Retry-After': '45', 'X-Upstream-Secret': 'leaked', 'Set-Cookie': 's=1'},
                        b'<html><body>Too Many Requests</body></html>',
                    ))
                    st, hdrs, raw = call_raw('identify')
                    lower_hdrs = {k.lower(): v for k, v in hdrs.items()}
                    assert st == 429, f'Expected 429, got {st}'
                    assert lower_hdrs.get('retry-after') == '45', f'Expected Retry-After 45, got {lower_hdrs}'
                    assert 'x-upstream-secret' not in lower_hdrs, f'Leaked upstream header: {lower_hdrs}'
                    assert 'set-cookie' not in lower_hdrs, f'Leaked Set-Cookie header: {lower_hdrs}'
                    assert json.loads(raw)['retry_after'] == 45

                    # Retry-After clamping (upper bound 300, lower bound 1) and Telegram parameters.retry_after fallback
                    upstream.scripted.append((
                        429,
                        {'Content-Type': 'text/plain', 'Retry-After': '9999'},
                        b'slow down',
                    ))
                    st, hdrs, raw = call_raw('identify')
                    assert st == 429 and {k.lower(): v for k, v in hdrs.items()}.get('retry-after') == '300'
                    assert json.loads(raw)['retry_after'] == 300

                    upstream.scripted.append((
                        429,
                        {'Content-Type': 'application/json', 'Retry-After': 'not-a-number'},
                        b'{"ok":false,"parameters":{"retry_after":27}}',
                    ))
                    st, hdrs, raw = call_raw('identify')
                    assert st == 429 and {k.lower(): v for k, v in hdrs.items()}.get('retry-after') == '27'
                    assert json.loads(raw)['retry_after'] == 27

                    # Non-JSON upstream 401 and 403 preserve real status and strip upstream headers
                    upstream.scripted.append((
                        401,
                        {'Content-Type': 'text/html', 'X-Upstream-Secret': 'leaked'},
                        b'<html>Unauthorized</html>',
                    ))
                    st, hdrs, _ = call_raw('identify')
                    assert st == 401, f'Expected 401, got {st}'
                    assert 'x-upstream-secret' not in {k.lower() for k in hdrs}

                    upstream.scripted.append((
                        403,
                        {'Content-Type': 'text/plain'},
                        b'Forbidden',
                    ))
                    st, _, _ = call_raw('identify')
                    assert st == 403, f'Expected 403, got {st}'

                    # 5. Idempotency cache for reply: same idempotency_key hits upstream once and replays cached response
                    before = len(upstream.received)
                    upstream.scripted.append((200, {'Content-Type': 'application/json'}, b'{"id":101}'))
                    first = call('reply', object_type='issues', iid='4', thread='a123', text='Idempotent hello', idempotency_key='reply-key-1')
                    second = call('reply', object_type='issues', iid='4', thread='a123', text='Idempotent hello', idempotency_key='reply-key-1')
                    assert first == {'id': 101} and second == {'id': 101}
                    assert len(upstream.received) == before + 1, f'Expected 1 upstream hit, got {len(upstream.received) - before}'

                    # Different idempotency_key hits upstream again
                    upstream.scripted.append((200, {'Content-Type': 'application/json'}, b'{"id":102}'))
                    third = call('reply', object_type='issues', iid='4', thread='a123', text='Idempotent hello 2', idempotency_key='reply-key-2')
                    assert third == {'id': 102}
                    assert len(upstream.received) == before + 2

                    # 6. Idempotent op retries on 503; reply never retries automatically on 503 and does not cache failure
                    before = len(upstream.received)
                    upstream.scripted.append((503, {'Content-Type': 'text/plain'}, b'unavailable'))
                    upstream.scripted.append((200, {'Content-Type': 'application/json'}, b'{"id":55}'))
                    assert call('identify') == {'id': 55}
                    assert len(upstream.received) == before + 2

                    before = len(upstream.received)
                    upstream.scripted.append((503, {'Content-Type': 'text/plain'}, b'unavailable'))
                    st, _, _ = call_raw('reply', object_type='issues', iid='4', thread='a123', text='No auto retry', idempotency_key='reply-fail-1')
                    assert st == 502, f'Expected 502 for reply 503, got {st}'
                    assert len(upstream.received) == before + 1, 'reply must not automatically retry on 503'
                    # Retrying explicitly later succeeds (failure was not cached)
                    upstream.scripted.append((200, {'Content-Type': 'application/json'}, b'{"id":103}'))
                    assert call('reply', object_type='issues', iid='4', thread='a123', text='No auto retry', idempotency_key='reply-fail-1') == {'id': 103}
                    assert len(upstream.received) == before + 2

                    # 7. Oversized inbound request (> 2 MiB) rejected cleanly with 413 (both Content-Length and chunked)
                    oversized_body = json.dumps({
                        'kind': 'gitlab', 'operation': 'reply', 'destination': '42',
                        'object_type': 'issues', 'iid': '4', 'thread': 'root',
                        'text': 'x' * (2 * 1024 * 1024 + 4096),
                        'credentials': {'token': 'synthetic-token'},
                    }).encode()
                    st, _, _ = raw_post(port, oversized_body)
                    assert st == 413, f'Expected 413 for oversized Content-Length request, got {st}'

                    conn = http_client.HTTPConnection('127.0.0.1', port, timeout=8)
                    try:
                        conn.putrequest('POST', '/exchange')
                        conn.putheader('Content-Type', 'application/json')
                        conn.putheader('Transfer-Encoding', 'chunked')
                        conn.endheaders()
                        chunk = b'a' * (64 * 1024)
                        try:
                            for _ in range(34):  # > 2 MiB
                                conn.send(f'{len(chunk):x}\r\n'.encode() + chunk + b'\r\n')
                            conn.send(b'0\r\n\r\n')
                        except OSError:
                            pass
                        resp = conn.getresponse()
                        resp.read()
                        assert resp.status == 413, f'Expected 413 for oversized chunked request, got {resp.status}'
                    finally:
                        conn.close()

                    # 8. Oversized upstream response (> 2 MiB) fails safely with 502 without OOM/hang
                    huge_upstream = b'{"data":"' + (b'z' * (2 * 1024 * 1024 + 4096)) + b'"}'
                    upstream.scripted.append((200, {'Content-Type': 'application/json'}, huge_upstream))
                    st, _, _ = call_raw('identify')
                    assert st == 502, f'Expected 502 for oversized upstream response, got {st}'
                    # Connector remains healthy after oversized upstream response
                    assert call('identify') == {'id': 17}

                    print('Camel HTTPS connector operations passed')
                finally:
                    proc.terminate()
                    proc.wait(timeout=20)
                    upstream.shutdown()

            # 9. Verify sanitized structured logs contain no secrets/bodies/URIs
            stderr_text = stderr_log.read_text('utf-8', errors='replace')
            for forbidden in ('synthetic-token', 'ABCDEF_ghijk', 'fixture', 'Synthetic reply', 'X-Upstream-Secret', 'leaked', '/api/v4/'):
                assert forbidden not in stderr_text, f'Sensitive data {forbidden!r} leaked into connector logs'
            json_lines = [line for line in stderr_text.splitlines() if line.startswith('{')]
            assert len(json_lines) >= 15, f'Expected structured log lines, got {len(json_lines)}'
            expected_keys = {'kind', 'operation', 'status', 'latencyMs', 'exception'}
            for line in json_lines:
                entry = json.loads(line)
                assert set(entry.keys()) == expected_keys, f'Unexpected log keys: {entry}'
                assert isinstance(entry['status'], int)
                assert isinstance(entry['latencyMs'], int)


if __name__ == '__main__':
    main()
