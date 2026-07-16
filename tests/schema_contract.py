#!/usr/bin/env python3
import json
import pathlib

import jsonschema


schema_path = pathlib.Path(__file__).parents[1] / "references" / "cli.schema.json"
schema = json.loads(schema_path.read_text())

expected = {
    "terminal_id": "^term_[0-9A-HJKMNP-TV-Z]{26}$",
    "output_ref": "^out_[0-9A-HJKMNP-TV-Z]{26}$",
}
found = {key: 0 for key in expected}


def walk(value):
    if isinstance(value, dict):
        properties = value.get("properties", {})
        for key, pattern in expected.items():
            if key in properties and "pattern" in properties[key]:
                assert properties[key]["pattern"] == pattern, (key, properties[key])
                found[key] += 1
        for child in value.values():
            walk(child)
    elif isinstance(value, list):
        for child in value:
            walk(child)


walk(schema)
assert all(count > 1 for count in found.values()), found
assert "active_differs_from_local" in schema["$defs"]["ConfigValue"]["required"]
assert {"$ref": "#/$defs/InboxSummary"} in schema["oneOf"]
assert {"$ref": "#/$defs/LogsSummary"} in schema["oneOf"]
assert schema["$defs"]["DaemonRunning"]["properties"]["protocol_version"] == {
    "const": 4
}
codes = set(schema["$defs"]["Error"]["properties"]["code"]["enum"])
assert {"side_not_found", "protocol_mismatch"} <= codes

validator = jsonschema.Draft202012Validator(schema)


def assert_invalid(value):
    assert list(validator.iter_errors(value)), value


base_event = {
    "event_id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "ref": "e_1",
    "agent_id": "agt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "agent_ref": "a_1",
    "sequence": 1,
    "timestamp": "2026-07-16T00:00:00Z",
    "type": "user_message",
    "data": {"content": "question", "source": "create"},
}
assert_invalid({**base_event, "side_id": "side_01ARZ3NDEKTSV4RRFFQ69G5FAV"})
assert_invalid(
    {
        "type": "config_value",
        "key": "model",
        "default_value": 1,
        "persisted_value": 2,
        "local_effective_value": 3,
        "local_source": "anything",
        "active_value": 4,
        "active_source": "anything",
        "active_differs_from_local": True,
        "restart_required": False,
    }
)
assert_invalid(
    {
        "type": "list_summary",
        "resource": "messages",
        "count": 0,
        "next_cursor": "invented",
    }
)

tool_validator = validator.evolve(schema=schema["$defs"]["ToolResult"])


def assert_invalid_tool(value):
    assert list(tool_validator.iter_errors(value)), value


preview = {
    "content": "",
    "head_bytes": 0,
    "tail_bytes": 0,
    "total_bytes": 0,
    "truncated": False,
}
assert_invalid_tool(
    {
        "ok": False,
        "status": "completed",
        "exit_code": 0,
        "output": preview,
        "output_ref": "out_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "truncated": False,
    }
)
assert_invalid_tool(
    {
        "ok": False,
        "terminal_id": "term_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "status": "running",
        "exit_code": 99,
        "output": "",
        "output_ref": "out_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "next_offset": 0,
        "truncated": False,
    }
)
assert_invalid_tool(
    {
        "ok": True,
        "notification_id": "ntf_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "priority": 4,
        "event_type": "progress",
    }
)
side_stop = schema["$defs"]["Side"]["properties"]["stop_reason"]["enum"]
assert "invented" not in side_stop and "parent_deleted" in side_stop
print("schema contract checks passed")
