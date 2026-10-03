#!/usr/bin/env python3
"""Verify the archived report, selected raw trials, capture bytes and replay pairs."""
import argparse
import collections
import gzip
import hashlib
import importlib.util
import json
import math
import pathlib
import re
import sys

sys.dont_write_bytecode = True


def sha(data):
    return hashlib.sha256(data).hexdigest()


def module(path):
    spec = importlib.util.spec_from_file_location(path.stem, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def same_comparison(actual, expected, path=()):
    if isinstance(actual, dict):
        assert actual.keys() == expected.keys(), path
        for key in actual:
            same_comparison(actual[key], expected[key], path + (key,))
    elif isinstance(actual, list):
        assert len(actual) == len(expected), path
        for index, (left, right) in enumerate(zip(actual, expected)):
            same_comparison(left, right, path + (str(index),))
    elif path[-1] in {'mean_seconds', 'median_seconds', 'total_seconds'} and isinstance(actual, float):
        # Python 3.12+ uses a more accurate float sum than the pinned 3.10 runner.
        assert math.isclose(actual, expected, rel_tol=0, abs_tol=1e-9), (path, actual, expected)
    else:
        assert actual == expected, (path, actual, expected)


def verify(base):
    def read(name):
        return json.loads((base / name).read_text())

    hashes = read('SHA256SUMS.json')
    actual_files = {str(path.relative_to(base)) for path in base.rglob('*')
                    if path.is_file() and path.name != 'SHA256SUMS.json'}
    assert actual_files == hashes.keys(), 'Hash index must cover exactly the artifact files'
    for name, digest in hashes.items():
        assert sha((base / name).read_bytes()) == digest, name
    before, after = read('baseline.json'), read('after.json')
    before_build, after_build = read('provenance/baseline-build.json'), read('provenance/after-build.json')
    sources = ['d34d91c09ee1731ec13eb3977c17344aeda95497', '3e2d24bd935fe822296bad2962c82675dfcd820b']
    assert [before_build['source'], after_build['source']] == sources
    expected_digest = '46e0c10c039e019119339687c3c1757cc81b9da49709a3b3924863ba87ca666e'
    for build in (before_build, after_build):
        assert build['model']['digest'] == expected_digest and build['exit_code'] == 0
    after_live = read('provenance/after-live.json')
    assert after_live['source'] == sources[1] and after_live['exit_code'] == 0
    assert after_live['binary_sha256'] == after_build['binary_sha256']
    assert after_live['report_sha256'] == sha((base / 'after.json').read_bytes())
    for name in ['harness-equivalence.json', 'protocol-action-definition-equivalence.json']:
        proof = read(name)
        assert proof['sources'] == sources and all(row['byte_identical'] for row in proof['files'])
    compare = module(base / 'compare_evals.py')
    same_comparison(compare.compare(before, after), read('comparison.json'))
    for report in (before, after):
        assert len(report['cases']) == 105 and report['totals']['runs_total'] == 520
        assert sum(case['passes'] for case in report['cases']) == report['totals']['runs_passed']
        assert report['totals']['pass_rate'] == report['totals']['runs_passed'] / 520
        assert report['totals']['cases_attempted'] == 104
        assert len({case['protocol'] for case in report['cases']}) == 39
        assert [(case['id'], case['attempts']) for case in report['cases']
                if case['status'] != 'attempted'] == [('ntp/current-time', 0)]
        for case in report['cases']:
            assert case['attempts'] == (5 if case['status'] == 'attempted' else 0)
            assert [run['run'] for run in case['runs']] == list(range(1, case['attempts'] + 1))
    inputs = before['measurement_provenance']['inputs']
    mapping = [('original', 'before', 'baseline-interrupted.json', 'retained_cases', 20),
               ('continuation', 'before-continuation', 'baseline-continuation.json', 'included_cases', 79),
               ('sleep_recovery', 'before-sleep-recovery', 'baseline-sleep-recovery.json', 'included_cases', 6)]
    source_for_case, selected = {}, {}
    for key, phase, name, ids_key, count in mapping:
        assert inputs[key]['sha256'] == sha((base / name).read_bytes()), name
        source = {case['id']: case for case in read(name)['cases']}
        ids = inputs[key][ids_key]
        assert len(ids) == count
        for case_id in ids:
            assert case_id not in selected, ('Duplicate selected case', case_id)
            selected[case_id] = source[case_id]
            source_for_case[case_id] = phase
    assert {case['id']: case for case in before['cases']} == selected
    logs = {}
    for name, info in read('raw-logs/uncompressed-hashes.json').items():
        raw = gzip.decompress((base / name).read_bytes())
        assert len(raw) == info['uncompressed_bytes'] and sha(raw) == info['uncompressed_sha256'], name
        text = raw.decode(errors='replace')
        if not name.endswith('/before.log.gz'):
            assert 'test result: ok.' in text, name
        markers = list(re.finditer(r'^▶ ([^ ]+) run (\d+)/5$', text, re.M))
        blocks = {}
        for index, marker in enumerate(markers):
            key = (marker[1], int(marker[2]))
            assert key not in blocks, (name, key)
            blocks[key] = text[marker.end():markers[index + 1].start() if index + 1 < len(markers) else len(text)]
        logs[pathlib.Path(name).name.removesuffix('.log.gz')] = blocks
    audit = module(base / 'audit_trial_logs.py')
    archived_audit = read('trial-log-audit.json')
    assert audit.audit(before, source_for_case, logs) == archived_audit['baseline']
    assert audit.audit(after, {case['id']: 'after-live' for case in after['cases']}, logs) == archived_audit['after']
    replay_module = module(base / 'prompt_replay.py')
    requests = {}
    for label in ['baseline', 'baseline-repeat', 'patched', 'patched-repeat']:
        manifest = read('captures/' + label + '/manifest.json')
        assert len(manifest['requests']) == 6
        assert manifest['binary_sha256'] == (before_build if label.startswith('baseline') else after_build)['binary_sha256']
        for record in manifest['requests']:
            path = base / 'captures' / label / record['request_file']
            raw = path.read_bytes()
            assert sha(raw) == record['sha256'], str(path)
            payload = json.loads(raw)
            assert payload['model'] == 'llama3.1:8b' and payload['options']['seed'] == 42
            assert payload['stream'] is True
            assert replay_module.request_text(payload) == path.with_name(path.name.replace('.request.json', '.prompt.txt')).read_text()
            assert len(replay_module.request_text(payload)) == record['prompt_chars']
            requests[label, record['event']] = (record, payload, raw)
    events = [record['event'] for record in read('captures/baseline/manifest.json')['requests']]
    for event in events:
        for label in ['baseline', 'patched']:
            assert requests[label, event][2] == requests[label + '-repeat', event][2]
        assert requests['baseline', event][0]['prompt_chars'] - requests['patched', event][0]['prompt_chars'] == 6629
        assert requests['baseline', event][0]['endpoint'] == requests['patched', event][0]['endpoint']
        assert {key: value for key, value in requests['baseline', event][1].items() if key != 'prompt'} == {
            key: value for key, value in requests['patched', event][1].items() if key != 'prompt'}
    replay = read('replay-results.json')
    assert len(replay['results']) == 36 and replay['repeats'] == 3 and replay['seed'] == 42
    assert replay['model_record']['digest'] == '46e0c10c039e019119339687c3c1757cc81b9da49709a3b3924863ba87ca666e'
    pairs = collections.defaultdict(list)
    for sequence, row in enumerate(replay['results'], 1):
        assert row['sequence'] == sequence
        record, payload, raw = requests[row['label'], row['event']]
        assert row['request'] == record['request_file'] and row['original_request_sha256'] == sha(raw)
        payload = dict(payload)
        payload['stream'] = False
        assert row['replayed_payload_sha256'] == sha(json.dumps(payload).encode())
        assert row['response']['done'] is True
        content = row['response'].get('response', row['response'].get('message', {}).get('content', ''))
        parsed = replay_module.answer_object(content)
        assert row['parsed'] == parsed
        assert row['tools'] == (None if parsed is None else len(parsed['tools']))
        assert row['actions'] == (None if parsed is None else len(parsed['actions']))
        pairs[row['event'], row['repeat']].append(row['label'])
    assert len(pairs) == 18 and all(set(labels) == {'baseline', 'patched'} for labels in pairs.values())
    assert collections.Counter(labels[0] for labels in pairs.values()) == {'baseline': 9, 'patched': 9}
    for request_index, event in enumerate(events):
        for repeat in range(1, 4):
            expected = ['baseline', 'patched'] if (request_index + repeat - 1) % 2 == 0 else ['patched', 'baseline']
            assert pairs[event, repeat] == expected
    print(json.dumps({'verified_files': len(hashes), 'before_passed': before['totals']['runs_passed'],
                      'after_passed': after['totals']['runs_passed'], 'trials_per_phase': 520,
                      'verified_raw_trial_blocks': 1040, 'captures': len(requests), 'replays': 36}, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('artifact_dir', type=pathlib.Path, nargs='?', default=pathlib.Path(__file__).resolve().parent)
    verify(parser.parse_args().artifact_dir.resolve())
