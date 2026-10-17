#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise label shapes through the public scanner without committing values.

Identifiers are assembled only in temporary message files. Each rejection
has a nearby passing form, and diagnostics must never print the identifier.
"""

from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCANNER = ROOT / "tools/validation/check-public-hygiene.sh"


class PrivateLabelTests(unittest.TestCase):
    def scan(self, text: str, rejected: bool, identifier: str = "") -> None:
        with tempfile.TemporaryDirectory() as tmp:
            message = Path(tmp) / "message.txt"
            message.write_text(text + "\n")
            result = subprocess.run(
                [str(SCANNER), "--base", "HEAD", "--message", str(message)],
                cwd=ROOT, capture_output=True, text=True, timeout=30,
            )
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, int(rejected), output)
        if rejected:
            self.assertIn("message#1:1: private-label", output)
            self.assertNotIn(identifier or text, output)
        else:
            self.assertIn("publishable:", output)

    def test_extension_identifiers_and_near_misses(self) -> None:
        for family in ("E", "O"):
            for suffix in ("", "a", "b"):
                label = f"X{family}-{7:02d}{suffix}"
                with self.subTest(family=family, suffix=suffix):
                    for text in (label, f"({label})", f"notes_{label}_draft"):
                        self.scan(text, True, label)
                    for text in (f"word{label}", f"{label}word", f"{label}9"):
                        self.scan(text, False)
        for digits in ("7", "007"):
            for family in ("E", "O"):
                self.scan(f"X{family}-{digits}", False)

    def test_step_identifiers_and_near_misses(self) -> None:
        for family, numbers, suffixes in (
            ("A", range(1, 4), ("",)),
            ("B", (1,), ("",)),
            ("B", (2,), ("", "a", "b")),
            ("C", range(1, 4), ("", "a", "b")),
            ("D", (1,), ("",)),
            ("F", (1,), ("",)),
        ):
            for number in numbers:
                for suffix in suffixes:
                    label = f"{family}{number}{suffix}"
                    with self.subTest(family=family, number=number, suffix=suffix):
                        for text in (f"({label})", f"( `{label}` )", f"row {label}", f"Row `{label}`"):
                            self.scan(text, True, label)
                        for text in (
                            label, f"(word{label})", f"({label}word)",
                            f"row {label}word", f"arrow {label}",
                            f"({label}000)", f"row {label}000",
                            f"digest: aa{label}0123456789abcdef",
                        ):
                            self.scan(text, False)


if __name__ == "__main__":
    unittest.main()
