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
#     (delete::tests_windows), the named-pipe code (winpipe), the helper's
#     scan-root judgement (scan::windows_pin_tests), and the interface's
#     elevation and delete code for Windows (elevate::windows and
#     delete::windows),
#   - four named scan-root tests passed (listed in the script),
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
for prefix in 'delete::tests_windows::' 'winpipe::' 'scan::windows_pin_tests::' 'elevate::windows::' 'delete::windows::'; do
  count="$(grep -c "^test ${prefix}.* \.\.\. ok$" <<< "$text" || true)"
  if [ "$count" -lt 1 ]; then
    echo "no passing test named ${prefix}* in the log" >&2
    fail=1
  else
    echo "ok: ${count} passing tests named ${prefix}*"
  fi
done
# The helper's scan-root judgement is checked by name: these four need nothing
# the runner might lack, so each must have run and not failed. (The junction,
# link, share and mount-point tests beside them print NOT RUN where the runner
# cannot make those, and are not required by name.) With --nocapture a test
# that prints while it runs puts its result on a later line, so a test counts
# when its line is in the log with a result of ok, or with no result line of
# FAILED and no place in the list of failures at the end.
for name in a_folder_is_judged_and_the_pin_is_that_folder \
  a_judged_folder_scans_with_its_pin_in_the_options \
  a_file_as_the_root_is_refused \
  a_folder_swapped_after_the_check_is_refused_with_nothing_sent; do
  full="scan::windows_pin_tests::${name}"
  if grep -q "^test ${full} \.\.\. ok$" <<< "$text"; then
    echo "ok: ${full}"
  elif grep -q "^test ${full} \.\.\. " <<< "$text" \
    && ! grep -q "^test ${full} \.\.\. FAILED$" <<< "$text" \
    && ! grep -qx "    ${full}" <<< "$text"; then
    echo "ok: ${full} (result printed on a later line)"
  else
    echo "${full} did not run, or did not pass, in the log" >&2
    fail=1
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
