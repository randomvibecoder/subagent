#!/usr/bin/env python3
import json
import pathlib
import sys

import jsonschema


schema_path = pathlib.Path(__file__).parents[1] / "references" / "cli.schema.json"
schema = json.loads(schema_path.read_text())
jsonschema.Draft202012Validator.check_schema(schema)
validator = jsonschema.Draft202012Validator(schema)

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
