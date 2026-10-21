#!/usr/bin/env python3
"""Print the tail of a failed step's log as one GitHub Actions annotation.

A boot or fixture failure is a thousands-of-lines serial dump, and the interesting part is
the assertion at the end. The workflow steps redirect the fixture output to a file and call
this on failure, so the annotation on the run page names what actually broke instead of
showing the first kilobytes of SeaBIOS output.

Usage: annotate-failure.py TITLE LOGFILE [TAIL_BYTES]
"""

import pathlib
import sys


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        return 2
    title, name = sys.argv[1], sys.argv[2]
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else 3000
    path = pathlib.Path(name)
    if not path.exists():
        tail = f'{name} was never written'
    else:
        tail = path.read_text(errors='replace')[-limit:]
    # Workflow commands carry one line: escape percent first, then the line breaks.
    escaped = tail.replace('%', '%25').replace('\r', '').replace('\n', '%0A')
    print(f'::error title={title}::{escaped}', flush=True)
    return 0


if __name__ == '__main__':
    sys.exit(main())
