# Offline pure-Python package baked into the sandbox image at
# /usr/lib/python3/site-packages (the guest sys.path root).
# The zeroboot_interpreters real-VM test imports exactly this module and
# asserts demo.VALUE == 42.
VALUE = 42
