#!/usr/bin/env bash
# Reads the output of the Windows helper and file-operation test run and fails
# unless the tests that matter for the elevated path actually ran:
#
#   packaging/ci/check-windows-helper-tests.sh LOGFILE
#
# The log is the output of
#   cargo test -p bs-fsops -p bs-ipc -p bs-platform -p bytescape-helper -- --nocapture --test-threads=1
# A test that cannot do its job prints NOT RUN and still passes, so a green
# run alone proves nothing. This checks that
#   - the helper's launch test program (tests/windows_launch.rs) ran,
#   - at least one test passed in each of: the helper's Windows delete tests
#     (delete::tests_windows), the named-pipe code (winpipe), and the
#     interface's elevation and delete code for Windows (elevate::windows and
#     delete::windows),
#   - and no test reported NOT RUN because BYTESCAPE_DISPOSABLE_HOST was
#     missing. (Other NOT RUN lines are real capability checks, such as a file
#     system that cannot make a symbolic link, and are allowed.)

set -euo pipefail

log="${1:?usage: check-windows-helper-tests.sh LOGFILE}"
# Colored cargo output is stripped before matching.
text="$(sed 's/\x1b\[[0-9;]*m//g' "$log")"
fail=0

if ! grep -q 'Running tests[/\\]windows_launch\.rs' <<< "$text"; then
  echo "the helper's windows_launch test program did not run" >&2
  fail=1
fi
for prefix in 'delete::tests_windows::' 'winpipe::' 'elevate::windows::' 'delete::windows::'; do
  count="$(grep -c "^test ${prefix}.* \.\.\. ok$" <<< "$text" || true)"
  if [ "$count" -lt 1 ]; then
    echo "no passing test named ${prefix}* in the log" >&2
    fail=1
  else
    echo "ok: ${count} passing tests named ${prefix}*"
  fi
done
if grep -q 'NOT RUN:.*BYTESCAPE_DISPOSABLE_HOST' <<< "$text"; then
  echo "a test did not run because BYTESCAPE_DISPOSABLE_HOST is not set:" >&2
  grep 'NOT RUN:.*BYTESCAPE_DISPOSABLE_HOST' <<< "$text" >&2
  fail=1
fi
if grep -q '^test .* \.\.\. FAILED$' <<< "$text"; then
  echo "failing tests in the log" >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "::error title=Windows helper tests did not all run::see the messages above"
  exit 1
fi
echo "the Windows helper tests ran"
