#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""`prepare`'s CHANGELOG.md step, driven through the release script.

A tag is cut from a trunk commit, so every entry under `[Unreleased]` at
that commit ships in that tag. `prepare` therefore folds `[Unreleased]`
into the dated section for the version being prepared, and leaves
`[Unreleased]` empty -- which is what makes `release_metadata_pending`,
and so `cut`, report the changelog pending until a preparation pull
request commits the fold.

0.2.1 is the release that showed why. Its dated heading was opened while
nine later entries were still to come; `prepare` does nothing once the
heading exists, so those entries stayed under `[Unreleased]`, shipped in
the tag, and were filed as unreleased in the published file.

Every case runs the real `prepare --execute` against a throwaway
repository holding the files it touches, and reads the file it wrote.
"""

from __future__ import annotations

from collections import Counter
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "tools/release/tensorplate-release.sh"
BRANCH = "prep-line"
VERSION = "9.9.9"
DATED = f"## [{VERSION}] - 2026-01-02"

# Everything `prepare` rewrites apart from CHANGELOG.md, taken from the
# repository so the other steps meet the real patterns and the changelog
# step is the only thing under test.
PREPARE_SOURCES = (
    "CMakeLists.txt",
    "Cargo.toml",
    "Cargo.lock",
    "vcpkg.json",
    "packaging/VERSION",
    "packaging/debian/changelog",
    "packaging/scripts/install.sh",
)

PROLOGUE = """# Changelog

All notable changes to TensorPlate will be documented in this file.

"""

# An entry whose body is a nested paragraph, a fenced block holding a line
# that would be a heading at column 0, and a wrapped nested bullet.
NESTED_ENTRY = """- An entry with a body under it.

  A paragraph indented under the bullet.

  ```console
  $ tensorplate doctor
  ### Added
  ```

  - a nested bullet
    wrapped onto a second line."""


def git(root: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-c", "user.email=t@example.invalid", "-c", "user.name=t", *args],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    )


class ChangelogFoldTests(unittest.TestCase):
    def fixture_repo(self, changelog: str) -> Path:
        root = Path(tempfile.mkdtemp(prefix="tp-changelog-fold-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        for rel in PREPARE_SOURCES:
            (root / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(REPO_ROOT / rel, root / rel)
        (root / "CHANGELOG.md").write_text(changelog)
        git(root, "init", "-q", "-b", BRANCH, ".")
        git(root, "add", "-A")
        git(root, "commit", "-qm", "base")
        return root

    def prepare(self, root: Path, version: str = VERSION) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                str(SCRIPT),
                "prepare",
                "--version",
                version,
                "--prep-branch",
                BRANCH,
                "--execute",
                "--confirm",
                f"PREPARE-v{version}",
            ],
            cwd=root,
            capture_output=True,
            text=True,
        )

    def fold(self, changelog: str, version: str = VERSION) -> str:
        """Prepare a repository holding `changelog` and return what it wrote."""
        root = self.fixture_repo(changelog)
        result = self.prepare(root, version)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return (root / "CHANGELOG.md").read_text()

    def refuse(self, changelog: str, message: str) -> None:
        """Prepare must fail with `message` and leave the changelog alone."""
        root = self.fixture_repo(changelog)
        result = self.prepare(root, VERSION)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(message, result.stdout + result.stderr)
        self.assertEqual((root / "CHANGELOG.md").read_text(), changelog)

    # --- the fold ------------------------------------------------------

    def test_entries_fold_into_matching_and_newly_opened_subsections(self) -> None:
        # The release section holds two "### Fixed" blocks, which is what
        # accumulating entries under one version looks like -- [0.2.1] holds
        # fifteen "### Added". The moved entry goes to the top of the FIRST
        # of them, so newest-first holds for the section as a whole.
        #
        # "### Security" is a block the release section has none of, and the
        # already-released [9.9.8] below it has one. The fold's lower
        # boundary is the end of the release section, so [9.9.8]'s block is
        # not a candidate: the entry ships in 9.9.9 and a block opens there.
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- The newest fix.

### Security

- A subsection the release section has no block for.

{DATED}

### Added

- An entry the release already held.

### Fixed

- An older fix.

### Fixed

- An older fix filed in a second block.

## [9.9.8] - 2025-12-01

### Security

- An ancient security fix.

### Fixed

- An ancient fix.
"""
        )
        self.assertEqual(
            folded,
            PROLOGUE
            + f"""## [Unreleased]

{DATED}

### Security

- A subsection the release section has no block for.

### Added

- An entry the release already held.

### Fixed

- The newest fix.

- An older fix.

### Fixed

- An older fix filed in a second block.

## [9.9.8] - 2025-12-01

### Security

- An ancient security fix.

### Fixed

- An ancient fix.
""",
        )

    def test_repeated_subsections_under_unreleased_all_move(self) -> None:
        # One PR per "### Fixed" block is the shape [Unreleased] is usually
        # in: thirty of the last forty commits to touch the file left
        # repeated same-named blocks behind. Every block's entries move, in
        # the order they are written in.
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- A fix from the newest PR.

