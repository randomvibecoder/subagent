#!/usr/bin/env python3
import json
import pathlib


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
assert schema["$defs"]["DaemonRunning"]["properties"]["protocol_version"] == {
    "const": 3
}
codes = set(schema["$defs"]["Error"]["properties"]["code"]["enum"])
assert {"side_not_found", "protocol_mismatch"} <= codes
print("schema contract checks passed")
