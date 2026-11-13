"""Summarize source-frame timings from a completed presentation workload."""
import argparse
import json
import math
from pathlib import Path


def distribution(ticks, frequency):
    if not ticks:
        raise ValueError('no frame samples')
    ordered = sorted(ticks)
    count = len(ordered)
    tail_count = math.ceil(count / 100)
    scale = 1000 / frequency
    percentile = lambda fraction: ordered[math.ceil(count * fraction)-1] * scale
    return {
        'frames': count,
        'sample_seconds': sum(ordered) / frequency,
        'average_fps': count * frequency / sum(ordered),
        'one_percent_low_fps': tail_count * frequency / sum(ordered[-tail_count:]),
        'p50_ms': percentile(.50),
        'p95_ms': percentile(.95),
        'p99_ms': percentile(.99),
        'max_ms': ordered[-1] * scale,
        'frames_over_16_67_ms': sum(t * scale > 1000/60 for t in ordered),
        'frames_over_33_33_ms': sum(t * scale > 1000/30 for t in ordered),
    }


def summarize(data):
    frequency = data['frequency']
    ticks = data['ticks']
    if data.get('schema') != 1 or frequency <= 0 or any(t <= 0 for t in ticks):
        raise ValueError('invalid frame timing data')
    diagnostics = set(data['diagnostic_frames'])
    if any(i < 0 or i >= len(ticks) for i in diagnostics):
        raise ValueError('invalid diagnostic frame index')
    cutoff = data['warmup_ms'] * frequency / 1000
    elapsed = 0
    kept = []
    excluded_warmup = 0
    excluded_diagnostics = 0
    for index, duration in enumerate(ticks):
        if elapsed < cutoff:
            excluded_warmup += 1
        elif index in diagnostics:
            excluded_diagnostics += 1
        else:
            kept.append(duration)
        elapsed += duration
    return {
        'schema': 1,
        'full_run': distribution(ticks, frequency),
        'steady_state': distribution(kept, frequency),
        'excluded_warmup_frames': excluded_warmup,
        'excluded_diagnostic_frames': excluded_diagnostics,
        'one_percent_low_definition': 'reciprocal mean duration of the slowest ceil(1%) of frames',
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', type=Path)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    result = json.dumps(summarize(json.loads(args.input.read_text())), indent=2)+'\n'
    if args.output:
        args.output.write_text(result)
    else:
        print(result, end='')


if __name__ == '__main__':
    main()
