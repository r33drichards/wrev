"""Local HTTP MCP discovery smoke test. No credentials/model calls or packages needed.
Run after cargo build: python3 tests/mcp_smoke.py
"""
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import subprocess
import threading
import sys

seen = []
calls = []

class MCP(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.send_error(405)

    def do_DELETE(self):
        self.send_response(200)
        self.end_headers()

    def do_POST(self):
        assert self.headers.get('Authorization') == 'Bearer local-test-credential'
        assert self.headers.get('X-MCP-Session-Id') == 'test-session'
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        method = request['method']
        seen.append(method)
        if 'id' not in request:
            self.send_response(202)
            self.end_headers()
            return
        result = None
        if method == 'initialize':
            result = {'protocolVersion': '2025-11-25', 'capabilities': {'tools': {}},
                      'serverInfo': {'name': 'local-test', 'version': '1'}}
        elif method == 'tools/list':
            second = request.get('params', {}).get('cursor') == 'page2'
            result = {'tools': [{'name': 'second' if second else 'first',
                                 'inputSchema': {'type': 'object', 'properties': {}}}]}
            if not second:
                result['nextCursor'] = 'page2'
        elif method == 'tools/call':
            params = request['params']
            calls.append((params['name'], params['arguments']))
            number = {'first': 19, 'second': 23}[params['name']]
            result = {
                'content': [{'type': 'text', 'text': str(number)}],
                'structuredContent': {'number': number},
            }
        response = {'jsonrpc': '2.0', 'id': request['id']}
        if result is None:
            response['error'] = {'code': -32601, 'message': 'Method not found'}
        else:
            response['result'] = result
        body = json.dumps(response).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = ThreadingHTTPServer(('127.0.0.1', 0), MCP)
threading.Thread(target=server.serve_forever, daemon=True).start()
env = dict(os.environ, MCP_URL=f'http://127.0.0.1:{server.server_port}/mcp')
env['MCP_TOKEN'] = 'local-test-credential'
env['MCP_SESSION_ID'] = 'test-session'
env.pop('MCP_SERVERS_JSON', None)
try:
    command = ['target/debug/minimal-mcp-agent']
    wasmer = len(sys.argv) == 3 and sys.argv[1] == '--wasmer'
    if wasmer:
        command = [os.environ.get('WASMER_BIN', 'wasmer'), 'run', sys.argv[2], '--net']
        for name in ('MCP_URL', 'MCP_TOKEN', 'MCP_SESSION_ID'):
            command.extend(['--env', f'{name}={env[name]}'])
        command.append('--')
    result = subprocess.run(command + ['tools'], env=env,
                            capture_output=True, text=True, timeout=45, check=True)
    assert result.stdout.splitlines() == ['first', 'second'], result.stdout
    assert seen.count('tools/list') == 2, seen
    assert 'initialize' in seen, seen
    print('PASS: exact bearer header, local HTTP connection, legacy fallback, and paginated MCP discovery')

    if wasmer or sys.argv[1:] == ['--quickjs']:
        script = """
const tools = mcp.listTools("default");
if (tools.map(tool => tool.name).join(",") !== "first,second")
    throw new Error("Incomplete tool catalog");
const rejected = await mcp.callTool("default", "undiscovered")
    .then(() => false, () => true);
if (!rejected) throw new Error("Accepted an undiscovered tool");
const first = await mcp.callTool("default", tools[0].name, {index: 0});
const second = await mcp.callTool("default", tools[1].name, {index: 1});
console.log(first.structuredContent.number + second.structuredContent.number);
"""
        result = subprocess.run(command + ['run-js', script], env=env,
                                capture_output=True, text=True, timeout=45)
        assert result.returncode == 0, result.stderr
        assert result.stdout.strip() == '42', result.stdout
        assert calls == [('first', {'index': 0}), ('second', {'index': 1})], calls
        print('PASS: QuickJS discovered and composed two upstream MCP tool calls')
finally:
    server.shutdown()