### Fixed

- A fix from the PR before it.

{DATED}

### Fixed

- An older fix.
"""
        )
        self.assertEqual(
            folded,
            PROLOGUE
            + f"""## [Unreleased]

{DATED}

### Fixed

- A fix from the newest PR.

- A fix from the PR before it.

- An older fix.
""",
        )

    def test_a_nested_entry_and_its_fenced_block_move_verbatim(self) -> None:
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

{NESTED_ENTRY}

{DATED}

### Fixed

- An older fix.
"""
        )
        self.assertIn(NESTED_ENTRY, folded)
        # The fenced "### Added" is indented under the bullet, so it is entry
        # text. Reading it as a heading would file the rest of the entry
        # under an Added block of its own.
        self.assertEqual(
            [line for line in folded.split("\n") if line.startswith("### ")],
            ["### Fixed"],
        )
        self.assertNotIn("### Added", folded.split("\n"))

    # A fence opened at column 0 is the other place a heading-shaped line
    # sits at column 0 without being a heading. Nothing in CONTRIBUTING.md
    # or the changelog docs requires an entry's fence to be indented.

    def test_a_column_0_fence_in_the_release_section_is_not_a_subsection(self) -> None:
        # Reading the quoted "### Added" as a block would file the moved
        # entry inside the fence, where it renders as code: a shipping entry
        # invisible in the published notes, at exit 0, idempotently.
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Added

- A brand new feature.

{DATED}

### Fixed

- An older fix that quotes a changelog:

```markdown
### Added
- quoted
```

- Another older fix.
"""
        )
        self.assertEqual(
            folded,
            PROLOGUE
            + f"""## [Unreleased]

{DATED}

### Added

- A brand new feature.

### Fixed

- An older fix that quotes a changelog:

```markdown
### Added
- quoted
```

- Another older fix.
""",
        )

    def test_a_shorter_run_does_not_close_a_longer_fence(self) -> None:
        # Comparing three characters reads the inner ``` as the close, so
        # the rest of the outer block comes back in scope: the quoted
        # "### Added" becomes a subsection, and the shipping entry is filed
        # under it, inside the code block, at exit 0 and idempotently.
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Added

- A brand new feature.

{DATED}

### Fixed

- An older fix that quotes a fenced changelog:

````markdown
```text
### Added
- quoted
```
````

- Another older fix.
"""
        )
        self.assertEqual(
            folded,
            PROLOGUE
            + f"""## [Unreleased]

{DATED}

### Added

- A brand new feature.

### Fixed

- An older fix that quotes a fenced changelog:

````markdown
```text
### Added
- quoted
```
````

- Another older fix.
""",
        )

    def test_a_fence_closes_only_on_its_own_character(self) -> None:
        # A ~~~ run inside a ``` block is content, not a close.
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Added

- A brand new feature.

{DATED}

### Fixed

- An older fix that quotes two fence styles:

```markdown
~~~
### Added
- quoted
~~~
```

