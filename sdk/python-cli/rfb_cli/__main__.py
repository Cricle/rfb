"""Console entry point: exec the bundled native rfb-cli binary."""

import os
import stat
import subprocess
import sys

_EXE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "_bin", "rfb-cli")


def main() -> int:
    if not os.path.exists(_EXE):
        print(
            f"rfb-cli: bundled binary missing at {_EXE}; "
            "this wheel must be built by the RFB release pipeline",
            file=sys.stderr,
        )
        return 127
    if not os.access(_EXE, os.X_OK):
        try:
            os.chmod(_EXE, stat.S_IRWXU | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH)
        except OSError as error:
            print(f"rfb-cli: cannot make binary executable: {error}", file=sys.stderr)
            return 126
    return subprocess.call([_EXE, *sys.argv[1:]])


if __name__ == "__main__":
    sys.exit(main())
