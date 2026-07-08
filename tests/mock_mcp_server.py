import sys
import json
import logging

logging.basicConfig(level=logging.INFO, filename="mock_mcp.log")

def main():
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        try:
            req = json.loads(line)
        except:
            continue
            
        logging.info(f"Received: {req}")

        if "id" not in req:
            # Notification
            continue

        req_id = req["id"]
        method = req.get("method")

        if method == "initialize":
            res = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "mock-mcp", "version": "1.0.0"}
                }
            }
        elif method == "tools/list":
            res = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": [
                        {
                            "name": "say_hello",
                            "description": "Say hello to someone",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"}
                                },
                                "required": ["name"]
                            }
                        }
                    ]
                }
            }
        elif method == "tools/call":
            args = req.get("params", {}).get("arguments", {})
            name = args.get("name", "world")
            res = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [
                        {"type": "text", "text": f"Hello, {name}!"}
                    ]
                }
            }
        else:
            res = {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32601, "message": "Method not found"}
            }

        out = json.dumps(res) + "\n"
        sys.stdout.write(out)
        sys.stdout.flush()

if __name__ == "__main__":
    main()
