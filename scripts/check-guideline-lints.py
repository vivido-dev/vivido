#!/usr/bin/env python3
"""Reject growth in the reviewed Clippy debt; strict checks run separately in CI."""
import argparse
import collections
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
BASELINE = ROOT / 'scripts/guideline-lint-baseline.json'
RESTRICTIONS = '''as_pointer_underscore assertions_on_result_states clone_on_ref_ptr
    deref_by_slicing disallowed_script_idents empty_drop empty_enum_variants_with_brackets
    empty_structs_with_brackets fn_to_numeric_cast_any if_then_some_else_none map_err_ignore
    redundant_type_annotations renamed_function_params semicolon_outside_block
    unnecessary_safety_comment unnecessary_safety_doc unneeded_field_pattern unused_result_ok
    too_long_first_doc_paragraph'''.split()
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--write-baseline', action='store_true', help='Explicitly review the resulting diff')
args = parser.parse_args()
command = ['cargo', 'clippy', '--workspace', '--all-targets', '--all-features',
           '--locked', '--message-format=json', '--', '--cap-lints', 'warn',
           '-W', 'clippy::pedantic', '-W', 'clippy::cargo']
for lint in RESTRICTIONS:
    command.extend(['-W', 'clippy::' + lint])
result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE, text=True, check=False)
if result.returncode:
    print(result.stdout, file=sys.stderr)
    sys.exit(result.returncode)
seen = set()
counts = collections.Counter()
for line in result.stdout.splitlines():
    try:
        item = json.loads(line)
    except json.JSONDecodeError:
        continue
    if item.get('reason') != 'compiler-message':
        continue
    message = item['message']
    if message['level'] not in ('warning', 'error'):
        continue
    code = (message.get('code') or {}).get('code', 'unknown')
    spans = [span for span in message['spans'] if span['is_primary']]
    # Cargo metadata warnings have no spans. Deduplicate the lib and lib-test diagnostics.
    identity = (code, message['message'], tuple(
        (span['file_name'], span['line_start'], span['column_start']) for span in spans))
    if identity in seen:
        continue
    seen.add(identity)
    path = spans[0]['file_name'] if spans else '<dependencies>'
    if pathlib.Path(path).is_absolute():
        path = '<toolchain>'
    counts[path + '|' + code] += 1
if args.write_baseline:
    BASELINE.write_text(json.dumps(dict(sorted(counts.items())), indent=2) + '\n')
    print(f'Wrote {sum(counts.values())} diagnostics; review every increase before committing.')
    sys.exit(0)
baseline = json.loads(BASELINE.read_text())
failures = [(key, count, baseline.get(key, 0)) for key, count in counts.items()
            if count > baseline.get(key, 0)]
for key, count, previous in sorted(failures):
    print(f'{key}: {count} diagnostics (allowed {previous})', file=sys.stderr)
if failures:
    sys.exit('Guideline lint debt increased. Fix the issue or review an explicit baseline change.')
print(f'Guideline lint debt has not increased ({sum(counts.values())} diagnostics).')
