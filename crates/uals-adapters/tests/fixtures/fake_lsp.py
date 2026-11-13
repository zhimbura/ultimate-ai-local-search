"""A language server for tests: speaks LSP over stdio and logs every message it gets.

Environment:
  FAKE_LOG       file that gets one JSON line per message received (and the replies to its requests)
  FAKE_REF       path, relative to the root folder, of the one reference it returns (line 3); empty: none
  FAKE_PROGRESS  "full": a begin and an end of progress after `initialized`; "begin": a begin only
  FAKE_FAIL      a log message to send after `initialized`
  FAKE_DELAY     seconds to wait before answering the first `textDocument/references`
"""

import json
import os
import sys
import time


def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode().strip()
        if not line:
            break
        key, value = line.split(":", 1)
        headers[key.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers["content-length"])))


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()


log = open(os.environ["FAKE_LOG"], "a")
root = None
delay = float(os.environ.get("FAKE_DELAY", "0"))
while True:
    message = read()
    if message is None:
        break
    method = message.get("method")
    log.write(json.dumps({"method": method, "params": message.get("params"), "result": message.get("result"),
                          "cwd": os.getcwd(), "args": sys.argv[1:],
                          "java_options": os.environ.get("IJ_JAVA_OPTIONS")}) + "\n")
    log.flush()
    if method == "initialize":
        root = message["params"]["rootUri"]
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {"referencesProvider": True}}})
    elif method == "initialized":
        send({"jsonrpc": "2.0", "id": "config-1", "method": "workspace/configuration",
              "params": {"items": [{"section": "a"}, {"section": "b"}]}})
        progress = os.environ.get("FAKE_PROGRESS")
        if progress:
            send({"jsonrpc": "2.0", "id": "progress-1", "method": "window/workDoneProgress/create",
                  "params": {"token": "load"}})
            send({"jsonrpc": "2.0", "method": "$/progress",
                  "params": {"token": "load", "value": {"kind": "begin", "title": "Importing"}}})
            if progress == "full":
                send({"jsonrpc": "2.0", "method": "$/progress", "params": {"token": "load", "value": {"kind": "end"}}})
        if os.environ.get("FAKE_FAIL"):
            send({"jsonrpc": "2.0", "method": "window/logMessage",
                  "params": {"type": 1, "message": os.environ["FAKE_FAIL"]}})
    elif method == "textDocument/references":
        time.sleep(delay)
        delay = 0
        locations = []
        if os.environ["FAKE_REF"]:
            locations.append({"uri": root + "/" + os.environ["FAKE_REF"],
                              "range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 8}}})
        send({"jsonrpc": "2.0", "id": message["id"], "result": locations})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "exit":
        break
