"""Check warm-up accounting and the slowest-frame definition."""
import unittest

from analyze_frame_times import summarize


class FrameTimesTests(unittest.TestCase):
    def test_one_percent_low_uses_the_tail_mean_and_keeps_real_stalls(self):
        data = dict(schema=1, frequency=1000, warmup_ms=5000,
                    ticks=[1000]*5 + [10]*99 + [100], diagnostic_frames=[])
        result = summarize(data)
        sample = result['steady_state']
        self.assertEqual(result['excluded_warmup_frames'], 5)
        self.assertEqual(sample['frames'], 100)
        self.assertEqual(sample['p99_ms'], 10)
        self.assertEqual(sample['one_percent_low_fps'], 10)
        self.assertAlmostEqual(sample['average_fps'], 100000/1090)

    def test_only_explicit_validation_frames_are_excluded(self):
        result = summarize(dict(schema=1, frequency=1000, warmup_ms=0,
                                ticks=[10]*99 + [100], diagnostic_frames=[99]))
        self.assertEqual(result['excluded_diagnostic_frames'], 1)
        self.assertEqual(result['steady_state']['average_fps'], 100)
        self.assertEqual(result['full_run']['one_percent_low_fps'], 10)


if __name__ == '__main__':
    unittest.main()
