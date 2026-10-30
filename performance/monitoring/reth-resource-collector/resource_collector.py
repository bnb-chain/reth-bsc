#!/usr/bin/env python3
"""Bounded read-only Linux resource collector; no database reads or stack sampling."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import resource
import signal
import time
import urllib.request

MEM_KEYS = {'MemTotal', 'MemFree', 'MemAvailable', 'Cached', 'Buffers', 'SReclaimable',
            'Active(file)', 'Inactive(file)', 'AnonPages', 'Dirty', 'Writeback',
            'SwapTotal', 'SwapFree', 'Unevictable'}
FLAGS = {'--chain', '--datadir', '--engine.cross-block-cache-size',
         '--engine.persistence-threshold', '--engine.memory-block-buffer-target',
         '--engine.persistence-backpressure-threshold', '--engine.state-root-task-timeout',
         '--engine.reserved-cpu-cores', '--db.sync-mode', '--db.read-transaction-timeout',
         '--max-pending-imports', '--builder.gaslimit', '--mining.gas-limit'}
SWITCHES = {'--engine.txpool-prewarming', '--mining.use-sparse-trie-state-root', '--mining.enabled'}
ENV_KEYS = {'BSC_MINING_PIPELINED_BUILD', 'BSC_MINING_PIPELINE_DEPTH',
            'BSC_MINING_ALLOW_LATE_FINISH', 'BSC_MINER_SPARSE_REUSE_CANONICAL',
            'BSC_MINING_ADAPTIVE_RESERVE', 'BSC_MINING_ROOT_RESERVE_MIN_MS',
            'BSC_MINING_ROOT_RESERVE_MAX_MS', 'BSC_MINING_LAST_BLOCK_PERIOD_DIVISOR'}


def read(path):
    try:
        return Path(path).read_text()
    except (OSError, UnicodeError):
        return None


def keyed(text, wanted=None):
    out = {}
    for line in (text or '').splitlines():
        parts = line.replace(':', '', 1).split()
        if len(parts) > 1 and (wanted is None or parts[0] in wanted):
            try:
                out[parts[0]] = int(parts[1])
            except ValueError:
                pass
    return out


def stat(text):
    if not text:
        return None
    # comm can contain whitespace and parentheses; fields after its final ')' are stable.
    fields = text.rsplit(')', 1)[1].split()
    return {'start_ticks': int(fields[19]), 'minor_faults': int(fields[7]),
            'major_faults': int(fields[9]), 'cpu_ticks': int(fields[11]) + int(fields[12]),
            'rss_pages': int(fields[21])}


def role(name):
    for prefix, group in [('miner-prewarm', 'miner_prewarm'),
                          ('miner-pipeline', 'miner_pipeline'),
                          ('miner-proof-st', 'miner_storage_proof'),
                          ('miner-proof-ac', 'miner_account_proof'),
                          ('miner-state-ov', 'miner_overlay'),
                          ('proof-strg', 'import_storage_proof'),
                          ('proof-acct', 'import_account_proof'),
                          ('miner-sparse', 'miner_root'), ('sparse-trie', 'engine_root'),
                          ('miner-trie', 'miner_hashing'), ('trie-hashing', 'engine_hashing'),
                          ('prewarm', 'engine_prewarm'), ('mdbx', 'mdbx')]:
        if name.startswith(prefix):
            return group
    return None


def schedstat(text):
    try:
        runtime, wait, slices = map(int, (text or '').split()[:3])
        return {'runtime_ns': runtime, 'runqueue_wait_ns': wait, 'timeslices': slices}
    except (ValueError, TypeError):
        return None


def vm_counters(vm):
    return {k: v for k, v in vm.items()
            if k in {'pgfault', 'pgmajfault', 'pswpin', 'pswpout', 'allocstall'}
            or k.startswith(('pgscan_', 'pgsteal_', 'allocstall_', 'workingset_'))}


def selected_metric(name):
    # Histograms: sums/counts support averages; quantiles remain in Prometheus/Grafana.
    if name.startswith(('reth_bsc_miner_prewarm_', 'reth_sync_caching_',
                        'reth_bsc_build_diagnostics_state_read',
                        'reth_bsc_build_diagnostics_prewarm_cache_',
                        'reth_bsc_build_diagnostics_transaction_stage_')):
        return not name.endswith('_bucket')
    if not name.endswith(('_sum', '_count', '_total')):
        return False
    return ('persistence' in name or 'finish_drain' in name or
            'proofs_pending_at_finish' in name or
            name.startswith(('reth_bsc_build_diagnostics_finish_stage_seconds_',
                             'reth_bsc_miner_block_finalize_duration_',
                             'reth_bsc_miner_block_trie_root_duration_')))


def thread_delta(current, previous):
    groups = {}
    for tid, row in current.items():
        group = groups.setdefault(row['role'], {'threads': 0, 'matched_threads': 0,
                                               'io_matched_threads': 0, 'sched_matched_threads': 0, 'delta': {}})
        group['threads'] += 1
        old = previous.get(tid)
        if old is None or old['start_ticks'] != row['start_ticks'] or old['role'] != row['role']:
            continue
        group['matched_threads'] += 1
        for key in ('minor_faults', 'major_faults', 'cpu_ticks'):
            group['delta'][key] = group['delta'].get(key, 0) + max(0, row[key] - old[key])
        if row.get('sched') is not None and old.get('sched') is not None:
            group['sched_matched_threads'] += 1
            for key, value in row['sched'].items():
                group['delta'][key] = group['delta'].get(key, 0) + max(0, value - old['sched'][key])
        if row['io'] and old['io']:
            group['io_matched_threads'] += 1
            for key in ('read_bytes', 'write_bytes', 'rchar', 'wchar'):
                if key in row['io'] and key in old['io']:
                    group['delta'][key] = group['delta'].get(key, 0) + max(0, row['io'][key] - old['io'][key])
    return groups


class JsonLog:
    def __init__(self, directory, max_bytes, backups):
        self.path = directory / 'samples.jsonl'
        self.max_bytes, self.backups = max_bytes, backups

    def write(self, row):
        data = (json.dumps(row, separators=(',', ':'), ensure_ascii=True) + '\n').encode()
        if self.path.exists() and self.path.stat().st_size + len(data) > self.max_bytes:
            oldest = Path(str(self.path) + '.' + str(self.backups))
            if oldest.exists():
                oldest.unlink()
            for index in range(self.backups - 1, 0, -1):
                src = Path(str(self.path) + '.' + str(index))
                if src.exists():
                    src.rename(Path(str(self.path) + '.' + str(index + 1)))
            self.path.rename(Path(str(self.path) + '.1'))
        fd = os.open(str(self.path), os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        with os.fdopen(fd, 'ab') as out:
            out.write(data)
        # Deliberately no fsync: diagnostic output must not force DB writeback.


class Collector:
    def __init__(self, proc='/proc'):
        self.proc = Path(proc)
        self.pid = None
        self.identity = None
        self.previous_threads = {}
        self.previous_time = None
        self.last_metrics = float('-inf')

    def find(self):
        if self.pid is not None and read(self.proc / str(self.pid) / 'comm') == 'reth-bsc\n':
            return
        candidates = []
        for entry in self.proc.iterdir():
            if entry.name.isdigit() and read(entry / 'comm') == 'reth-bsc\n':
                args = (read(entry / 'cmdline') or '').split('\0')
                if 'node' in args:
                    candidates.append(int(entry.name))
        # Do not guess when multiple node processes are present.
        self.pid = candidates[0] if len(candidates) == 1 else None

    def process(self):
        self.find()
        if self.pid is None:
            self.previous_threads = {}
            self.identity = None
            return {'status': 'no_unique_reth_node'}
        base = self.proc / str(self.pid)
        summary = stat(read(base / 'stat'))
        if summary is None:
            return {'status': 'process_exited', 'pid': self.pid}
        identity = (self.pid, summary['start_ticks'])
        result = {'pid': self.pid, 'stat': summary, 'io': keyed(read(base / 'io'))}
        if identity != self.identity:
            self.previous_threads = {}
            self.previous_time = None
            self.identity = identity
            args = (read(base / 'cmdline') or '').split('\0')
            flags = {}
            for i, value in enumerate(args):
                name, sep, suffix = value.partition('=')
                if name in FLAGS:
                    flags[name] = suffix if sep else (args[i + 1] if i + 1 < len(args) else None)
                elif name in SWITCHES:
                    flags[name] = suffix if sep else True
            # Only explicitly allowlisted configuration is recorded, never keystore passwords.
            env = dict(x.split('=', 1) for x in (read(base / 'environ') or '').split('\0') if '=' in x)
            status = read(base / 'status') or ''
            result['metadata'] = {'exe': os.readlink(str(base / 'exe')), 'flags': flags,
                                  'env': {k: env[k] for k in ENV_KEYS if k in env},
                                  'affinity': [x for x in status.splitlines() if x.startswith('Cpus_allowed_list:')]}
        threads = {}
        errors = 0
        for task in (base / 'task').iterdir():
            name = (read(task / 'comm') or '').strip()
            group = role(name)
            if group is None:
                continue
            try:
                value = stat(read(task / 'stat'))
                if value is not None:
                    threads[task.name] = dict(value, role=group, io=keyed(read(task / 'io')),
                                             sched=schedstat(read(task / 'schedstat')))
            except (OSError, ValueError, IndexError):
                errors += 1
        now = time.monotonic()
        result['thread_interval_s'] = None if self.previous_time is None else now - self.previous_time
        result['thread_groups'] = thread_delta(threads, self.previous_threads)
        result['thread_errors'] = errors
        self.previous_threads, self.previous_time = threads, now
        return result

    def sample(self):
        started = time.monotonic()
        cpu = time.process_time()
        vm = keyed(read('/proc/vmstat'))
        disks = {}
        for line in (read('/proc/diskstats') or '').splitlines():
            fields = line.split()
            if len(fields) >= 14:
                disks[fields[2]] = {'major': int(fields[0]), 'minor': int(fields[1]),
                                    'counters': [int(x) for x in fields[3:]]}
        usage = resource.getrusage(resource.RUSAGE_SELF)
        row = {'type': 'sample', 'epoch': time.time(), 'monotonic': started,
               'uptime': read('/proc/uptime'),
               'memory_kib': keyed(read('/proc/meminfo'), MEM_KEYS),
               'vm': vm_counters(vm),
               'cpu': [x for x in (read('/proc/stat') or '').splitlines() if x.startswith('cpu ')],
               'pressure': {k: read('/proc/pressure/' + k) for k in ('memory', 'io', 'cpu')},
               'diskstats': disks, 'reth': self.process(),
               'self': {'pid': os.getpid(), 'cpu_seconds': time.process_time(),
                        'max_rss_kib': usage.ru_maxrss}}
        row['self'].update(sample_cpu_s=time.process_time() - cpu,
                           sample_elapsed_s=time.monotonic() - started)
        if started - self.last_metrics >= 60:
            self.last_metrics = started
            requested = time.monotonic()
            try:
                with urllib.request.urlopen('http://127.0.0.1:6060/metrics', timeout=1) as response:
                    raw = response.read(8 * 1024 ** 2 + 1)
                if len(raw) > 8 * 1024 ** 2:
                    raise ValueError('metrics response exceeds 8MiB budget')
                selected = []
                for line in raw.decode().splitlines():
                    if line.startswith('#') or not line:
                        continue
                    name = line.split('{', 1)[0].split(' ', 1)[0]
                    if selected_metric(name):
                        selected.append(line)
                row['metrics'] = {'epoch': time.time(), 'response_bytes': len(raw),
                                  'elapsed_s': time.monotonic() - requested, 'lines': selected}
            except (OSError, ValueError) as error:
                row['metrics'] = {'error': str(error)[:200]}
        row['self'].update(sample_cpu_s=time.process_time() - cpu,
                           sample_elapsed_s=time.monotonic() - started)
        return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--interval', type=float, default=5)
    parser.add_argument('--samples', type=int, default=0, help='0 means remain running')
    parser.add_argument('--max-mib', type=int, default=16)
    parser.add_argument('--backups', type=int, default=7)
    args = parser.parse_args()
    if args.interval < 2 or args.samples < 0 or args.max_mib < 1 or not 1 <= args.backups <= 32:
        parser.error('interval >=2s; samples >=0; max-mib >=1; backups 1..32 required')
    os.umask(0o077)
    args.output.mkdir(parents=True, exist_ok=True, mode=0o700)
    lock = (args.output / 'collector.lock').open('a+')
    try:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise SystemExit('collector already running for this output directory')
    try:
        os.nice(10)
    except OSError:
        pass
    pidfile = args.output / 'collector.pid'
    pidfile.write_text(str(os.getpid()) + '\n')
    stop = [False]
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, lambda *_: stop.__setitem__(0, True))
    log = JsonLog(args.output, args.max_mib * 1024 ** 2, args.backups)
    log.write({'type': 'start', 'epoch': time.time(), 'pid': os.getpid(),
               'host': os.uname().nodename, 'interval_s': args.interval,
               'clock_ticks': os.sysconf('SC_CLK_TCK'), 'page_size': os.sysconf('SC_PAGE_SIZE'),
               'max_log_bytes': args.max_mib * 1024 ** 2 * (args.backups + 1),
               'kernel_settings': {key: read('/proc/sys/' + key) for key in (
                   'kernel/sched_schedstats', 'vm/swappiness', 'vm/vfs_cache_pressure',
                   'vm/dirty_ratio', 'vm/dirty_background_ratio', 'vm/dirty_bytes',
                   'vm/dirty_background_bytes', 'vm/dirty_expire_centisecs',
                   'vm/dirty_writeback_centisecs')}})
    collector = Collector()
    count = 0
    try:
        while not stop[0] and (args.samples == 0 or count < args.samples):
            started = time.monotonic()
            try:
                row = collector.sample()
            except (OSError, ValueError, IndexError) as error:
                row = {'type': 'sample_error', 'epoch': time.time(), 'error': str(error)[:300]}
            log.write(row)
            count += 1
            delay = args.interval - (time.monotonic() - started)
            # No catch-up bursts if collecting one sample took too long.
            until = time.monotonic() + max(0.5, delay)
            while not stop[0] and time.monotonic() < until:
                time.sleep(min(0.5, max(0, until - time.monotonic())))
        log.write({'type': 'stop', 'epoch': time.time(), 'samples': count})
    finally:
        if read(pidfile) == str(os.getpid()) + '\n':
            pidfile.unlink()


if __name__ == '__main__':
    main()
