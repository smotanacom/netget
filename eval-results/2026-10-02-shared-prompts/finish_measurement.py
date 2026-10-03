#!/usr/bin/env python3
"""Resume preserved trials, run the complete after phase, then replay captured prompts.

This runner never builds Rust or edits measurement source. Per-case trials are
retained in their original reports; the resumed roll-up names every source.
"""
import collections
import datetime
import hashlib
import json
import os
import pathlib
import re
import subprocess
import time
import urllib.request

BASE = pathlib.Path(__file__).resolve().parent
EXPECTED_MODEL = '46e0c10c039e019119339687c3c1757cc81b9da49709a3b3924863ba87ca666e'
REPLACE_PROTOCOLS = {'gemini', 'beanstalkd', 'whois', 'gopher'}


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def read(path):
    return json.loads(path.read_text())


def save(path, value):
    tmp = path.with_suffix(path.suffix + '.tmp')
    tmp.write_text(json.dumps(value, indent=2) + '\n')
    tmp.replace(path)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def status(phase, **fields):
    record = {'phase': phase, 'updated_at': now(), 'controller_pid': os.getpid(), **fields}
    save(BASE / 'finish-status.json', record)
    print(json.dumps(record), flush=True)


def preflight():
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open('http://127.0.0.1:11434/api/tags', timeout=20) as r:
        tags = json.load(r)
    model = next(m for m in tags['models'] if m['name'] == 'llama3.1:8b')
    assert model['digest'] == EXPECTED_MODEL, model
    with opener.open('http://127.0.0.1:11434/api/version', timeout=20) as r:
        version = json.load(r)
    return model, version


def direct_run(phase, output_name, protocols, only_live=False):
    output = BASE / output_name
    output.mkdir(exist_ok=False)
    source = BASE / ('before-source' if phase == 'before' else 'after-measurement')
    build = read(BASE / phase / 'build-provenance.json')
    executable = BASE / (phase + '-target/debug/deps/eval-02a2a94d62b715a2')
    binary = BASE / phase / 'netget'
    assert digest(binary) == build['binary_sha256']
    source_sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=source, text=True).strip()
    assert source_sha == build['source']
    assert not subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=source, text=True).strip()
    model, version = preflight()
    env = os.environ.copy()
    env.update(PATH=str(BASE / 'tools') + os.pathsep + env['PATH'],
               CARGO_BIN_EXE_netget=str(binary), NETGET_USE_OLLAMA='1',
               NETGET_LLM_TEST_MODEL='llama3.1:8b', NETGET_EVAL_RUNS='5',
               NETGET_EVAL_SEED='42', NETGET_EVAL_TEMPERATURE='',
               NETGET_EVAL_PROTOCOLS=','.join(protocols), NETGET_EVAL_OUT_DIR=str(output),
               OLLAMA_BASE_URL='http://127.0.0.1:11434')
    env.pop('NETGET_EVAL_MIN_RATE', None)
    command = [str(executable)]
    if only_live:
        command += ['real_model_eval', '--exact']
    command += ['--nocapture', '--test-threads=1']
    provenance = {'source': source_sha, 'phase': phase, 'started_at': now(),
                  'command': command, 'command_equivalence': 'Prebuilt executable and environment from run-eval.sh; cargo compilation and preflight printing omitted.',
                  'protocols': protocols, 'environment': {k: env[k] for k in env if k.startswith('NETGET_') or k in ['OLLAMA_BASE_URL', 'CARGO_BIN_EXE_netget', 'PATH']},
                  'binary_sha256': digest(binary), 'harness_sha256': digest(executable),
                  'build_provenance': build, 'model': model, 'ollama_version': version}
    save(output / 'provenance.json', provenance)
    start = time.monotonic()
    with (output / 'eval.log').open('wb') as log:
        child = subprocess.Popen(command, cwd=source, env=env, stdout=log, stderr=subprocess.STDOUT)
        status(output_name, child_pid=child.pid, selected_protocols=protocols)
        code = child.wait()
    provenance.update(finished_at=now(), elapsed_seconds=time.monotonic() - start, exit_code=code)
    if (output / 'latest.json').exists():
        provenance['report_sha256'] = digest(output / 'latest.json')
    save(output / 'provenance.json', provenance)
    assert code == 0, (output_name, code)
    log_text = (output / 'eval.log').read_text(errors='replace')
    assert 'test result: ok.' in log_text, output_name
    assert 'LLM transport failure' not in log_text, ('Backend failure in phase', output_name)
    return read(output / 'latest.json')


