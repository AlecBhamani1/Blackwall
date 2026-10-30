# A minimal MCP server over standard input and output, used by the core tests. It answers
# `initialize` and `tools/list` with one tool, `echo`, and ignores everything else.
import json
import sys

for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    identifier = message.get("id")
    if identifier is None:
        continue
    method = message.get("method")
    if method == "initialize":
        result = {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "echo", "version": "0"},
        }
    elif method == "tools/list":
        result = {
            "tools": [
                {
                    "name": "echo",
                    "description": "Echo",
                    "inputSchema": {"type": "object", "properties": {}},
                }
            ]
        }
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": identifier, "result": result}) + "\n")
    sys.stdout.flush()
