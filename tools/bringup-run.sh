#!/usr/bin/env bash
# Launcher wrapper for bringup.sh.
#
# Exists purely to own the quoting. Passing `script -q -c "bash …" log` through
# ssh -> bash -> konsole -e loses the inner quotes, and `script` then runs a bare
# interactive `bash` with the rest as filenames — which looks like a silent hang. konsole
# gets exactly one argument (this file) and there is nothing left to mis-parse.
#
# `script` is here for the PTY, not the transcript: a real terminal keeps the child's output
# unbuffered and live, while still capturing it for later reading.
# -f flushes the transcript after every write. Without it `script` buffers the log file, so
# the window shows progress live while anyone reading the log sees an empty file and
# concludes it has hung.
exec script -qf -c "bash $HOME/spatiand/tools/bringup.sh" /tmp/spatiand-bringup.log
