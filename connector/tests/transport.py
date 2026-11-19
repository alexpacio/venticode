"""Exercise the packaged Camel route against an owned HTTPS GitLab stand-in."""
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
    def log_message(self, *_):
        pass

    def handle_operation(self):
        assert self.headers['PRIVATE-TOKEN'] == 'synthetic-token'
        length = int(self.headers.get('Content-Length', 0))
        body = json.loads(self.rfile.read(length)) if length else None
        self.server.received.append((self.command, self.path, body))
        self.send_response(204 if self.command == 'DELETE' else 200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        if self.command != 'DELETE':
            self.wfile.write(b'{"id":17}')

    do_GET = do_PUT = do_DELETE = do_POST = handle_operation


def main():
    connector = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory() as temp:
        cwd = Path(temp)
        subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1',
                        '-subj','/CN=localhost','-addext','subjectAltName=IP:127.0.0.1',
                        '-keyout','key.pem','-out','cert.pem'],cwd=cwd,capture_output=True,check=True)
        subprocess.run(['keytool','-importcert','-noprompt','-alias','fixture','-file','cert.pem',
                        '-keystore','trust.p12','-storepass','synthetic'],cwd=cwd,capture_output=True,check=True)
        with ThreadingHTTPServer(('127.0.0.1',0),GitLab) as upstream:
            context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(cwd/'cert.pem',cwd/'key.pem')
            upstream.socket=context.wrap_socket(upstream.socket,server_side=True)
            upstream.received=[]
            threading.Thread(target=upstream.serve_forever,daemon=True).start()
            with socket.socket() as s:
                s.bind(('127.0.0.1',0))
                port=s.getsockname()[1]
            proc=subprocess.Popen(['java','-Djavax.net.ssl.trustStore='+str(cwd/'trust.p12'),
                                   '-Djavax.net.ssl.trustStorePassword=synthetic','-cp','target/classes:target/dependency/*',
                                   'io.ventic.venticode.Connector'],cwd=connector,
                                  env={**os.environ,'GITLAB_ORIGIN':f'https://127.0.0.1:{upstream.server_port}',
                                       'CONNECTOR_PORT':str(port)})
            try:
                for _ in range(100):
                    if proc.poll() is not None: raise RuntimeError('Connector failed to start')
                    try:
                        with socket.create_connection(('127.0.0.1',port),.25): break
                    except OSError: time.sleep(.1)
                with request.urlopen(f'http://127.0.0.1:{port}/healthz', timeout=4) as res:
                    assert res.status == 200 and res.read() == b'ok'
                def call(op, **fields):
                    body=dict(kind='gitlab',operation=op,destination='42',credentials={'token':'synthetic-token','webhook_secret':'fixture'},**fields)
                    req=request.Request(f'http://127.0.0.1:{port}/exchange',data=json.dumps(body).encode(),headers={'Content-Type':'application/json'})
                    with request.urlopen(req,timeout=8) as res: return json.load(res)
                assert call('identify')=={'id':17}
                assert call('hooks',page='2')=={'id':17}
                assert call('install',hook_id=3,webhook_url='https://fixture.invalid/hook')=={'id':17}
                assert call('remove',hook_id='3')=={}
                assert call('reply',object_type='issues',iid='4',thread='a123',text='Synthetic reply')=={'id':17}
                assert [(verb,path) for verb,path,_ in upstream.received]==[
                    ('GET','/api/v4/user'),('GET','/api/v4/projects/42/hooks?per_page=100&page=2'),
                    ('PUT','/api/v4/projects/42/hooks/3'),('DELETE','/api/v4/projects/42/hooks/3'),
                    ('POST','/api/v4/projects/42/issues/4/discussions/a123/notes')],[(v,p) for v,p,_ in upstream.received]
                assert upstream.received[-1][2]=={'body':'Synthetic reply'}
                try: call('unsupported')
                except error.HTTPError as err: assert err.code==400
                else: raise AssertionError('Unexpected connector operation accepted')
                print('Camel HTTPS connector operations passed')
            finally:
                proc.terminate()
                proc.wait(timeout=20)
                upstream.shutdown()

if __name__=='__main__':
    main()
