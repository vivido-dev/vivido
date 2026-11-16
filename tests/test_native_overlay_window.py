"""Portable checks for native-window acceptance prerequisites."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import native_overlay_window as acceptance


class PreflightTests(unittest.TestCase):
    def test_missing_capture_permission_fails_before_launching_a_window(self):
        preflight = {"locked": False, "screen_capture_allowed": False}
        completed = subprocess.CompletedProcess([], 0, json.dumps(preflight), "")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "run"
            with (patch.object(acceptance.subprocess, "run", return_value=completed),
                  patch.object(acceptance.subprocess, "Popen") as spawn):
                with self.assertRaisesRegex(RuntimeError, "Screen Recording.*Vivido"):
                    acceptance.run(Path("vivido"), Path("python"), output)
            spawn.assert_not_called()
            report = json.loads((output / "result.json").read_text())
            self.assertEqual(report["preflight"], preflight)
            self.assertEqual(report["exit_code"], 1)

    def test_locked_desktop_fails_even_with_capture_permission(self):
        with self.assertRaisesRegex(RuntimeError, "unlocked macOS session"):
            acceptance.validate_preflight({"locked": True, "screen_capture_allowed": True})

    def test_unlocked_authorized_desktop_passes(self):
        acceptance.validate_preflight({"locked": False, "screen_capture_allowed": True})


if __name__ == "__main__":
    unittest.main()
