#!/usr/bin/env python3
import json
import pathlib
import sys
from datetime import datetime

import jsonschema


schema_path = pathlib.Path(__file__).parents[1] / "references" / "cli.schema.json"
schema = json.loads(schema_path.read_text())
jsonschema.Draft202012Validator.check_schema(schema)
format_checker = jsonschema.FormatChecker()


@format_checker.checks("date-time")
def is_rfc3339(value):
    if not isinstance(value, str):
        return True
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return "T" in value and parsed.tzinfo is not None
    except ValueError:
        return False


validator = jsonschema.Draft202012Validator(schema, format_checker=format_checker)

count = 0
for line_number, line in enumerate(sys.stdin, 1):
    if not line.strip():
        continue
    value = json.loads(line)
    errors = sorted(validator.iter_errors(value), key=lambda error: list(error.path))
    if errors:
        print(f"line {line_number} does not match cli.schema.json", file=sys.stderr)
        for error in errors:
            print(f"  {error.json_path}: {error.message}", file=sys.stderr)
        raise SystemExit(1)
    count += 1

if count == 0:
    raise SystemExit("expected at least one JSONL object")
