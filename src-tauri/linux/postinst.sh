#!/bin/sh
# Runs as root after the package is installed or upgraded (.deb postinst, .rpm %post).
#
# Gives the program the one right it needs to read the game's memory under Proton: CAP_SYS_PTRACE, which lets it
# call process_vm_readv on a process it did not start (crates/core/src/proc/linux.rs). It reads; it never
# writes or attaches. An upgrade installs a new file without the right, so this runs again then.
#
# Never fails the install: without the right the app still runs, and its status line says what to do.

BIN=/usr/bin/zar-logs

if command -v setcap >/dev/null 2>&1 && setcap cap_sys_ptrace=ep "$BIN"; then
    :
else
    echo "zar-logs: could not give $BIN the right to read the game; run: sudo setcap cap_sys_ptrace=ep $BIN" >&2
fi
exit 0
