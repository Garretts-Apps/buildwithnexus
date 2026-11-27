#!/bin/sh
# Ubuntu 24.04 only, run by probe.sh as root after the root tests. Answers,
# as an unprivileged user with AppArmor's user-namespace restriction at its
# default: can bwn's namespace backend run, can bubblewrap, and can a
# command join a long-lived holder's user + network namespace (the
# per-session private network namespace, roadmap 2.2)?
echo "--- userns probe"
echo "restrict_unprivileged_userns=$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null)"
DEBIAN_FRONTEND=noninteractive apt-get install -y -q bubblewrap >/root/apt.log 2>&1 \
  && echo "bwrap: $(bwrap --version)" || { echo "bwrap install failed"; tail -5 /root/apt.log; }
id probe >/dev/null 2>&1 || useradd -m probe
cp /root/bwn-tests /root/buildwithnexus /home/probe/ && chown probe /home/probe/bwn-tests /home/probe/buildwithnexus
as() { runuser -u probe -- sh -c "cd /home/probe && $1" 2>&1 | head -20; }
echo "[1] plain unshare -Urn, lo up:"
as 'unshare -Urn sh -c "ip link set lo up && echo LO_UP"'
echo "[2] bwn namespace backend forced:"
as 'BWN_TEST_SANDBOX_BACKEND=userns ./bwn-tests --test-threads=1 invariant_userns 2>&1 | grep -E "^test |unavailable|test result"'
echo "[3] bwrap with its own user + net namespace:"
as 'bwrap --unshare-user --unshare-net --ro-bind / / --dev /dev --proc /proc sh -c "ip link set lo up && ip -o link | cut -d: -f2 && echo BWRAP_NET_OK"'
echo "[4] long-lived bwrap holder, then join it:"
as 'bwrap --unshare-user --unshare-net --ro-bind / / --bind /tmp /tmp --dev /dev --proc /proc sh -c "ip link set lo up; echo \$\$ > /tmp/holder.pid; exec sleep 300" & sleep 3; P=$(cat /tmp/holder.pid); echo holder=$P;
  nsenter --preserve-credentials -U -n -t $P sh -c "ip -o link | cut -d: -f2; echo JOINED";
  echo "[4b] bwrap after joining (no new user ns):";
  nsenter --preserve-credentials -U -n -t $P bwrap --ro-bind / / --dev /dev --proc /proc --share-net sh -c "ip -o link | cut -d: -f2; echo BWRAP_IN_HOLDER_OK";
  echo "[4c] bwrap after joining, with a nested user ns:";
  nsenter --preserve-credentials -U -n -t $P bwrap --unshare-user --ro-bind / / --dev /dev --proc /proc --share-net sh -c "ip -o link | cut -d: -f2; echo BWRAP_NESTED_OK";
  echo "[4d] bwn namespace backend after joining:";
  nsenter --preserve-credentials -U -n -t $P sh -c "BWN_TEST_SANDBOX_BACKEND=userns ./bwn-tests --test-threads=1 invariant_userns_confines 2>&1 | grep -E \"^test |unavailable|test result\"";
  kill $P 2>/dev/null'
echo "--- userns probe end"
