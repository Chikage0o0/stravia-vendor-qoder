#!/usr/bin/env python3
"""Observe an official Qoder CLI PTY against a synthetic, network-isolated service.

This is not a live-comparison tool and must not be given production credentials.
The default interactive restore has not been proved safe for live use: read-only
storage alone cannot prevent a refresh request, and an opaque TLS relay cannot
inspect or enforce HTTP path and per-inference limits. A live probe requires a
fail-closed guard before encryption, demonstrated against an isolated synthetic
server, as well as private storage populated from authentic session metadata.
Do not send credentials until those conditions are established.

The 1.1.75 global JavaScript bundle's SDK auth branch maps {"type":"qodercli"}
to read-only storage and refreshStrategy none. It is distinct from the default
interactive entrypoint. A synthetic CN native interactive restart with this
payload did not consume it or initialize that SDK context; it restored using
normal interactive auth. Its print/stream-json execution and network guards
have not been validated by this tool. The SDK payload reader logs its first 32 bytes,
so never put credentials in the payload file.

The separately branded CN CLI is distributed by the official installer at
https://qoder.com.cn/install, whose base is static.qoder.com.cn/qoder-cli-cn.
Its 1.1.75 manifest at /channels/1.1.75/manifest.json identifies the linux
amd64 archive qoderclicn-linux-x64.tar.gz with SHA-256
55d3d947a9e67240677c0847d02252de5210abe2f0bf2debb9bcf58e2b0738cf.
The global 1.1.75 Linux amd64 standalone archive is independently listed by
https://qoder.com/install and its versioned manifest at
https://qoder-ide.oss-accelerate.aliyuncs.com/qodercli/channels/1.1.75/manifest.json;
its archive SHA-256 is
6207478eda10efca0e2bea20c57c3c890c0cc82bd79c149be8f8c95931d72637.
The --executable option runs the verified CN or global native binary only
inside the isolated synthetic network namespace; it is not a live probe.
"""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import tty

HOSTS = ['api2.qoder.sh', 'openapi.qoder.sh', 'center.qoder.sh', 'qoder.com', 'gateway.qoder.com.cn', 'openapi.qoder.com.cn', 'qoder.cn']


class Cursor:
    # Same bounded TLS vectors as the host capture-client hello parser.
    def __init__(self, data):
        self.data, self.pos = data, 0

    def take(self, length):
        if length < 0 or self.pos + length > len(self.data):
            raise ValueError('truncated TLS vector')
        data = self.data[self.pos:self.pos + length]
        self.pos += length
        return data

    def number(self, length):
        return int.from_bytes(self.take(length), 'big')

    def vector(self, length):
        return self.take(self.number(length))

    def done(self):
        return self.pos == len(self.data)


def tls_words(data):
    if len(data) % 2:
        raise ValueError('invalid TLS word vector')
    return [int.from_bytes(data[i:i + 2], 'big') for i in range(0, len(data), 2)]


