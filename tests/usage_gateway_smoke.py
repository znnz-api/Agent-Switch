"""Offline Windows integration test. Never starts or closes an AI client.

Usage: python tests/usage_gateway_smoke.py NEW_EXE OLD_EXE
All gateway state and SQLite data are isolated in a temporary LOCALAPPDATA.
"""
import concurrent.futures
from contextlib import closing
import ctypes
import faulthandler
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send(self, status, data, content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.send(200, b'{"data":[]}')

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        assert self.headers["Authorization"] in ("Bearer key-a", "Bearer key-b")
        if b"fail" in body:
            self.send(502, b'{"error":{"message":"fixture failure"}}')
        elif self.path.endswith("/messages"):
            self.send(200, b'data: {"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":800,"cache_creation_input_tokens":100}}}\n\ndata: {"type":"message_delta","usage":{"output_tokens":20}}\n\ndata: {"type":"message_stop"}\n\n', "text/event-stream")
        else:
            self.send(200, b'{"usage":{"input_tokens":1000,"output_tokens":20,"input_tokens_details":{"cached_tokens":800}}}')


def wait_for(read, predicate, seconds=12):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            result = read()
            if predicate(result):
                return result
        except (OSError, ValueError, sqlite3.Error):
            pass
        time.sleep(0.05)
    raise AssertionError("Timed out waiting for isolated gateway fixture")


def call(info, path, payload=None):
    req = urllib.request.Request(info["base_url"] + path, data=payload,
        headers={"Authorization": "Bearer " + info["client_token"], "Content-Type": "application/json"})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(req, timeout=8) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def terminate_owned_backend(pid, expected_image):
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.restype = ctypes.c_void_p
    kernel.QueryFullProcessImageNameW.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_wchar_p, ctypes.POINTER(ctypes.c_ulong)]
    kernel.TerminateProcess.argtypes = [ctypes.c_void_p, ctypes.c_uint]
    kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    kernel.CloseHandle.argtypes = [ctypes.c_void_p]
    handle = kernel.OpenProcess(0x1000 | 0x0001 | 0x100000, False, pid)
    if not handle:
        return
    try:
        buffer = ctypes.create_unicode_buffer(32768)
        length = ctypes.c_ulong(len(buffer))
        assert kernel.QueryFullProcessImageNameW(handle, 0, buffer, ctypes.byref(length))
        assert Path(buffer.value).resolve() == expected_image
        assert kernel.TerminateProcess(handle, 0)
        kernel.WaitForSingleObject(handle, 5000)
    finally:
        kernel.CloseHandle(handle)


def main():
    faulthandler.dump_traceback_later(30, repeat=True)
    executable, legacy = (Path(arg).resolve() for arg in sys.argv[1:3])
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    upstream = f"http://127.0.0.1:{server.server_port}/v1"
    children, backends = [], set()
    with tempfile.TemporaryDirectory(prefix="agent-switch-usage-smoke-") as root:
        env = dict(os.environ, LOCALAPPDATA=root, ZNNZ_API_KEY="key-a")
        state_dir = Path(root) / "Agent-Switch" / "gateway-v2"
        def launch(image, target):
            print("launch", target, image.name, flush=True)
            child = subprocess.Popen([str(image), "internal-gateway"], env=env,
                stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                creationflags=subprocess.CREATE_NO_WINDOW)
            children.append(child)
            child.stdin.write(json.dumps({"target":target,"gateway_url":upstream,"api_key":"key-a"}).encode())
            child.stdin.close()
            return wait_for(lambda: json.loads((state_dir / f"{target}.json").read_text()), lambda info: info["pid"] == child.pid)

        def upgrade(key):
            print("upgrade isolated legacy gateway", flush=True)
            # Detached Windows console children can retain captured pipe handles.
            # Wait on the updater process, not on EOF from all descendants.
            with tempfile.TemporaryFile() as log:
                result = subprocess.run([str(executable), "internal-gateway-update", "codex-desktop",
                    "--gateway-url", upstream], env=dict(env, ZNNZ_API_KEY=key),
                    stdout=log, stderr=log, timeout=20, creationflags=subprocess.CREATE_NO_WINDOW)
                log.seek(0)
                assert result.returncode == 0, log.read().decode(errors="replace")
            print("upgrade returned", flush=True)
            info = json.loads((state_dir / "codex-desktop.json").read_text())
            backends.add(info["statistics_backend"]["pid"])
            return info

        try:
            old = launch(legacy, "codex-desktop")
            assert old.get("usage_schema", 0) == 0
            # Fake attachment refers only to this owned test runner process.
            (state_dir / "codex-desktop.attached.json").write_text(json.dumps({
                "gateway_pid":old["pid"],"worker_pid":os.getpid(),"worker_image":sys.executable,
                "processes":[[os.getpid(),sys.executable]]
            }))
            upgraded = upgrade("key-a")
            print("legacy endpoint check", flush=True)
            assert upgraded["base_url"] == old["base_url"] and upgraded["pid"] == old["pid"]
            assert upgraded["statistics_backend"]["usage_schema"] == 1
            routes = [upgraded] + [launch(executable, target) for target in ("codex-cli", "claude-code", "claude-desktop")]
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                results = list(pool.map(lambda info: call(info, "/responses", b"{}"), routes))
            assert all(status == 200 for status, _ in results)
            assert call(upgraded, "/v1/models")[0] == 200
            assert call(upgraded, "/responses", b'{"fail":true}')[0] == 502
            switched = upgrade("key-b")
            assert switched["statistics_backend"]["pid"] == upgraded["statistics_backend"]["pid"]
            assert call(switched, "/messages", b"{}")[0] == 200

            database = Path(root) / "Agent-Switch" / "usage.sqlite3"
            def rows():
                with closing(sqlite3.connect(database)) as connection:
                    return connection.execute("SELECT COUNT(*),SUM(outcome='success'),SUM(outcome='failed'),SUM(input_tokens),SUM(output_tokens),SUM(cache_read),SUM(cache_write) FROM requests GROUP BY configuration_id ORDER BY COUNT(*) DESC").fetchall()
            result = wait_for(rows, lambda rows: len(rows) == 2 and sum(row[1] + row[2] for row in rows) == 6)
            assert result == [(5,4,1,4000,80,3200,None),(1,1,0,1000,20,800,100)], result
            for child in children:
                child.terminate()
                child.wait(timeout=5)
            for pid in backends:
                terminate_owned_backend(pid, executable)
            backends.clear()
            assert rows() == result, "Statistics did not persist after gateway exit"
            raw = database.read_bytes()
            assert b"key-a" not in raw and b"key-b" not in raw
            print(json.dumps({"four_clients":"passed","legacy_endpoint_preserved":True,
                "backend_reused":True,"requests":6,"success":5,"failed":1,
                "configuration_groups":2,"persistence":"passed","plaintext_keys":"absent"}))
        finally:
            for path in state_dir.glob("*.statistics.json"):
                backends.add(json.loads(path.read_text())["pid"])
            for pid in backends:
                terminate_owned_backend(pid, executable)
            for child in children:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=5)
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
