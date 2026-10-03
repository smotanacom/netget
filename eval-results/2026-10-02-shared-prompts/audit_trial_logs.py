#!/usr/bin/env python3
"""Count logged model requests and executed tools for the selected full trials."""
import collections
import json
import pathlib
import re

BASE = pathlib.Path(__file__).resolve().parent


def read(name):
    return json.loads((BASE / name).read_text())


def blocks(name):
    text = (BASE / name / 'eval.log').read_text(errors='replace')
    markers = list(re.finditer(r'^▶ ([^ ]+) run (\d+)/5$', text, re.M))
    result = {}
    for index, marker in enumerate(markers):
        key = (marker[1], int(marker[2]))
        assert key not in result, ('Duplicate run marker', name, key)
        result[key] = text[marker.end():markers[index + 1].start() if index + 1 < len(markers) else len(text)]
    return result


def audit(report, source_for_case, logs):
    rows = []
    total_tools = collections.Counter()
    for case in report['cases']:
        for run in case['runs']:
            source = source_for_case[case['id']]
            block = logs[source][case['id'], run['run']]
            assert 'LLM transport failure' not in block, (source, case['id'], run['run'])
            tools = collections.Counter(re.findall(r'→ Executing tool: ([^:\n]+)', block))
            total_tools.update(tools)
            rows.append({'case': case['id'], 'run': run['run'], 'log_source': source,
                         'request_attempts_logged': len(re.findall(r'LLM request \(attempt \d+/\d+\)', block)),
                         'completed_model_responses_logged': len(re.findall(r'LLM response received \(attempt \d+\)', block)),
                         'tool_executions_logged': dict(sorted(tools.items())),
                         'model_output_recorded': bool(run['model_output']),
                         'failure_mode': run['failure_mode'],
                         'verdict': run['verdict']})
    assert len(rows) == report['totals']['runs_total'] == 520
    return {'trials': len(rows), 'request_attempts_logged': sum(x['request_attempts_logged'] for x in rows),
            'completed_model_responses_logged': sum(x['completed_model_responses_logged'] for x in rows),
            'tool_executions_logged': dict(sorted(total_tools.items())),
            'trials_without_observed_model_request': [x for x in rows if not x['request_attempts_logged']],
            'trials_without_recorded_model_output': [x for x in rows if not x['model_output_recorded']],
            'trials_without_completed_model_response': [x for x in rows if not x['completed_model_responses_logged']],
            'runs': rows}


def main():
    before = read('before-complete.json')
    inputs = before['measurement_provenance']['inputs']
    selected = {}
    for kind, phase, key in [('original', 'before', 'retained_cases'),
                             ('continuation', 'before-continuation', 'included_cases'),
                             ('sleep_recovery', 'before-sleep-recovery', 'included_cases')]:
        for case in inputs[kind][key]:
            assert case not in selected, ('Duplicate selected case', case)
            selected[case] = phase
    assert len(selected) == 105
    logs = {name: blocks(name) for name in ['before', 'before-continuation', 'before-sleep-recovery', 'after-live']}
    after = read('after-live/latest.json')
    result = {'definition': 'Counts actual Executing tool log lines and LLM request attempt markers inside the selected case/run blocks. Model request markers show a request was attempted; these are not backend HTTP access-log counts.',
              'baseline': audit(before, selected, logs),
              'after': audit(after, {case['id']: 'after-live' for case in after['cases']}, logs)}
    (BASE / 'trial-log-audit.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({key: {k: v for k, v in value.items() if k != 'runs'} for key, value in result.items() if isinstance(value, dict)}, indent=2))


if __name__ == '__main__':
    main()