def client_hello(conn):
    # Peek complete TLS records so SSL receives the original bytes unchanged.
    offset, handshake = 0, bytearray()
    for _ in range(256):
        header = conn.recv(offset + 5, socket.MSG_PEEK | socket.MSG_WAITALL)
        if len(header) < offset + 5 or header[offset] != 22:
            raise ValueError('missing TLS handshake record')
        length = int.from_bytes(header[offset + 3:offset + 5], 'big')
        if length > 18432 or offset + 5 + length > 262144:
            raise ValueError('TLS record limit')
        record = conn.recv(offset + 5 + length, socket.MSG_PEEK | socket.MSG_WAITALL)
        if len(record) < offset + 5 + length:
            raise ValueError('truncated TLS record')
        handshake.extend(record[offset + 5:offset + 5 + length])
        offset += 5 + length
        if len(handshake) >= 4:
            size = int.from_bytes(handshake[1:4], 'big')
            if handshake[0] != 1 or size + 4 > 262144:
                raise ValueError('invalid ClientHello')
            if len(handshake) >= size + 4:
                break
    else:
        raise ValueError('too many TLS records')
    c = Cursor(handshake[4:4 + size])
    legacy = c.number(2)
    c.take(32)  # random is secret-bearing dynamic data
    c.vector(1)  # session ID is never persisted
    suites = tls_words(c.vector(2))
    c.vector(1)
    result = {'legacy_version': legacy, 'server_name': '<absent>', 'cipher_suites': suites, 'extensions': [], 'extension_details': [], 'supported_versions': [], 'alpn': [],
              'supported_groups': [], 'signature_algorithms': [], 'signature_algorithms_cert': [],
              'key_shares': [], 'certificate_compression': [], 'point_formats': [], 'psk_modes': []}
    if c.done():
        return result
    ext = Cursor(c.vector(2))
    if not c.done():
        raise ValueError('invalid TLS extensions length')
    while not ext.done():
        kind, payload = ext.number(2), ext.vector(2)
        result['extensions'].append(kind)
        result['extension_details'].append({'id': kind, 'length': len(payload)})
        if kind == 0:
            p = Cursor(payload)
            names = Cursor(p.vector(2))
            if not p.done():
                raise ValueError('invalid server-name extension')
            while not names.done():
                name_type, name = names.number(1), names.vector(2)
                if name_type == 0:
                    host = name.decode('ascii', 'strict').lower()
                    result['server_name'] = host if host in HOSTS else '<other>'
        elif kind in (16, 43):
            p = Cursor(payload)
            inner = Cursor(p.vector(2 if kind == 16 else 1))
            if not p.done():
                raise ValueError('invalid TLS extension')
            if kind == 16:
                while not inner.done():
                    name = inner.vector(1)
                    result['alpn'].append(name.decode('ascii') if name in (b'h2', b'http/1.1', b'http/1.0', b'h3', b'h3-29') else '<other>')
            else:
                result['supported_versions'] = tls_words(inner.take(len(inner.data)))
        elif kind in (10, 13, 50, 27, 11, 45):
            p = Cursor(payload)
            data = p.vector(1 if kind in (27, 11, 45) else 2)
            if not p.done():
                raise ValueError('invalid TLS extension length')
            key = {10: 'supported_groups', 13: 'signature_algorithms', 50: 'signature_algorithms_cert',
                   27: 'certificate_compression', 11: 'point_formats', 45: 'psk_modes'}[kind]
            result[key] = list(data) if kind in (11, 45) else tls_words(data)
        elif kind == 51:
            p = Cursor(payload)
            shares = Cursor(p.vector(2))
            if not p.done():
                raise ValueError('invalid key-share length')
            while not shares.done():
                group = shares.number(2)
                result['key_shares'].append({'group': group, 'length': len(shares.vector(2))})
    return result


def read_request_body(stream, prefix, length, chunked):
    limit = 8 * 1024 * 1024
    buffered = bytearray(prefix)

    def take(count):
        while len(buffered) < count:
            block = stream.recv(min(65536, count - len(buffered)))
            if not block:
                raise ValueError('truncated HTTP body')
            buffered.extend(block)
        block = bytes(buffered[:count])
        del buffered[:count]
        return block

    def line():
        while b'\r\n' not in buffered:
            if len(buffered) >= 4096:
                raise ValueError('HTTP chunk line limit')
            block = stream.recv(4096 - len(buffered))
            if not block:
                raise ValueError('truncated HTTP chunk')
            buffered.extend(block)
        end = buffered.index(b'\r\n')
        if end > 4096:
            raise ValueError('HTTP chunk line limit')
        return take(end + 2)[:-2]

    if not chunked:
        length = 0 if length is None else length
        if not 0 <= length <= limit:
            raise ValueError('HTTP body limit')
        return take(length)
    if length is not None:
        raise ValueError('ambiguous HTTP framing')
    chunks, total = [], 0
    while True:
        size = line().split(b';', 1)[0]
        if not re.fullmatch(rb'[0-9A-Fa-f]+', size):
            raise ValueError('invalid HTTP chunk size')
        size = int(size, 16)
        if size == 0:
            trailer_bytes = 0
            while trailer := line():
                trailer_bytes += len(trailer) + 2
                if trailer_bytes > 131072:
                    raise ValueError('HTTP trailer limit')
            return b''.join(chunks)
        total += size
        if total > limit:
            raise ValueError('HTTP body limit')
        chunks.append(take(size))
        if take(2) != b'\r\n':
            raise ValueError('invalid HTTP chunk terminator')


