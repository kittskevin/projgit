#!/bin/sh
# fsmonitor hook (query-fsmonitor) protocol v2, answered from the
# vworktree harness's "write log" file ($VWORKTREE_FSM).
#
# git invokes us as:  hook <version> <last_token>
# Required stdout:    <token>\0 <changed-path>\0 <changed-path>\0 ...
#
# The harness OWNS the file's exact bytes: a monotonic token followed by
# the cumulative set of paths modified since mount, each NUL-terminated.
# We stream it verbatim. This is the spike's stand-in for "projgit's
# daemon IS the FSMonitor": the daemon authoritatively knows every write,
# so it answers the modified-paths query with ZERO filesystem scanning.
# An advancing token tells git the state changed; the path list tells it
# exactly which entries to re-examine (everything else stays trusted-clean).

if [ -n "$VWORKTREE_FSM" ] && [ -s "$VWORKTREE_FSM" ]; then
  cat "$VWORKTREE_FSM"
else
  printf '%s\0' "0"
fi
