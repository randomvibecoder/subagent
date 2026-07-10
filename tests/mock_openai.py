#!/usr/bin/env python3
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def function_call(index, name, args):
    return {
        "index": index,
        "id": f"call_{index}",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(args)},
    }


def fragment_calls(calls):
    first = []
    second = []
    for call in calls:
        arguments = call["function"]["arguments"]
        midpoint = len(arguments) // 2
        first.append(
            {
                **call,
                "function": {
                    "name": call["function"]["name"],
                    "arguments": arguments[:midpoint],
                },
            }
        )
        second.append(
            {
                "index": call["index"],
                "function": {"arguments": arguments[midpoint:]},
            }
        )
    return [{"tool_calls": first}, {"tool_calls": second}]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length))
        messages = request.get("messages", [])
        user_text = "\n".join(
            message.get("content", "")
            for message in messages
            if message.get("role") == "user" and isinstance(message.get("content"), str)
        )
        latest_user = next(
            (
                message.get("content", "")
                for message in reversed(messages)
                if message.get("role") == "user"
                and isinstance(message.get("content"), str)
            ),
            "",
        )
        tool_names = {
            tool.get("function", {}).get("name") for tool in request.get("tools", [])
        }
        has_tool_result = any(message.get("role") == "tool" for message in messages)
        tool_results = [message for message in messages if message.get("role") == "tool"]
        system_text = "\n".join(
            message.get("content", "")
            for message in messages
            if message.get("role") == "system"
            and isinstance(message.get("content"), str)
        )

        if latest_user == "DELAY" and not has_tool_result:
            time.sleep(10)

        if "READONLY_PROMPT" in latest_user:
            correct = (
                "must not modify files or system state" in system_text
                and "sed without -i" in system_text
                and not {"write", "edit", "apply_patch"}.intersection(tool_names)
            )
            deltas = [{"content": "readonly prompt correct" if correct else "readonly prompt incorrect"}]
        elif "SIDE_TOOL_QUESTION" in latest_user and not {
            "read",
            "glob",
            "grep",
            "exec_command",
            "view_image",
        }.issubset(tool_names):
            deltas = [{"content": "missing inherited tools"}]
        elif "SIDE_TOOL_QUESTION" in latest_user and {
            "write",
            "edit",
            "apply_patch",
        }.intersection(tool_names):
            deltas = [{"content": "unsafe mutation tools exposed"}]
        elif "SIDE_TOOL_QUESTION" in latest_user and not tool_results:
            deltas = fragment_calls(
                [
                    function_call(0, "read", {"path": "side.txt"}),
                    function_call(1, "glob", {"pattern": "*.txt"}),
                    function_call(
                        2, "grep", {"pattern": "side-file-content", "path": "."}
                    ),
                    function_call(
                        3,
                        "exec_command",
                        {
                            "command": "grep -n side-file-content side.txt",
                            "yield_time_ms": 250,
                        },
                    ),
                    function_call(4, "view_image", {"path": "pixel.png"}),
                ]
            )
        elif "SIDE_TOOL_QUESTION" in latest_user:
            deltas = [{"content": "side inherited context and tools"}]
        elif "SIDE_CONTEXT_ONLY" in latest_user:
            deltas = [
                {
                    "content": (
                        "context inherited"
                        if "SIDE_PARENT_MARKER" in user_text
                        else "context missing"
                    )
                }
            ]
        elif "SIDE_WHILE_WORKING" in latest_user:
            deltas = [{"content": "parent still running"}]
        elif "SECRET_ENV" in user_text and not tool_results:
            deltas = fragment_calls(
                [
                    function_call(
                        0,
                        "exec_command",
                        {
                            "command": "if env | grep '^OPENAI_API_KEY='; then exit 9; else printf hidden; fi",
                            "yield_time_ms": 250,
                        },
                    )
                ]
            )
        elif "SECRET_ENV" in user_text:
            deltas = [{"content": "secret check completed"}]
        elif "TERMINAL_POLL" in user_text and not tool_results:
            deltas = fragment_calls(
                [
                    function_call(
                        0,
                        "exec_command",
                        {
                            "command": "printf start; sleep 1; printf end",
                            "yield_time_ms": 250,
                        },
                    )
                ]
            )
        elif "TERMINAL_POLL" in user_text and len(tool_results) == 1:
            result = json.loads(tool_results[-1]["content"])
            deltas = fragment_calls(
                [
                    function_call(
                        0,
                        "write_stdin",
                        {
                            "terminal_id": result["terminal_id"],
                            "yield_time_ms": 1500,
                        },
                    )
                ]
            )
        elif "TERMINAL_POLL" in user_text:
            deltas = [{"content": "terminal completed"}]
        elif "WRITE_EDIT_PATCH" in user_text and not has_tool_result:
            calls = [
                function_call(0, "write", {"path": "generated.txt", "content": "alpha\n"}),
                function_call(
                    1,
                    "edit",
                    {
                        "path": "generated.txt",
                        "old_text": "alpha",
                        "new_text": "beta",
                        "expected_replacements": 1,
                    },
                ),
                function_call(
                    2,
                    "apply_patch",
                    {
                        "patch": "*** Begin Patch\n*** Update File: generated.txt\n@@\n-beta\n+gamma\n*** End Patch"
                    },
                ),
                function_call(3, "read", {"path": "generated.txt"}),
                function_call(4, "glob", {"pattern": "*.txt"}),
                function_call(5, "grep", {"pattern": "gamma", "include": "*.txt"}),
            ]
            deltas = fragment_calls(calls)
        elif "BACKGROUND_LIMIT" in user_text and not has_tool_result:
            deltas = fragment_calls(
                [
                    function_call(
                        i,
                        "exec_command",
                        {"command": "sleep 30", "yield_time_ms": 250},
                    )
                    for i in range(9)
                ]
            )
        else:
            deltas = [
                {"reasoning": "mock reasoning", "content": "comp"},
                {"content": "leted"},
            ]

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        try:
            for delta in deltas:
                event = {"choices": [{"index": 0, "delta": delta}]}
                self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except BrokenPipeError:
            pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 18080), Handler).serve_forever()
