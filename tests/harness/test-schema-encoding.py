"""Check schema reports under Windows legacy stdout encodings without installation."""
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]


class SchemaReportEncodingTests(unittest.TestCase):
    def test_m3_preserves_localized_cases_with_ascii_and_windows_stdout(self):
        reports = []
        for encoding in ("ascii", "cp1252", "utf-8"):
            with self.subTest(encoding=encoding):
                env = dict(os.environ, PYTHONIOENCODING=encoding)
                result = subprocess.run(
                    [sys.executable, str(ROOT / "tests/harness/check-m3-schemas.py")],
                    cwd=ROOT,
                    env=env,
                    capture_output=True,
                    timeout=60,
                    check=False,
                )
                self.assertEqual(
                    result.returncode, 0, result.stderr.decode("utf-8", errors="replace")
                )
                # CI reads UTF-8 regardless of the child's system code page.
                report = json.loads(result.stdout.decode("utf-8"))
                self.assertEqual(report["status"], "PASS")
                self.assertTrue(report["positive_cases"])
                self.assertTrue(report["negative_cases"])
                self.assertTrue(
                    any(any(ord(char) > 127 for char in case)
                        for case in report["negative_cases"]),
                    "Localized cases must be retained, not removed to avoid encoding failures",
                )
                reports.append(report)
        self.assertTrue(all(report == reports[0] for report in reports))


if __name__ == "__main__":
    unittest.main()