SCHEMA_KEYS = frozenset('request_id request_set_id session_id chat_record_id stream chat_task chat_context text features extra context modelConfig key is_reasoning originalContent chatPrompt imageUrls is_reply is_retry source version agent_id task_id session_type aliyun_user_type business id type stage model messages role content tool_calls tool_call_id function name arguments parameters properties required items description thinking tools data input max_tokens temperature top_p user_prompt_id'.split())
BODY_ALPHABET = b'_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!$'
STANDARD_ALPHABET = b'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/='


def safe_schema(value, depth=0):
    if depth >= 12:
        return {'type': 'depth_limit'}
    if isinstance(value, dict):
        return {'type': 'object', 'fields': [{'name': key if key in SCHEMA_KEYS else '<other>', 'schema': safe_schema(item, depth + 1)} for key, item in list(value.items())[:256]], 'field_count': len(value)}
    if isinstance(value, list):
        return {'type': 'array', 'count': len(value), 'items': [safe_schema(item, depth + 1) for item in value[:16]]}
    if value is None:
        return {'type': 'null'}
    if isinstance(value, bool):
        return {'type': 'boolean'}
    if isinstance(value, (int, float)):
        return {'type': 'number'}
    return {'type': 'string'}


def body_schema(body, category):
    if not body:
        return {'encoding': 'empty'}
    encoding = 'json'
    try:
        value = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        if category != 'infer':
            return {'encoding': 'opaque'}
        try:
            encoded = bytearray(body)
            third = len(encoded) // 3
            for i in range(third):
                encoded[i], encoded[len(encoded) - third + i] = encoded[len(encoded) - third + i], encoded[i]
            decoded = base64.b64decode(bytes(encoded).translate(bytes.maketrans(BODY_ALPHABET, STANDARD_ALPHABET)), validate=True)
            value = json.loads(decoded)
            encoding = 'qoder-base64-json'
        except (ValueError, UnicodeDecodeError):
            return {'encoding': 'opaque'}
    return {'encoding': encoding, 'schema': safe_schema(value)}


def has_synthetic_marker(body):
    # Match only this fixture's fixed marker, never output any decoded content.
    try:
        encoded = bytearray(body)
        third = len(encoded) // 3
        for i in range(third):
            encoded[i], encoded[len(encoded) - third + i] = encoded[len(encoded) - third + i], encoded[i]
        decoded = base64.b64decode(bytes(encoded).translate(bytes.maketrans(BODY_ALPHABET, STANDARD_ALPHABET)), validate=True)
        return b'QODER_SYNTHETIC_CAPTURE_MARKER_8274' in decoded
    except ValueError:
        return False


def synthetic_utility_request(body):
    try:
        encoded = bytearray(body)
        third = len(encoded) // 3
        for i in range(third):
            encoded[i], encoded[len(encoded) - third + i] = encoded[len(encoded) - third + i], encoded[i]
        decoded = base64.b64decode(bytes(encoded).translate(bytes.maketrans(BODY_ALPHABET, STANDARD_ALPHABET)), validate=True)
        data = json.loads(decoded)
        return isinstance(data, dict) and isinstance(data.get('tools'), list) and not data['tools']
    except (ValueError, UnicodeDecodeError):
        return False