- Another older fix.
"""
        )
        self.assertIn("- A brand new feature.\n\n### Fixed", folded)
        self.assertIn("~~~\n### Added\n- quoted\n~~~\n```\n", folded)

    def test_a_run_carrying_an_info_string_does_not_close_a_fence(self) -> None:
        # A fence is closed by its run alone. Reading a second opening
        # line as the close ends the block early and leaves the real
        # closing run opening one that is never closed, which is refused.
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Added

- A brand new feature.

{DATED}

### Fixed

- An older fix that quotes two opening lines:

````text
````python
### Added
- quoted
````

- Another older fix.
"""
        )
        self.assertIn("- A brand new feature.\n\n### Fixed", folded)
        self.assertIn("````text\n````python\n### Added\n- quoted\n````\n", folded)

    def test_a_column_0_fence_in_an_entry_moves_with_the_entry(self) -> None:
        # Reading the quoted "### Added" as a block splits one entry across
        # two subsections and leaves the fence unterminated.
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- An entry showing output:

```console
### Added
not a heading
```

- A second entry.

{DATED}

### Fixed

- An older fix.
"""
        )
        self.assertEqual(
            folded,
            PROLOGUE
            + f"""## [Unreleased]

{DATED}

### Fixed

- An entry showing output:

```console
### Added
not a heading
```

- A second entry.