def rollup(template, cases, inputs):
    """Rebuild report summary fields using tests/eval/report.rs's definitions."""
    result = {k: template[k] for k in ['schema', 'model', 'runs_per_case', 'seed', 'temperature', 'determinism']}
    result['generated_at'] = str(int(time.time()))
    result['measurement_provenance'] = {'resumed': True, 'inputs': inputs,
        'note': 'Each included case has five fresh trials from one source revision. Original incomplete trials and backend-failure cases remain available separately. This is a resumed measurement, not one uninterrupted invocation.'}
    repeated = [c for c in cases if c['verdicts_agree'] is not None]
    runs = sum(c['attempts'] for c in cases)
    passed = sum(c['passes'] for c in cases)
    recovered = sum(c['recoverable_runs'] for c in cases)
    attempted = sum(c['status'] == 'attempted' for c in cases)
    result['totals'] = {'cases_total': len(cases), 'cases_attempted': attempted, 'cases_skipped': len(cases) - attempted,
        'runs_total': runs, 'runs_passed': passed, 'pass_rate': passed / runs if runs else 0.,
        'runs_recoverable': recovered, 'pass_rate_with_lenient_parse': (passed + recovered) / runs if runs else 0.,
        'cases_with_repeats': len(repeated), 'cases_verdicts_agree': sum(c['verdicts_agree'] is True for c in repeated),
        'cases_actions_agree': sum(c['actions_agree'] is True for c in repeated)}
    groups = collections.defaultdict(list)
    for c in cases:
        groups[c['protocol']].append(c)
    summaries = []
    for protocol, group in sorted(groups.items()):
        n = sum(c['attempts'] for c in group)
        p = sum(c['passes'] for c in group)
        here = [c for c in group if c['status'] == 'attempted']
        summaries.append({'protocol': protocol, 'client': here[0]['client'].split()[0] if here else '-',
            'independence': here[0]['independence'] if here else 'none', 'instructions': len(group),
            'instructions_fully_passed': sum(c['attempts'] > 0 and c['passes'] == c['attempts'] for c in group),
            'runs': n, 'runs_passed': p, 'pass_rate': p / n if n else None,
            'dominant_failures': sorted({c['dominant_failure'] for c in group if c['dominant_failure']}),
            'skipped_reason': None if here else next((c['status_reason'] for c in group if c['status_reason']), None),
            'runs_recoverable': sum(c['recoverable_runs'] for c in group)})
    result['protocols'] = summaries
    modes = collections.defaultdict(lambda: {'runs': 0, 'cases': []})
    explanations = {m['mode']: m['explanation'] for r in inputs.values() if isinstance(r, dict) and 'failure_modes' in r for m in r['failure_modes']}
    for c in cases:
        for r in c['runs']:
            if r['failure_mode']:
                mode = modes[r['failure_mode']]
                mode['runs'] += 1
                if c['id'] not in mode['cases']:
                    mode['cases'].append(c['id'])
    result['failure_modes'] = [{'mode': name, **mode, 'explanation': explanations.get(name, 'Harness-level, not a model or description defect.')}
                              for name, mode in sorted(modes.items(), key=lambda item: (-item[1]['runs'], item[0]))]
    result['cases'] = cases
    return result


def main():
    status('starting')
    original = read(BASE / 'before/latest.json')
    assert len(original['cases']) == 32 and original['totals']['runs_total'] == 160
    suite = (BASE / 'before-source/tests/eval/suites.rs').read_text()
    expected_ids = re.findall(r'EvalCase::(?:new|unavailable)\(\s*"([^"\s]+)"', suite)
    assert len(expected_ids) == len(set(expected_ids)) == 105
    retained = {c['id']: c for c in original['cases'] if c['protocol'] not in REPLACE_PROTOCOLS}
    protocols = list(dict.fromkeys(x.split('/')[0] for x in expected_ids if x not in retained))
    continuation = direct_run('before', 'before-continuation', protocols)
    measured = dict(retained)
    measured.update({c['id']: c for c in continuation['cases'] if c['id'] not in retained})
    assert set(measured) == set(expected_ids), (set(expected_ids) - set(measured))
    inputs = {'original': {'path': 'before/latest.json', 'sha256': digest(BASE / 'before/latest.json'), 'retained_cases': list(retained)},
              'continuation': {'path': 'before-continuation/latest.json', 'sha256': digest(BASE / 'before-continuation/latest.json'),
                               'included_cases': [x for x in expected_ids if x not in retained]},
              'replaced_protocols': sorted(REPLACE_PROTOCOLS)}
    before = rollup(original, [measured[x] for x in expected_ids], {'original': original, 'continuation': continuation})
    before['measurement_provenance']['inputs'] = inputs
    save(BASE / 'before-complete.json', before)
    status('before-complete', totals=before['totals'])
    after = direct_run('after', 'after-live', list(dict.fromkeys(x.split('/')[0] for x in expected_ids)))
    assert set(c['id'] for c in after['cases']) == set(expected_ids)
    status('after-complete', totals=after['totals'])
    subprocess.run([str(BASE / 'tools/python3'), str(BASE / 'compare_evals.py'), str(BASE / 'before-complete.json'),
                    str(BASE / 'after-live/latest.json'), str(BASE / 'comparison.json')], check=True)
    status('replay')
    with (BASE / 'replay.log').open('wb') as log:
        subprocess.run([str(BASE / 'tools/python3'), str(BASE / 'prompt_replay.py'), 'replay',
                        str(BASE / 'captures/baseline'), str(BASE / 'captures/patched'), '--ollama-url', 'http://127.0.0.1:11434',
                        '--model', 'llama3.1:8b', '--seed', '42', '--repeats', '3', '--output', str(BASE / 'replay-results.json')],
                       stdout=log, stderr=subprocess.STDOUT, check=True)
    status('complete', before=before['totals'], after=after['totals'])


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        status('failed', error=repr(error))
        raise