def inside(args):
    parent_namespaces = json.loads(os.environ.pop('QODER_CAPTURE_PARENT_NAMESPACES'))
    for namespace in ('net', 'mnt', 'user'):
        if os.stat('/proc/self/ns/' + namespace).st_ino == parent_namespaces[namespace]:
            raise RuntimeError('refusing to run outside isolated namespaces')
    root = Path(os.environ['QODER_CAPTURE_ROOT'])
    home = root / 'home'
    home.mkdir(mode=0o700)
    (home / 'capture.txt').write_text('Synthetic local capture fixture.\n')
    if args.executable:
        with open(args.executable, 'rb') as cli_file:
            digest = hashlib.file_digest(cli_file, 'sha256').hexdigest()
        known = {('cn', '1.1.75'): '49d6e014df9fb636bf50d2c2ff68d4a42f6952c8de763e3b065ac2312948a17c',
                 ('global', '1.1.75'): 'f64099d3d9516c4e324df775545c035877551bfd5b4d69650f90422acbcc9289'}
        if known.get((args.site, args.version)) != digest:
            raise ValueError('native executable digest does not match verified official artifact')
        version_env = {'PATH': os.environ['PATH'], 'HOME': str(home), 'XDG_CONFIG_HOME': str(home / 'config'), 'XDG_CACHE_HOME': str(home / 'cache'), 'XDG_DATA_HOME': str(home / 'data')}
        version = subprocess.run([args.executable, '--version'], cwd=home, env=version_env, check=True, capture_output=True, text=True).stdout.strip()
        if version != args.version:
            raise ValueError('executable version mismatch')
        provenance = {'product_version': args.version, 'site': args.site, 'executable_sha256': digest, 'runtime': 'official-native'}
    else:
        node_version = subprocess.run([args.node, '--version'], check=True, capture_output=True, text=True).stdout.strip()
        if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?', node_version):
            raise ValueError('unrecognized Node version')
        with open(args.node, 'rb') as node_file, open(args.package / 'bundle/qodercli.js', 'rb') as cli_file:
            provenance = {'product_version': args.version, 'node_version': node_version, 'node_sha256': hashlib.file_digest(node_file, 'sha256').hexdigest(), 'entry_sha256': hashlib.file_digest(cli_file, 'sha256').hexdigest()}
    (root / 'hosts').write_text('127.0.0.1 localhost ' + ' '.join(HOSTS) + '\n::1 localhost\n')
    (root / 'nsswitch').write_text('hosts: files dns\n')
    (root / 'resolv').write_text('nameserver 127.0.0.1\n')
    for source, dest in [('hosts', '/etc/hosts'), ('nsswitch', '/etc/nsswitch.conf'), ('resolv', '/etc/resolv.conf')]:
        subprocess.run(['mount', '--bind', str(root / source), dest], check=True)
    for dest in ['/run/systemd/resolve/io.systemd.Resolve', '/var/run/nscd/socket']:
        if os.path.exists(dest):
            subprocess.run(['mount', '--bind', '/dev/null', dest], check=True)
    subprocess.run(['ip', 'link', 'set', 'lo', 'up'], check=True)
    token, uid = 'synthetic-token', 'synthetic-user'
    if args.credentials_fd is not None:
        with os.fdopen(args.credentials_fd, 'r') as source:
            data = json.load(source)
        if set(data) != {'token', 'user_id'} or not all(isinstance(v, str) and v.startswith('synthetic-') for v in data.values()):
            raise ValueError('credentials FD accepts only synthetic token and user_id; guest credentials cannot be imported')
        token, uid = data['token'], data['user_id']
    ca, key = root / 'ca.pem', root / 'key.pem'
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(key), '-out', str(ca), '-days', '1', '-subj', '/CN=synthetic Qoder capture', '-addext', 'subjectAltName=' + ','.join('DNS:' + h for h in HOSTS) + ',DNS:localhost,IP:127.0.0.1', '-addext', 'basicConstraints=critical,CA:TRUE'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(ca, key)
    context.set_alpn_protocols(['http/1.1'])
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(('127.0.0.1', 443))
    listener.listen(32)
    listener.settimeout(0.5)
    observations = []
    trace_by_connection = {}
    failures = []
    main_turns = 0
    workers = []
    lock = threading.Lock()
    stop = threading.Event()

    def handle(conn, connection_id):
        nonlocal main_turns
        stage = 'client_hello'
        try:
            conn.settimeout(10)
            hello = client_hello(conn)
            stage = 'tls_handshake'
            with context.wrap_socket(conn, server_side=True) as stream:
                stage = 'http_request'
                buf = b''
                while b'\r\n\r\n' not in buf and len(buf) < 131072:
                    part = stream.recv(65536)
                    if not part:
                        raise ValueError('truncated HTTP headers')
                    buf += part
                headers, body = buf.split(b'\r\n\r\n', 1)
                lines = headers.split(b'\r\n')
                parts = lines[0].decode('ascii', 'strict').split(' ')
                if len(parts) < 2:
                    raise ValueError('invalid HTTP request line')
                fields = []
                length, chunked, host, trace_id, model_key = None, False, '<other>', None, '<other>'
                static_values = {
                    'accept': {'*/*', 'application/json', 'text/event-stream'},
                    'accept-encoding': {'br, gzip, deflate', 'gzip, deflate', 'gzip,deflate', 'gzip, deflate, br', 'gzip, deflate, br, zstd', 'identity'},
                    'cache-control': {'no-cache', 'no-store', 'max-age=0'},
                    'content-encoding': {'gzip', 'br', 'deflate', 'zstd', 'identity'},
                    'transfer-encoding': {'chunked'},
                    'accept-language': {'*'}, 'connection': {'keep-alive', 'close'},
                    'content-type': {'application/json', 'application/json; charset=utf-8', 'text/plain', 'text/plain;charset=UTF-8'},
                    'sec-fetch-mode': {'cors'}, 'user-agent': {'undici'}, 'x-model-key': {'auto', 'lite'},
                }
                for line in lines[1:]:
                    name, sep, value = line.partition(b':')
                    if not sep or not re.fullmatch(rb"[!#$%&'*+.^_`|~0-9A-Za-z-]+", name):
                        raise ValueError('invalid HTTP header')
                    spelling = name.decode('ascii')
                    lower = spelling.lower()
                    text = value.strip().decode('latin1')
                    public_ua = lower == 'user-agent' and re.fullmatch(r'(?:Bun|bun|qoder|QoderCLI|qoder-cli)/[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?', text)
                    fields.append({'name': spelling, 'value': text if public_ua or text in static_values.get(lower, set()) else '<redacted>'})
                    if lower == 'x-model-key' and text in ('auto', 'lite'):
                        model_key = text
                    if lower == 'traceparent':
                        match = re.fullmatch(r'[0-9a-f]{2}-([0-9a-f]{32})-[0-9a-f]{16}-[0-9a-f]{2}', text)
                        trace_id = match.group(1) if match else None
                    if lower == 'host' and text.lower() in HOSTS:
                        host = text.lower()
                    if lower == 'content-length':
                        if length is not None and length != int(text):
                            raise ValueError('conflicting HTTP content lengths')
                        length = int(text)
                    elif lower == 'transfer-encoding':
                        if chunked or text.lower() != 'chunked':
                            raise ValueError('unsupported HTTP transfer encoding')
                        chunked = True
                body = read_request_body(stream, body, length, chunked)
                path = parts[1].split('?', 1)[0]
                category = next((name for fragment, name in [('/deviceToken/poll', 'device_poll'), ('/userinfo', 'userinfo'), ('/model/list', 'model_list'), ('agent_chat_generation', 'infer')] if fragment in path), 'other')
                with lock:
                    turn = 0
                    if category == 'infer' and len(body) > 30000:
                        # Previous 1.1.75 PTY experiment used this size heuristic;
                        # it is a candidate, not proof for future CLI versions.
                        main_turns += 1
                        turn = main_turns
                    if trace_id:
                        trace_by_connection[connection_id] = trace_id
                    safe_paths = {'/algo/api/v2/service/pro/sse/agent_chat_generation', '/api/v1/deviceToken/poll', '/api/v1/userinfo', '/api/v2/model/list'}
                    observations.append({'connection_id': connection_id, 'origin': 'https://' + host if host != '<other>' else '<other>', 'path': path if path in safe_paths else '<other>', 'http_version': parts[2] if len(parts) == 3 and parts[2] in ('HTTP/1.0', 'HTTP/1.1') else '<other>', 'operation': category, 'synthetic_prompt_marker': category == 'infer' and has_synthetic_marker(body), 'large_body_heuristic': bool(turn), 'method': parts[0] if parts[0] in ('GET', 'POST') else 'other', 'client_hello': hello, 'headers': fields, 'body_bytes': len(body), 'body_sha256': hashlib.sha256(body).hexdigest(), 'body_schema': body_schema(body, category)})
                payloads = {
                    'device_poll': {'token': token, 'user_id': uid, 'user_name': 'Synthetic', 'expires_in': 86400, **({} if args.no_refresh_fixture else {'refresh_token': 'synthetic-refresh', 'refresh_token_expires_in': 864000})},
                    'userinfo': {'id': uid, 'name': 'Synthetic'},
                    'model_list': {'assistant': [{'key': 'auto', 'display_name': 'Auto', 'format': 'openai', 'is_default': True, 'enable': True, 'max_input_tokens': 128000, 'max_output_tokens': 8192}]},
                }
                if category == 'infer':
                    if turn == 1 and not args.no_refresh_fixture:
                        chunk = {'id': 'synthetic-response', 'object': 'chat.completion.chunk', 'model': 'auto', 'choices': [{'index': 0, 'delta': {'role': 'assistant', 'tool_calls': [{'index': 0, 'id': 'synthetic-call', 'type': 'function', 'function': {'name': 'Read', 'arguments': json.dumps({'file_path': str(home / 'capture.txt')})}}]}, 'finish_reason': None}]}
                        finish = {'id': 'synthetic-response', 'object': 'chat.completion.chunk', 'model': 'auto', 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'tool_calls'}]}
                        payload = ('data: ' + json.dumps(chunk, separators=(',', ':')) + '\n\ndata: ' + json.dumps(finish, separators=(',', ':')) + '\n\ndata: [DONE]\n\n').encode()
                    elif args.no_refresh_fixture and synthetic_utility_request(body):
                        content = 'Synthetic suggestion.' if model_key == 'lite' else json.dumps({'title': 'Synthetic title'})
                        chunk = {'id': 'synthetic', 'choices': [{'index': 0, 'delta': {'content': content}, 'finish_reason': 'stop'}]}
                        payload = ('data: ' + json.dumps(chunk, separators=(',', ':')) + '\n\ndata: [DONE]\n\n').encode()
                    else:
                        payload = b'data: {"id":"synthetic","choices":[{"index":0,"delta":{"content":"Synthetic answer."},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
                    ctype, status = 'text/event-stream', '200 OK'
                else:
                    payload = json.dumps(payloads.get(category, {'success': False, 'message': 'synthetic local capture'})).encode()
                    ctype, status = 'application/json', '200 OK' if category in payloads else '404 Not Found'
                stage = 'synthetic_response'
                stream.sendall(f'HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nConnection: close\r\nContent-Length: {len(payload)}\r\n\r\n'.encode() + payload)
        except Exception as error:
            with lock:
                failures.append({'stage': stage, 'code': 'capture_failed', 'kind': type(error).__name__})
        finally:
            conn.close()

    def serve():
        connection_id = 0
        while not stop.is_set():
            try:
                conn, _ = listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            connection_id += 1
            worker = threading.Thread(target=handle, args=(conn, connection_id), daemon=True)
            workers.append(worker)
            worker.start()

    server = threading.Thread(target=serve, daemon=True)
    server.start()
    env = {'PATH': os.environ['PATH'], 'HOME': str(home), 'XDG_CONFIG_HOME': str(home / 'config'), 'XDG_CACHE_HOME': str(home / 'cache'), 'XDG_DATA_HOME': str(home / 'data'), 'LANG': 'C.UTF-8', 'TERM': 'xterm-256color', 'NODE_EXTRA_CA_CERTS': str(ca)}
    if args.executable:
        runtime_probe = home / 'synthetic-runtime-probe.js'
        runtime_probe.write_text("require('node:fs').writeFileSync(process.env.QODER_SYNTHETIC_RUNTIME, JSON.stringify({bun: Bun.version, node: process.versions.node}), {mode: 0o600});\n")
        if any(ch.isspace() for ch in str(runtime_probe)):
            raise ValueError('native runtime probe path cannot contain whitespace')
        env['BUN_OPTIONS'] = '--preload=' + str(runtime_probe)
        env['QODER_SYNTHETIC_RUNTIME'] = str(home / 'synthetic-runtime.json')
    def run_client():
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(home)
            if args.executable:
                os.execve(args.executable, [args.executable], env)
            os.execve(args.node, [args.node, str(args.package / 'bundle/qodercli.js')], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
        try:
            while True:
                ready, _, _ = select.select([fd, sys.stdin], [], [])
                if fd in ready:
                    try:
                        data = os.read(fd, 65536)
                    except OSError:
                        break
                    if not data:
                        break
                    os.write(sys.stdout.fileno(), data)
                if sys.stdin in ready:
                    data = os.read(sys.stdin.fileno(), 4096)
                    if not data:
                        break
                    os.write(fd, data)
        finally:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
            os.close(fd)

    old = termios.tcgetattr(sys.stdin.fileno())
    try:
        tty.setraw(sys.stdin.fileno())
        run_client()
        if args.restore_readonly:
            auth = home / ('.qoder-cn' if args.site == 'cn' else '.qoder') / '.auth'
            if not (auth / 'user').is_file():
                raise RuntimeError('synthetic login did not produce an auth store')
            subprocess.run(['mount', '--bind', str(auth), str(auth)], check=True)
            subprocess.run(['mount', '-o', 'remount,bind,ro', str(auth)], check=True)
            if args.sdk_auth_interactive:
                payload = home / 'synthetic-sdk-auth.json'
                payload.write_text('{"type":"qodercli"}')
                env['QODER_SDK_AUTH_PAYLOAD_FILE'] = str(payload)
                env['QODER_SDK_AUTH_CONFIG_DIR'] = str(auth.parent)
            os.write(sys.stdout.fileno(), b'\r\nSynthetic read-only auth store: restarting client.\r\n')
            run_client()
    finally:
        termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, old)
        stop.set()
        listener.close()
        server.join(timeout=2)
        for worker in workers:
            worker.join(timeout=11)
        if server.is_alive() or any(worker.is_alive() for worker in workers):
            failures.append({'stage': 'shutdown', 'code': 'capture_worker_timeout'})
        trace_sets, main_sets = {}, set()
        sdk_context_initialized = False
        sdk_readonly_loaded = False
        for log_path in home.glob('.qoder*/logs/runs/*/qodercli.log'):
            with log_path.open(errors='replace') as log_file:
                for line in log_file:
                    if '[sdk-auth-init] Set context: storage=readOnly, refresh=none' in line:
                        sdk_context_initialized = True
                    if '[auth] SDK read-only credentials loaded from store' in line:
                        sdk_readonly_loaded = True
                    if 'operation=sendRemoteChatAsk method=POST' in line:
                        trace = re.search(r'\btraceId=([0-9a-f]{32})\b', line)
                        request_set = re.search(r'\brequestSetId=([0-9a-f-]{36})\b', line)
                        if trace and request_set:
                            trace_sets[trace.group(1)] = request_set.group(1)
                    if 'model.request.preparation' in line:
                        match = re.search(r'\brequest_set_id="([0-9a-f-]{36})"', line)
                        if match:
                            main_sets.add(match.group(1))
        for item in observations:
            request_set = trace_sets.get(trace_by_connection.get(item['connection_id']))
            item['operation_context'] = 'main_agent_loop' if item['operation'] == 'infer' and request_set in main_sets and request_set is not None else 'unclassified'
        if not any(item['operation'] == 'infer' for item in observations):
            failures.append({'stage': 'infer', 'code': 'infer_not_observed'})
        if args.executable:
            try:
                runtime = json.loads((home / 'synthetic-runtime.json').read_text())
                if set(runtime) != {'bun', 'node'} or not all(isinstance(v, str) and re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?', v) for v in runtime.values()):
                    raise ValueError('invalid runtime versions')
                provenance['runtime_versions'] = runtime
            except (OSError, ValueError, TypeError):
                failures.append({'stage': 'runtime_provenance', 'code': 'runtime_probe_failed'})
        if args.sdk_auth_interactive:
            provenance['sdk_auth_context_initialized'] = sdk_context_initialized
            provenance['sdk_auth_readonly_loaded'] = sdk_readonly_loaded
        report = {'client': 'qoder-cli', 'provenance': provenance, 'status': 'failed' if failures else 'captured', 'boundary': 'synthetic isolated PTY; no upstream success established', 'failures': failures, 'requests': observations}
        Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
    if failures:
        raise SystemExit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    entry = parser.add_mutually_exclusive_group(required=True)
    entry.add_argument('--package', type=Path, help='extracted official npm package directory (Node entrypoint)')
    entry.add_argument('--executable', type=Path, help='verified official native Linux amd64 1.1.75 executable')
    parser.add_argument('--site', choices=('cn', 'global'), help='required with --executable; selects the matching native artifact and auth directory')
    parser.add_argument('--version', required=True, help='expected official package or native executable version')
    parser.add_argument('--node', default=shutil.which('node'), help='Node executable for --package only')
    parser.add_argument('--credentials-fd', type=int, help='optional inherited JSON FD with synthetic-only token and user_id; never production credentials')
    parser.add_argument('--output', required=True, type=Path, help='path for the redacted synthetic capture report')
    parser.add_argument('--temp-parent', type=Path, help='parent for the automatically removed private temporary HOME; /dev/shm uses tmpfs where available')
    parser.add_argument('--sdk-auth-interactive', action='store_true', help='with --restore-readonly, test a token-free SDK payload on the default interactive restart; does not establish SDK auth')
    parser.add_argument('--restore-readonly', action='store_true', help='after exiting synthetic login, restart the CLI with its synthetic auth directory bind-mounted read-only; exit the second session too')
    parser.add_argument('--no-refresh-fixture', action='store_true', help='omit the synthetic refresh token and return direct synthetic answers, including utility responses; does not limit requests')
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.sdk_auth_interactive and not args.restore_readonly:
        parser.error('--sdk-auth-interactive requires --restore-readonly')
    args.output = args.output.resolve()
    if args.executable:
        args.executable = str(args.executable.resolve())
        if not args.site:
            parser.error('--site is required for a native executable')
    else:
        args.package = args.package.resolve()
        if not args.node:
            parser.error('node not found')
        args.node = str(Path(args.node).resolve())
        manifest = json.loads((args.package / 'package.json').read_text())
        if manifest.get('name') != '@qoder-ai/qodercli' or manifest.get('version') != args.version:
            parser.error('package name/version mismatch')
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        parser.error('interactive PTY required')
    if args.inside:
        try:
            inside(args)
        except Exception as error:
            Path(args.output).write_text(json.dumps({'client': 'qoder-cli', 'status': 'failed', 'failures': [{'stage': 'setup_or_pty', 'code': 'capture_failed', 'kind': type(error).__name__}], 'requests': []}, indent=2) + '\n')
            raise SystemExit(1) from None
        return
    os.umask(0o077)
    args.output.write_text(json.dumps({'client': 'qoder-cli', 'status': 'failed', 'failures': [{'stage': 'isolation', 'code': 'capture_not_started'}], 'requests': []}, indent=2) + '\n')
    with tempfile.TemporaryDirectory(prefix='qoder-capture-', dir=args.temp_parent) as root:
        # PID 1 namespace handles are not readable by an unprivileged user.
        # Compare with our own namespace snapshot taken before unshare instead.
        parent_namespaces = {name: os.stat('/proc/self/ns/' + name).st_ino for name in ('net', 'mnt', 'user')}
        env = dict(os.environ, QODER_CAPTURE_ROOT=root,
                   QODER_CAPTURE_PARENT_NAMESPACES=json.dumps(parent_namespaces))
        command = ['unshare', '--user', '--map-root-user', '--mount', '--net', '--fork', sys.executable, str(Path(__file__).resolve()), '--inside', '--version', args.version, '--output', str(args.output)]
        if args.executable:
            command += ['--executable', args.executable, '--site', args.site]
        else:
            command += ['--package', str(args.package), '--node', args.node]
        if args.sdk_auth_interactive:
            command.append('--sdk-auth-interactive')
        if args.restore_readonly:
            command.append('--restore-readonly')
        if args.no_refresh_fixture:
            command.append('--no-refresh-fixture')
        fds = ()
        if args.credentials_fd is not None:
            command += ['--credentials-fd', str(args.credentials_fd)]
            fds = (args.credentials_fd,)
        subprocess.run(command, env=env, pass_fds=fds, check=True)


if __name__ == '__main__':
    main()
