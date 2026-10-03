#!/usr/bin/env python3
"""Keep the ongoing baseline child, supplement sleep-affected cases, then finish."""
import fcntl
import datetime
import json
import os
import pathlib
import re
import signal
import subprocess
import time

from finish_measurement import BASE, REPLACE_PROTOCOLS, direct_run, read, rollup, save, status, digest

OLD_CONTROLLER = 15255


def main():
    status('before-continuation', preserved_child_pid=15262, coordinator_replaced=OLD_CONTROLLER,
           reason='Add clean complete-protocol repeats after confirmed host sleep; existing live trials keep running.')
    while True:
        log = (BASE / 'before-continuation/eval.log').read_text(errors='replace')
        if 'test result:' in log:
            assert 'test result: ok.' in log
            break
        time.sleep(5)
    # Only the paused coordinator is replaced; its measurement child finished.
    os.kill(OLD_CONTROLLER, signal.SIGTERM)
    os.kill(OLD_CONTROLLER, signal.SIGCONT)
    continuation = read(BASE / 'before-continuation/latest.json')
    assert len(continuation['cases']) == 81
    assert 'LLM transport failure' not in log
    provenance_path = BASE / 'before-continuation/provenance.json'
    provenance = read(provenance_path)
    finished = datetime.datetime.now(datetime.timezone.utc)
    provenance.update(finished_at=finished.isoformat(), report_sha256=digest(BASE / 'before-continuation/latest.json'),
                      completion_observed_via='Final test result: ok. in harness log; original coordinator was paused to insert sleep recovery.',
                      exit_code=None,
                      elapsed_wall_seconds=(finished - datetime.datetime.fromisoformat(provenance['started_at'])).total_seconds())
    save(provenance_path, provenance)
    original = read(BASE / 'before/latest.json')
    assert original['totals']['runs_total'] == 160
    supplemental = direct_run('before', 'before-sleep-recovery', ['whois', 'gopher'], only_live=True)
    retained = {c['id']: c for c in original['cases'] if c['protocol'] not in REPLACE_PROTOCOLS}
    suite = (BASE / 'before-source/tests/eval/suites.rs').read_text()
    expected_ids = re.findall(r'EvalCase::(?:new|unavailable)\(\s*"([^"\s]+)"', suite)
    assert len(expected_ids) == len(set(expected_ids)) == 105
    measured = dict(retained)
    measured.update({c['id']: c for c in continuation['cases'] if c['id'] not in retained})
    measured.update({c['id']: c for c in supplemental['cases']})
    assert set(measured) == set(expected_ids)
    inputs = {'original': {'path': 'before/latest.json', 'sha256': digest(BASE / 'before/latest.json'), 'retained_cases': list(retained)},
              'continuation': {'path': 'before-continuation/latest.json', 'sha256': digest(BASE / 'before-continuation/latest.json'),
                               'included_cases': [c['id'] for c in continuation['cases'] if c['id'] not in retained]},
              'sleep_recovery': {'path': 'before-sleep-recovery/latest.json', 'sha256': digest(BASE / 'before-sleep-recovery/latest.json'),
                                'included_cases': [c['id'] for c in supplemental['cases']]},
              'replaced_protocols': sorted(REPLACE_PROTOCOLS),
              'replacement_reason': {'gemini': 'backend transport failure', 'beanstalkd': 'backend transport failure',
                                     'whois': 'host sleep', 'gopher': 'host sleep'}}
    before = rollup(original, [measured[x] for x in expected_ids], {'original': original, 'continuation': continuation, 'sleep_recovery': supplemental})
    before['measurement_provenance']['inputs'] = inputs
    assert before['totals']['runs_total'] == 520
    save(BASE / 'before-complete.json', before)
    status('before-complete', totals=before['totals'])
    after = direct_run('after', 'after-live', list(dict.fromkeys(x.split('/')[0] for x in expected_ids)))
    assert set(c['id'] for c in after['cases']) == set(expected_ids)
    status('after-complete', totals=after['totals'])
    subprocess.run([str(BASE / 'tools/python3'), str(BASE / 'compare_evals.py'), str(BASE / 'before-complete.json'),
                    str(BASE / 'after-live/latest.json'), str(BASE / 'comparison.json')], check=True)
    status('waiting-for-replay-build-slot')
    # The root and other builders share this existing advisory build lock.
    with pathlib.Path('/Users/matus/dev/netget/.protocol-expansion-20261001/build.lock').open('r') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        status('replay', rust_build_slot_held=True)
        with (BASE / 'replay.log').open('wb') as replay_log:
            subprocess.run([str(BASE / 'tools/python3'), str(BASE / 'prompt_replay.py'), 'replay',
                            str(BASE / 'captures/baseline'), str(BASE / 'captures/patched'), '--ollama-url', 'http://127.0.0.1:11434',
                            '--model', 'llama3.1:8b', '--seed', '42', '--repeats', '3', '--output', str(BASE / 'replay-results.json')],
                           stdout=replay_log, stderr=subprocess.STDOUT, check=True)
        fcntl.flock(lock, fcntl.LOCK_UN)
    status('complete', before=before['totals'], after=after['totals'])


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        status('failed', error=repr(error))
        raise
