#!/usr/bin/env python3
"""Loopback-only relay and deterministic model fixture for native app acceptance.
The model is a test fixture, not a real LLM. Ctrl+C stops only these fixture processes.
"""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import threading
import time
from urllib.parse import urlparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--directory', type=Path, required=True)
parser.add_argument('--model-port', type=int, default=0)
parser.add_argument('--relay-port', type=int, default=0)
parser.add_argument('--stream-delay', type=float, default=0,
                    help='Pause up to 60 seconds between the first streamed delta and completion.')
parser.add_argument('--empty-response', action='store_true',
                    help='Complete requests without answer text to exercise native error recovery.')
parser.add_argument('--agent-probe', action='store_true',
                    help='Propose a fixed, approval-gated delayed file write in the chosen test workspace.')
parser.add_argument('--resume', action='store_true', help='Reuse saved fixture ports, relay data, and request counts after stopping the previous fixture.')
args = parser.parse_args()
if not 0 <= args.stream_delay <= 60:
    parser.error('--stream-delay must be between 0 and 60 seconds')
os.umask(0o077)
directory = args.directory.resolve()
directory.mkdir(parents=True, exist_ok=True, mode=0o700)
state_path = directory / 'state.json'
if state_path.exists() and not args.resume:
    raise SystemExit('Use a new directory, or --resume after stopping the previous fixture.')
if args.resume and not state_path.exists():
    raise SystemExit('No saved fixture state exists. Start without --resume.')
previous = json.loads(state_path.read_text()) if args.resume else {}
model_port, relay_port = args.model_port, args.relay_port
if not all(0 <= port <= 65535 for port in (model_port, relay_port)):
    raise SystemExit('Fixture ports must be between 0 and 65535.')
if previous:
    model_url, relay_url = (urlparse(previous[key]) for key in ('modelEndpoint', 'relayUrl'))
    if any(url.scheme != 'http' or url.hostname != '127.0.0.1' or not url.port for url in (model_url, relay_url)):
        raise SystemExit('Saved fixture addresses must use loopback HTTP ports.')
    model_port, relay_port = model_url.port, relay_url.port
requests_path = directory / 'model-requests.json'
requests_seen = json.loads(requests_path.read_text()) if args.resume and requests_path.exists() else []
requests_lock = threading.Lock()
class Model(BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def do_GET(self):
        if self.path != '/v1/models':
            self.send_error(404); return
        data = json.dumps({'object': 'list', 'data': [{'id': 'acceptance-model', 'object': 'model'}]}).encode()
        self.send_response(200); self.send_header('Content-Type', 'application/json'); self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data)
    def do_POST(self):
        length = int(self.headers.get('Content-Length', '0'))
        if self.path != '/v1/chat/completions' or not 0 < length <= 32 * 1024 * 1024:
            self.send_error(400); return
        body = json.loads(self.rfile.read(length))
        # Record only fixture-level evidence, not prompt/attachment contents or credentials.
        with requests_lock:
            requests_seen.append({'model': body.get('model'), 'messageCount': len(body.get('messages', [])), 'stream': body.get('stream', False)})
            (directory / 'model-requests.json').write_text(json.dumps(requests_seen, indent=2))
        content = '' if args.empty_response else 'Native acceptance response: the model connection works.'
        agent_probe = args.agent_probe and body.get('tools') and body.get('messages', [{}])[-1].get('role') != 'tool'
        delta = {'content': content}
        if agent_probe:
            delta = {'tool_calls': [{'index': 0, 'id': 'acceptance_lock_probe', 'type': 'function',
                     'function': {'name': 'shell', 'arguments': json.dumps({
                         'command': 'sleep 30; printf acceptance > blackwall-lock-probe.txt'})}}]}
        if body.get('stream'):
            chunks = [json.dumps({'choices': [{'delta': delta}]}), '[DONE]']
            data = ''.join(f'data: {chunk}\n\n' for chunk in chunks).encode()
            content_type = 'text/event-stream'
        else:
            data = json.dumps({'choices': [{'message': {'role': 'assistant', 'content': content}}]}).encode()
            content_type = 'application/json'
        self.send_response(200)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        try:
            if body.get('stream') and args.stream_delay and not agent_probe:
                first, completion = data.split(b'data: [DONE]', 1)
                self.wfile.write(first)
                self.wfile.flush()
                time.sleep(args.stream_delay)
                self.wfile.write(b'data: [DONE]' + completion)
            else:
                self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            # Expected when the tested app cancels its request or locks mid-stream.
            pass

model = ThreadingHTTPServer(('127.0.0.1', model_port), Model)
threading.Thread(target=model.serve_forever, daemon=True).start()
env = os.environ.copy()
env.update(BLACKWALL_RELAY_BIND=f'127.0.0.1:{relay_port}', BLACKWALL_RELAY_DATA_DIR=str(directory / 'relay'), BLACKWALL_RELAY_TOKEN='')
log = (directory / 'relay.log').open('w')
relay = subprocess.Popen([str(ROOT / 'src/target/debug/blackwall-relay')], env=env, stdout=log, stderr=log)
def cleanup():
    relay.terminate()
    try: relay.wait(timeout=5)
    except subprocess.TimeoutExpired: relay.kill(); relay.wait()
    model.shutdown(); log.close()
def stop(*_):
    cleanup()
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
for _ in range(100):
    match = re.search(r'listening on (127\.0\.0\.1:\d+)', (directory / 'relay.log').read_text())
    if match: break
    if relay.poll() is not None:
        cleanup()
        raise SystemExit('Fixture relay failed to start; inspect its local log.')
    time.sleep(.05)
else:
    cleanup()
    raise SystemExit('Fixture relay did not become ready; inspect its local log.')
state = {'fixturePid': os.getpid(), 'relayPid': relay.pid, 'relayUrl': f'http://{match[1]}', 'modelEndpoint': f'http://127.0.0.1:{model.server_port}/v1', 'model': 'acceptance-model'}
(directory / 'state.json').write_text(json.dumps(state, indent=2))
print(json.dumps(state), flush=True)
while True: time.sleep(1)