- An older fix.
""",
        )

    def test_a_column_0_fence_hides_the_section_headings_it_quotes(self) -> None:
        # The "## " readers take the same view: a quoted [Unreleased] is not
        # a second [Unreleased], and a quoted dated heading is not a second
        # section. Reading them as headings refuses a legitimate file.
        folded = self.fold(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- An entry quoting what a changelog looks like:

```markdown
## [Unreleased]

## [{VERSION}] - 2020-01-01
```

{DATED}

### Fixed

- An older fix.
"""
        )
        self.assertEqual(
            folded,
            PROLOGUE
            + f"""## [Unreleased]

{DATED}

### Fixed

- An entry quoting what a changelog looks like:

```markdown
## [Unreleased]

## [{VERSION}] - 2020-01-01
```

- An older fix.
""",
        )

    def test_the_fold_is_idempotent(self) -> None:
        source = (
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- The newest fix.

{DATED}

### Fixed

- An older fix.
"""
        )
        root = self.fixture_repo(source)
        self.assertEqual(self.prepare(root).returncode, 0)
        once = (root / "CHANGELOG.md").read_text()
        self.assertNotEqual(once, source)
        git(root, "add", "-A")
        git(root, "commit", "-qm", "prepared")
        result = self.prepare(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((root / "CHANGELOG.md").read_text(), once)
        self.assertEqual(
            subprocess.run(
                ["git", "status", "--porcelain", "--", "CHANGELOG.md"],
                cwd=root,
                capture_output=True,
                text=True,
            ).stdout,
            "",
        )

    def test_an_unreleased_section_holding_no_entries_is_left_untouched(self) -> None:
        # Nothing is waiting to ship, so nothing is rewritten -- including
        # the subsection heading in the second shape, which files no entry
        # and costs the release nothing by staying where it is. A fold
        # here would report the changelog pending for a cosmetic reason.
        for shape in (
            f"""## [Unreleased]

{DATED}

### Fixed

- An older fix.
""",
            f"""## [Unreleased]

### Fixed

{DATED}

### Fixed

- An older fix.
""",
        ):
            with self.subTest(shape=shape.splitlines()[2]):
                self.assertEqual(self.fold(PROLOGUE + shape), PROLOGUE + shape)

    def test_a_version_with_no_dated_section_still_opens_one(self) -> None:
        # The behaviour that already existed: the first preparation opens
        # the dated section directly under [Unreleased], which files
        # everything below it under the new heading.
        folded = self.fold(
            PROLOGUE
            + """## [Unreleased]

### Fixed

- The newest fix.

## [9.9.8] - 2025-12-01

### Fixed

- An ancient fix.
"""
        )
        opened = folded.split("\n")[folded.split("\n").index("## [Unreleased]") + 2]
        self.assertRegex(opened, rf"^## \[{re.escape(VERSION)}\] - \d{{4}}-\d{{2}}-\d{{2}}$")
        self.assertEqual(
            folded.replace(opened, DATED),
            PROLOGUE
            + f"""## [Unreleased]

{DATED}

### Fixed

- The newest fix.

## [9.9.8] - 2025-12-01

### Fixed

- An ancient fix.
""",
        )

    # --- fail closed ---------------------------------------------------

    def test_a_changelog_without_an_unreleased_section_is_refused(self) -> None:
        self.refuse(
            PROLOGUE
            + f"""{DATED}

### Fixed

- An older fix.
""",
            "CHANGELOG.md is missing ## [Unreleased]",
        )

    def test_a_release_section_that_is_not_below_unreleased_is_refused(self) -> None:
        # Folding here would move the newest entries past a released
        # section, so the shape is refused rather than guessed at.
        self.refuse(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- The newest fix.

## [9.9.8] - 2025-12-01

### Fixed

- An ancient fix.

{DATED}

### Fixed

- An older fix.
""",
            "not by the [9.9.9] section",
        )

    def test_unreleased_as_the_last_section_of_the_file_is_refused(self) -> None:
        # The same shape as the test above with nothing after [Unreleased],
        # which is where the "what follows it" lookup runs off the end. The
        # guard must still be the thing that speaks.
        self.refuse(
            PROLOGUE
            + f"""{DATED}

### Fixed

- An older fix.

## [Unreleased]

### Fixed

- The newest fix.
""",
            f"followed by the end of the file, not by the [{VERSION}] section",
        )

    def test_a_column_0_fence_that_is_never_closed_is_refused(self) -> None:
        # Every heading below an unclosed fence would read as quoted, so the
        # file's shape cannot be established at all. Refuse, do not guess.
        self.refuse(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- An entry whose fence is never closed:

```console
$ tensorplate doctor

{DATED}

### Fixed

- An older fix.
""",
            "fenced block opened at column 0 on line 11 that is never closed",
        )

    def test_content_before_the_first_subsection_of_unreleased_is_refused(self) -> None:
        self.refuse(
            PROLOGUE
            + f"""## [Unreleased]

A loose paragraph belonging to no subsection.

### Fixed

- The newest fix.

{DATED}

### Fixed

- An older fix.
""",
            "[Unreleased] has content before its first '### ' subsection",
        )

    def test_content_before_the_first_subsection_of_the_release_is_refused(self) -> None:
        self.refuse(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- The newest fix.

{DATED}

A loose paragraph belonging to no subsection.

### Fixed

- An older fix.
""",
            "the [9.9.9] section has content before its first '### ' subsection",
        )

    def test_a_repeated_unreleased_heading_is_refused(self) -> None:
        self.refuse(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- The newest fix.

## [Unreleased]

{DATED}

### Fixed

- An older fix.
""",
            "has 2 '## [Unreleased]' headings",
        )

    def test_a_repeated_dated_heading_for_the_version_is_refused(self) -> None:
        self.refuse(
            PROLOGUE
            + f"""## [Unreleased]

### Fixed

- The newest fix.

{DATED}

### Fixed

- An older fix.

## [{VERSION}] - 2025-11-01

### Fixed

- A duplicate section.
""",
            f"has 2 '## [{VERSION}] - ' headings",
        )

    # --- against the repository's own changelog -------------------------

    def test_the_repositorys_changelog_folds_without_losing_a_line(self) -> None:
        # The file this exists for. Whatever shape it is in -- entries
        # waiting under [Unreleased] or none -- preparing the version the
        # tree declares must move text and never rewrite or drop it, and
        # must leave [Unreleased] holding nothing.
        version = (REPO_ROOT / "packaging/VERSION").read_text().strip()
        source = (REPO_ROOT / "CHANGELOG.md").read_text()
        folded = self.fold(source, version)

        def content(text: str) -> Counter:
            return Counter(
                line
                for line in text.split("\n")
                if line.strip() and not line.startswith(("## ", "### "))
            )

        self.assertEqual(content(folded), content(source))
        lines = folded.split("\n")
        start = lines.index("## [Unreleased]")
        stop = next(
            i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")
        )
        self.assertEqual([line for line in lines[start + 1 : stop] if line.strip()], [])
        self.assertTrue(lines[stop].startswith(f"## [{version}] - "))


if __name__ == "__main__":
    unittest.main(verbosity=2)
