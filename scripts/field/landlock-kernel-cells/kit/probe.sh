#!/bin/sh
# Runs inside the cell, writing to the serial console. The kit ISO carries
# the static test binary (bwn-tests) and the bwn binary it execs under
# bubblewrap (BWN_TEST_CONFINE_EXE).
exec > /dev/ttyS0 2>&1
echo "=== PROBE START $(uname -r)"
echo "lsm: $(cat /sys/kernel/security/lsm 2>/dev/null)"
K=$(dirname "$0")
cp "$K/bwn-tests" /root/bwn-tests
cp "$K/bwn" /root/buildwithnexus
chmod +x /root/bwn-tests /root/buildwithnexus
cd /root
for t in landlock:: invariant_m2 invariant_m5 invariant_m8c sandbox::tests::landlock; do
  echo "--- $t"
  BWN_TEST_SANDBOX_BACKEND=landlock BWN_TEST_CONFINE_EXE=/root/buildwithnexus \
    ./bwn-tests --test-threads=1 --nocapture "$t" 2>&1 \
    | grep -E "^test |panicked|ABI|test result|truncated|ruleset" | head -60
done
echo "=== PROBE END"
poweroff
