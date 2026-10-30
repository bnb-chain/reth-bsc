import json
import os
from pathlib import Path
import tempfile
import unittest
from resource_collector import Collector, JsonLog, stat, thread_delta, schedstat, vm_counters, selected_metric, role


def proc_stat(start=100, major=3, cpu=20):
    fields = ['0'] * 30
    fields[0] = 'R'
    for index, value in {7: 2, 9: major, 11: cpu, 12: 0, 19: start, 21: 10}.items():
        fields[index] = str(value)
    return '100 (worker (with spaces)) ' + ' '.join(fields)


class Tests(unittest.TestCase):
    def test_stat_and_tid_reuse_do_not_create_false_fault_deltas(self):
        self.assertEqual(stat(proc_stat())['major_faults'], 3)
        old = dict(stat(proc_stat()), role='miner_prewarm', io={'read_bytes': 100})
        new = dict(stat(proc_stat(major=8, cpu=25)), role='miner_prewarm', io={'read_bytes': 300})
        delta = thread_delta({'100': new}, {'100': old})['miner_prewarm']
        self.assertEqual(delta['delta']['major_faults'], 5)
        self.assertEqual(delta['delta']['read_bytes'], 200)
        new['start_ticks'] += 1
        reused = thread_delta({'100': new}, {'100': old})['miner_prewarm']
        self.assertEqual(reused['matched_threads'], 0)
        self.assertEqual(reused['delta'], {})

    def test_refault_and_scheduler_availability_and_restart(self):
        self.assertEqual(vm_counters({'workingset_refault_file': 12, 'workingset_activate': 4,
                                     'pgscan_kswapd': 1, 'unrelated': 5}),
                         {'workingset_refault_file': 12, 'workingset_activate': 4, 'pgscan_kswapd': 1})
        self.assertIsNone(schedstat(None))
        self.assertIsNone(schedstat('0 0'))
        self.assertEqual(role('miner-pipeline-'), 'miner_pipeline')
        old = dict(stat(proc_stat()), role='miner_prewarm', io={}, sched=schedstat('100 20 3'))
        new = dict(stat(proc_stat()), role='miner_prewarm', io={}, sched=schedstat('180 30 5'))
        group = thread_delta({'1': new}, {'1': old})['miner_prewarm']
        self.assertEqual(group['sched_matched_threads'], 1)
        self.assertEqual(group['delta']['runqueue_wait_ns'], 10)
        new['start_ticks'] += 1
        group = thread_delta({'1': new}, {'1': old})['miner_prewarm']
        self.assertEqual(group['sched_matched_threads'], 0)
        self.assertNotIn('runqueue_wait_ns', group['delta'])

    def test_metric_filter_keeps_cache_gauges_and_bounded_prewarm_series(self):
        self.assertTrue(selected_metric('reth_sync_caching_storage_cache_size'))
        self.assertTrue(selected_metric('reth_bsc_miner_prewarm_active_workers'))
        self.assertTrue(selected_metric('reth_bsc_miner_prewarm_handoff_max_age_seconds_sum'))
        self.assertFalse(selected_metric('reth_bsc_miner_prewarm_handoff_max_age_seconds_bucket'))
        self.assertFalse(selected_metric('unrelated_metric_total'))

    def test_log_rotation_stays_bounded_and_each_file_is_valid_jsonl(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = JsonLog(Path(tmp), 160, 2)
            for n in range(20):
                log.write({'number': n, 'value': 'x' * 40})
            files = list(Path(tmp).glob('samples.jsonl*'))
            self.assertEqual(len(files), 3)
            for path in files:
                self.assertLessEqual(path.stat().st_size, 160)
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                for line in path.read_text().splitlines():
                    self.assertIn('number', json.loads(line))

    def test_process_restarts_and_metadata_excludes_passwords(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            process = root / '100'
            task = process / 'task' / '101'
            task.mkdir(parents=True)
            (process / 'comm').write_text('reth-bsc\n')
            (process / 'stat').write_text(proc_stat())
            (process / 'cmdline').write_text('\0'.join([
                'reth-bsc', 'node', '--engine.cross-block-cache-size', '16384',
                '--mining.keystore-password', 'secret-password']))
            (process / 'environ').write_text('BSC_MINING_PIPELINE_DEPTH=2\0PASSWORD=other-secret\0')
            (process / 'status').write_text('Cpus_allowed_list:\t0-15\n')
            os.symlink('/bin/reth-bsc', process / 'exe')
            (task / 'comm').write_text('miner-prewarm-0\n')
            (task / 'stat').write_text(proc_stat())
            collector = Collector(root)
            first = collector.process()
            self.assertEqual(first['metadata']['flags']['--engine.cross-block-cache-size'], '16384')
            self.assertEqual(first['metadata']['env']['BSC_MINING_PIPELINE_DEPTH'], '2')
            self.assertNotIn('secret', json.dumps(first))
            second = collector.process()
            self.assertEqual(second['thread_groups']['miner_prewarm']['matched_threads'], 1)
            (process / 'stat').write_text(proc_stat(start=200))
            restarted = collector.process()
            self.assertIn('metadata', restarted)
            self.assertEqual(restarted['thread_groups']['miner_prewarm']['matched_threads'], 0)


if __name__ == '__main__':
    unittest.main()
