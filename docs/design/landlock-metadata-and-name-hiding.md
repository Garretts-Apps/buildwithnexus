# Closing the Landlock metadata and name-existence gaps

Status: design note for roadmap 2.2. Nothing here is implemented. The owner has
not yet decided.

This note reviews an earlier recommendation ("build a user-namespace mount
fallback, not a seccomp supervisor") against the man pages and the kernel's
Landlock documentation, and against tests run for this note.

**Verdict: the direction is right, four details in it were wrong, and several
things were missing.** Use a mount namespace, not a supervisor. But do not map
the command to root, do not rely on `remount,ro,bind /`, and add the new mount
syscalls to the seccomp deny-list. The corrections are in "What the earlier
recommendation got wrong".

Every claim below is marked by how it is known:

- **Source**: quoted from a man page or kernel document, linked.
- **Tested**: run for this note on Linux 6.18.44 as uid 1000 (no root, no setuid
  helper), in a container with 26 mounts. The method is in the appendix.
- **Needs a test**: reasoned from code or from memory. Not to be relied on until
  a test says so.

## The two gaps

Both exist only under the built-in Landlock + seccomp backend. Bubblewrap and
Seatbelt confine both (one exception for bubblewrap is in "Missing"). Both are
disclosed in the capability table (`harness/src/capabilities.rs`) and the
startup check, and under Landlock auto mode asks before every command (roadmap
1.8).

1. **A file's metadata outside the workspace is not confined.** `chmod`,
   `chown`, `utime` and the xattr calls succeed on any file the command can
   name. The descriptor form gets past any path filter: `open(path, O_RDONLY)`
   then `fchmod(fd)`. Proven by `landlock_does_not_confine_file_metadata_outside`.

2. **A hidden store's file names leak.** Listing a hidden store is refused
   (M5), but `stat ~/.ssh/id_ed25519` succeeds, and `open` returns `EACCES` for
   a name that exists and `ENOENT` for one that does not. Contents stay
   unreadable. Proven by `landlock_credential_names_leak_via_stat`.

## What the primary sources say

### Landlock has no metadata right, on any ABI

Source, [landlock(7)](https://man7.org/linux/man-pages/man7/landlock.7.html),
CAVEATS (the kernel document carries the same text as a warning):

> It is currently not possible to restrict some file-related actions accessible
> through these system call families: chdir(2), stat(2), flock(2), chmod(2),
> chown(2), setxattr(2), utime(2), fcntl(2), access(2). Future Landlock
> evolutions will enable to restrict them.

That one sentence covers both gaps: `chmod`/`chown`/`setxattr`/`utime` for gap
1, `stat` and `access` for gap 2.

The ABI table in landlock(7) now runs to ABI 9. The earlier note stopped at
ABI 6 and called it current; it is not.

| ABI | Kernel | Added |
|---|---|---|
| 1 | 5.13 | `FS_EXECUTE`, `WRITE_FILE`, `READ_FILE`, `READ_DIR`, `REMOVE_DIR`, `REMOVE_FILE`, `MAKE_CHAR`, `MAKE_DIR`, `MAKE_REG`, `MAKE_SOCK`, `MAKE_FIFO`, `MAKE_BLOCK`, `MAKE_SYM` |
| 2 | 5.19 | `FS_REFER` |
| 3 | 6.2 | `FS_TRUNCATE` |
| 4 | 6.7 | `NET_BIND_TCP`, `NET_CONNECT_TCP` |
| 5 | 6.10 | `FS_IOCTL_DEV` |
| 6 | 6.12 | `SCOPE_ABSTRACT_UNIX_SOCKET`, `SCOPE_SIGNAL` |
| 7 | 6.15 | logging flags for `landlock_restrict_self` |
| 8 | 7.0 | `RESTRICT_SELF_TSYNC` |
| 9 | 7.1 | `FS_RESOLVE_UNIX` (connect to a pathname Unix socket) |

The kernel's own document
([docs.kernel.org/userspace-api/landlock.html](https://docs.kernel.org/userspace-api/landlock.html),
dated November 2026) also lists ABI 10 (UDP rights, a quiet-rule flag), ABI 11
(a `NO_NEW_PRIVS` flag) and ABI 12 (capability and namespace permissions).
Which kernel release carries each of 10 to 12 is not in the man page table yet:
needs checking before bwn keys anything on them. None of the twelve adds a
right over metadata or over `stat`. Waiting for Landlock is not a plan.

Two other statements in the kernel document matter for the design below:

> Threads sandboxed with filesystem restrictions cannot modify filesystem
> topology, whether via mount(2) or pivot_root(2).

> A bind mount mirrors a source file hierarchy to a destination. The
> destination hierarchy is then composed of the exact same files, on which
> Landlock rules can be tied, either via the source or the destination path.

### seccomp user notification is not for security policy

Source, [seccomp_unotify(2)](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html),
"Design goals; use of SECCOMP_USER_NOTIF_FLAG_CONTINUE":

> Note well: this mechanism must not be used to make security policy decisions
> about the system call, which would be inherently race-prone for reasons
> described next.
>
> The SECCOMP_USER_NOTIF_FLAG_CONTINUE flag must be used with caution. If set
> by the supervisor, the target's system call will continue. However, there is
> a time-of-check, time-of-use race here, since an attacker could exploit the
> interval of time where the target is blocked waiting on the "continue"
> response to do things such as rewriting the system call arguments.
>
> [...] It should thus be absolutely clear that the seccomp user-space
> notification mechanism can not be used to implement a security policy! [...]
> in order to continue a system call, the supervisor should be sure that another
> security mechanism or the kernel itself will sufficiently block the system
> call if its arguments are rewritten to something unsafe.

The man page is stronger than the earlier note said. It does not only warn
about `CONTINUE`; it says the mechanism is not for security decisions at all.
The reading that stays within it: a supervisor may never answer "continue" for
a call it checked by path, because nothing else blocks the rewritten call (that
is the gap). It would have to copy the arguments once, do the operation itself
on its copy, and return a spoofed result.

### Unprivileged user and mount namespaces

Source, [user_namespaces(7)](https://man7.org/linux/man-pages/man7/user_namespaces.7.html):

> Since Linux 3.8, unprivileged processes can create user namespaces, and the
> other types of namespaces can be created with just the CAP_SYS_ADMIN
> capability in the caller's user namespace.

> If CLONE_NEWUSER is specified along with other CLONE_NEW* flags in a single
> clone(2) or unshare(2) call, the user namespace is guaranteed to be created
> first, giving the child (clone(2)) or caller (unshare(2)) privileges over the
> remaining namespaces created by the call.

> Holding CAP_SYS_ADMIN within the user namespace that owns a process's mount
> namespace allows that process to create bind mounts and mount the following
> types of filesystems: [...]

(tmpfs is in that list.) And the sentence the earlier note missed:

> Note that a call to execve(2) will cause a process's capabilities to be
> recalculated in the usual way (see capabilities(7)). Consequently, unless the
> process has a user ID of 0 within the namespace, or the executable file has a
> nonempty inheritable capabilities mask, the process will lose all
> capabilities.

Source, [unshare(2)](https://man7.org/linux/man-pages/man2/unshare.2.html):
"CLONE_NEWUSER requires that the calling process is not threaded". bwn is
threaded, so the call has to happen in the forked child or a helper process.

Source, [mount_namespaces(7)](https://man7.org/linux/man-pages/man7/mount_namespaces.7.html),
"Restrictions on mount namespaces":

> [2] When creating a less privileged mount namespace, shared mounts are
> reduced to slave mounts. This ensures that mappings performed in less
> privileged mount namespaces will not propagate to more privileged mount
> namespaces.
>
> [3] Mounts that come as a single unit from a more privileged mount namespace
> are locked together and may not be separated in a less privileged mount
> namespace.
>
> [5] The mount(2) flags MS_RDONLY, MS_NOSUID, MS_NOEXEC, and the "atime" flags
> [...] settings become locked when propagated from a more privileged to a less
> privileged mount namespace, and may not be changed in the less privileged
> mount namespace.

The same page gives `mount --make-rprivate /` (or `MS_SLAVE | MS_REC`) as the
step that stops mount events crossing between namespaces.

So the pattern needs no privilege: `unshare(CLONE_NEWUSER | CLONE_NEWNS)`,
write the id maps, set propagation, bind, mount tmpfs, set read-only.

## What the tests showed

All run as uid 1000 on 6.18.44. "Identity map" means the command's uid maps to
itself in the new namespace; "root map" means it maps to 0.

| Probe, inside the namespace | Identity map | Root map | Root map + Landlock |
|---|---|---|---|
| `chmod`, `utimensat`, `setxattr` on a file outside the workspace | `EROFS` | `EROFS` | `EROFS` |
| `fchmod`, `fchown`, `futimens`, `fsetxattr` on an `O_RDONLY` fd of it | `EROFS` | `EROFS` | `EROFS` |
| `chmod /proc/self/fd/N` of that fd | `EROFS` | `EROFS` | `EROFS` |
| `chmod` and create inside the workspace | works | works | works |
| `stat` and `open` of a real name in the covered store | `ENOENT` | `ENOENT` | `ENOENT` |
| `stat` and `open` of an absent name | `ENOENT` | `ENOENT` | `ENOENT` |
| Capabilities after `execve` (`CapEff`) | none | all | all |
| `umount2` of the tmpfs over the store | `EPERM` | **works** | `EPERM` |
| Remount `/` read-write | `EPERM` | **works** | `EPERM` |
| `open_tree(home, OPEN_TREE_CLONE)` then `openat(".ssh/id_test")` | `EPERM` | **reads the key** | **reads the key** |
| Nested `unshare(NEWUSER\|NEWNS)`, then `umount2` / remount rw | `EINVAL` / `EPERM` | then `stat` of the key and `chmod` outside **work** | `EINVAL` / `EPERM` |
| `open /proc/<host pid>/root/<store>/id_test` | `EACCES` | `EACCES` | `EACCES` |

Also tested:

- With only `mount -o remount,ro,bind /` (not recursive), files on submounts
  stayed writable: `/dev/shm` and a tmpfs mounted under the home directory.
  `mount_setattr(2)` with `AT_RECURSIVE` and `MOUNT_ATTR_RDONLY` made the whole
  tree read-only in one call.
- Setup cost: `unshare`, id maps, propagation, one bind, one tmpfs, recursive
  read-only, a three-rule Landlock ruleset, then `exec` took about 2.1 ms a
  command against 1.1 ms for a plain `exec` (300 runs each). So about 1 ms a
  command on a 26-mount tree. A machine with hundreds of mounts needs measuring.

The Landlock ruleset in these tests handled write rights only. bwn's real
ruleset also handles reads when stores are hidden, so in bwn the `open_tree`
row would likely return the existence oracle rather than the key's contents.
Needs a test with the real ruleset. The conclusion does not depend on it.

## What the earlier recommendation got wrong

1. **"Map root" reopens both gaps.** With uid 0 in the namespace the command
   keeps `CAP_SYS_ADMIN` over its own mount namespace after `execve`. It can
   unmount the tmpfs over the store and remount `/` read-write (tested). Map
   the uid to itself instead. The process still holds full capabilities between
   `unshare` and `execve`, which is when the mounts are made, and loses them at
   `execve` (source and tested). This is also what bubblewrap does by default.

2. **"Then apply Landlock on top" does not rescue a root map.** Landlock
   refused `umount2` and the remount, as its document says, but not
   `open_tree(2)`, which hands back a copy of the tree without the tmpfs
   (tested on 6.18). The kernel document says ABI 12 adds a namespace permission
   that covers `open_tree` and `fsmount`; bwn cannot require that.

3. **`remount,ro,bind /` is not recursive.** `/home`, `/var` or `/dev/shm` on
   their own mounts stay writable (tested). Use `mount_setattr` with
   `AT_RECURSIVE`. It arrived in Linux 5.12
   ([mount_setattr(2)](https://man7.org/linux/man-pages/man2/mount_setattr.2.html),
   HISTORY) and Landlock in 5.13, so every kernel that reaches this code has it.
   A container seccomp profile may still refuse it: the probe decides.

4. **The reason given for `make-rprivate` was wrong.** The earlier note said
   binds would otherwise leak to the host. The kernel already reduces shared
   mounts to slave mounts in a less privileged namespace (source, [2] above).
   The step is still needed, for the other direction: without it a mount or
   unmount made on the host later propagates into the command's tree.

5. **Landlock's ABI list was stale** (see the table).

6. **The ptrace option was misdescribed.** A tracer does not have to stop every
   syscall; with a seccomp `RET_TRACE` filter it stops only the filtered ones.
   It is rejected for the same reason as option A, not for overhead.

## Missing from the earlier recommendation

- **The seccomp deny-list does not name the new mount API.** `seccomp()` in
  `landlock.rs` refuses `mount`, `umount2`, `pivot_root`, `unshare` and `setns`,
  but not `open_tree`, `move_mount`, `fsopen`, `fsconfig`, `fsmount`, `fspick`
  or `mount_setattr`. Without capabilities these fail anyway (tested: `EPERM`),
  so this is a second layer, but it should be there before this backend ships.
  `clone`/`clone3` with `CLONE_NEWUSER` stay allowed and that is safe: in a
  nested namespace the mounts are locked (source [3] and [5]; tested).

- **The working directory.** A process's cwd keeps pointing at the mount it was
  on. After the workspace is bound over itself, a cwd set earlier is on the
  mount underneath, which is now read-only. The child must `chdir` again after
  the mounts. The test did; the failure without it is expected, not tested.

- **Landlock rules and a private `/tmp`.** Rules attach to files, so a rule
  built in the parent still applies through a bind mount (source above). A
  fresh tmpfs is a different file tree: a parent-built rule for `/tmp` would
  not match it and writes there would be refused. Either bind the host `/tmp`
  or build the ruleset after the mounts. Needs a test either way.

- **The probe must do the mounts, not only `unshare`.** From memory, Ubuntu
  24.04's AppArmor policy lets an unconfined program create the namespace and
  then denies it capabilities inside, so `unshare` succeeds and the first mount
  fails. Needs a test in a 24.04 cell. The probe should run the whole sequence
  once in a throwaway child.

- **Hidden files, as opposed to hidden folders, keep the oracle, and under
  bubblewrap too.** `bwrap_argv` covers a hidden folder with an empty tmpfs but
  a hidden file (`~/.netrc`, `~/.git-credentials`) with `/dev/null`, and
  `hidden_paths_in` only lists entries that exist. So `stat ~/.netrc` succeeds
  exactly when the real file exists, on the default backend, today. This is by
  code reading and needs an invariant test. It is a smaller leak than a key
  name (the names are fixed and public), but the capability table's bubblewrap
  column should say it, and no mount trick short of covering the parent folder
  removes it.

- **Inherited descriptors.** A descriptor opened before `unshare` belongs to
  the host's mount, not the read-only one, so `fchmod` on it would succeed.
  Rust's `Command` marks its descriptors close-on-exec; stdin, stdout and
  stderr are what remains. Needs a test that they are pipes, never a file
  outside the workspace.

- **Who this helps.** Where user namespaces work, installing bubblewrap gives
  the same result with no new code. The in-process fallback helps only where
  `bwrap` is absent or unused and user namespaces work. That is: a machine
  without the package, and WSL, where `detect_linux()` skips bubblewrap on
  purpose. It does not help on Ubuntu 23.10 and later with the AppArmor
  restriction on, or (from memory, needs a test) inside a Docker container
  with the default seccomp profile, which refuses `unshare` and `mount`. Those
  stay on Landlock-only.

## Options

### A. seccomp user-notification supervisor

- **Robustness: poor.** The man page says not to build security policy on it.
  A supervisor that never continues a checked call avoids the argument race,
  but then it must emulate. Gap 1 is about twenty calls (`chmod`, `fchmod`,
  `fchmodat`, `fchmodat2`, the four `chown` forms, `utime`, `utimes`,
  `futimesat`, `utimensat`, six xattr set and remove forms, `setxattrat` and
  `removexattrat` on new kernels). The fd forms need the target's descriptor
  fetched with `pidfd_getfd(2)`, and the supervisor must resolve paths as the
  target would: its cwd, root, symlinks, `/proc/self/fd`. Gap 2 is worse:
  every path-taking call (`open`, `openat`, `openat2`, `stat`, `statx`,
  `access`, `readlink`, `execve`, `chdir`, and so on) has to be emulated to
  make "exists but hidden" and "absent" return the same error.
- **Complexity: high**, and it grows with each new syscall. A call the filter
  does not name passes unchecked, so it fails open.
- **Overhead:** two context switches and a read of the target's memory for
  each intercepted call. For gap 2 that is every `open` and `stat` a build
  makes.
- **Failure modes:** supervisor crash or hang blocks or kills the command; a
  missed syscall or a path-resolution difference is a silent bypass.
- Rejected.

### B. ptrace

Same emulation burden and the same race when the tracer lets a checked call
run. One tracer per process, so it conflicts with anything that traces; bwn's
own filter already refuses `ptrace` inside a command. Rejected.

### C. Wait for Landlock

No ABI through 12 has the rights (source). Rejected as a plan. If a metadata
right appears, `handled_access()` picks it up in a few lines.

### D. User-namespace mount tree, built by bwn (recommended)

- **Robustness: good, with the corrections above.** The kernel decides at the
  moment of use, from the mount the path or descriptor is on. There is no
  user-space check to race.
- **Complexity: moderate.** About ten syscalls in a fixed order, and it follows
  what bubblewrap does. The hard parts are ordering and failing closed.
- **Overhead:** about 1 ms a command here (tested).
- **Failure modes:** user namespaces unavailable (probe, fall back to
  Landlock-only, keep the disclosure); a mount step fails midway (the child
  must exit 126, never run the command in a half-built tree); a host with many
  mounts is slower.

## Time-of-check analysis

A supervisor checks a path in user space and the kernel resolves it again
later. Any thread or process sharing the target's memory can change the bytes
in between, and a rename or symlink swap can change what the same bytes mean.

A mount tree has no such window. Whatever the path resolves to, by whatever
spelling, symlink or `..`, it is reached through some mount. That mount is
read-only unless it is a write root. A descriptor carries its mount with it,
which is why `fchmod` on an `O_RDONLY` descriptor returned `EROFS`. A covered
store is not hidden by an answer bwn gives; the files are not in the tree.

What a mount tree does not cover:

- A descriptor opened before the namespace was made (see "Inherited
  descriptors").
- A file inside a write root that is a hard link to a file outside. The
  command can change the shared inode's mode. New links across the boundary
  fail, since the two sides are different mounts. Unchanged from bubblewrap.
- A daemon outside the sandbox that acts on request: Docker's socket, the D-Bus
  session bus, `systemd-run --user`. Bubblewrap removes most by replacing
  `/run`; this backend should do the same.
- Signals. With no PID namespace the command can still signal the user's other
  processes. Unchanged from Landlock-only today. `/proc/<pid>/root` of a host
  process is refused (tested, `EACCES`), so no PID namespace is needed for the
  file guarantees.

## Preconditions, and when the fallback does not apply

bwn must probe and degrade. Each of these disables it:

- `user.max_user_namespaces` set to 0 (some RHEL-family defaults; needs a test).
- `kernel.unprivileged_userns_clone=0`, a Debian and Ubuntu kernel patch. From
  memory Debian has enabled it by default since 11; needs a test.
- Ubuntu's `kernel.apparmor_restrict_unprivileged_userns=1`. Sources: the
  [Ubuntu 23.10 announcement](https://ubuntu.com/blog/ubuntu-23-10-restricted-unprivileged-user-namespaces)
  and the [24.04 release notes](https://discourse.ubuntu.com/t/ubuntu-24-04-lts-noble-numbat-release-notes/39890),
  which name the sysctl. On by default in 24.04.
- A container runtime's seccomp profile that refuses `unshare` or `mount`
  (needs a test).
- bwn itself running in a `chroot`, or already multi-threaded at the call (hence
  the forked child or helper).

A bubblewrap that is installed but fails its probe usually fails for one of
these reasons, so the in-process fallback will usually fail with it. That is
fine: the order becomes bubblewrap, then the built-in mount tree, then
Landlock-only with the two gaps disclosed and auto mode asking, then none.

## Does covering a store break legitimate use?

No more than bubblewrap already does. `git` over SSH, `gh`, `aws` and `npm
publish` already fail in a sandboxed command on every backend; `sandbox_expose`
is the way out. Three differences to check:

- The error changes from `EACCES` to `ENOENT`. `sandbox::hidden_note` should
  still fire on the new message. Needs a test.
- The tmpfs must be read-only, as bubblewrap's is, or a tool will write a fresh
  `known_hosts` into it and lose it.
- A store inside a write root (a session run from the home folder) can be
  covered by a mount. Landlock cannot carve it out (`can_hide` returns false
  today), so this closes a case Landlock-only leaves open.

Inside the namespace, files owned by other users show as owner `nobody`.
Setuid programs already do not gain privilege because of `no_new_privs`.
Whether supplementary groups are still honoured for access after `setgroups`
is denied (expected yes) needs a test.

The network side changes too; see the next section.

## Network isolation

The owner has set five requirements. This backend, and bubblewrap, must meet
them. The same helper that builds the mount tree is where the network is cut
off, so they belong in this design.

### What bwn does today

A command with the network checked (`Net::Proxy`) runs in the host's network
namespace on every Linux backend. Bubblewrap is started with `--share-net`, and
`bwn __confine-net` then applies the same two layers the Landlock backend uses:

- a Landlock rule that allows `connect` on TCP sockets to one port, the
  checker's (`create()` in `landlock.rs`; needs ABI 4, kernel 6.7);
- a seccomp filter (`proxy_filter`) that allows `socket()` only for `AF_INET`
  and `AF_INET6` with `SOCK_STREAM`, and refuses TCP Fast Open.

The checker (`egress.rs`) listens on `127.0.0.1` on a port the kernel picks.
Commands find it through `HTTPS_PROXY` and `HTTP_PROXY`. It resolves the name
itself, checks every address, and connects to the address it checked.

Below ABI 4 there is no network at all for commands. A command may listen
(capability table: "a command can listen"). With the network off, seccomp
refuses every socket call.

This is a filter on a shared network, not a private one. What that means,
from the sources:

- The Landlock rule names a port, not an address. Source, kernel document:
  "`LANDLOCK_ACCESS_NET_CONNECT_TCP`: Connect TCP sockets to the given remote
  port."
- The rule covers TCP only. Source, kernel document, "Erratum 1: TCP socket
  identification": the fix "ensures that only TCP sockets are subject to TCP
  access rights, allowing other protocols to operate without unnecessary
  restrictions", naming SMC, MPTCP and SCTP.
- A listening socket on the host's network is reachable by whatever can reach
  the host.

Two findings about the current limiter came out of testing for this section.
They are reported to the owner directly and are left out of this public note
until they are fixed.

### What a private network namespace gives

Source, [network_namespaces(7)](https://man7.org/linux/man-pages/man7/network_namespaces.7.html):
a network namespace isolates "network devices, IPv4 and IPv6 protocol stacks,
IP routing tables, firewall rules, [...] port numbers (sockets), and so on. In
addition, network namespaces isolate the UNIX domain abstract socket
namespace".

Tested on 6.18.44 as uid 1000, after
`unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET)` with an identity id map,
the loopback interface brought up before `execve`, and probes run after it:

| Attempt from inside | Result |
|---|---|
| TCP connect to a listener on the host's `127.0.0.1` | `ECONNREFUSED` |
| TCP connect to `1.1.1.1:443` | `ENETUNREACH` |
| UDP `sendto` `8.8.8.8:53` | `ENETUNREACH` |
| Raw ICMP socket | `EPERM` |
| `AF_PACKET` socket | `EPERM` |
| Netlink `RTM_NEWLINK` (create an interface) | `EPERM` |
| Connect to an abstract Unix socket a host process listens on | `ECONNREFUSED` |
| Connect to a **pathname** Unix socket a host process listens on, through the read-only tree | **connects** |
| Listen on `0.0.0.0` inside; host connects to that port | listen works; host gets `ECONNREFUSED` |

Not tested: IPv6 (this machine has none; the expectation is `::1` only), and
`setns` into a host process's network namespace (expected `EPERM`).

The one row that connects is the important one. A pathname Unix socket lives
in the file tree, not the network namespace. That is the way to reach the
checker from inside. It is also a way out if the tree shows other sockets:
Docker's, the D-Bus session bus, systemd-resolved's, X11's, the SSH agent's.

### The design

One helper process per sandbox does all of this. For bubblewrap it is
`__confine-net`, which already exists; for the built-in backend it is the new
`__confine-ns`.

1. **Private namespace.** `CLONE_NEWNET` is added to the `unshare` call, and
   bubblewrap is run without `--share-net` (its `--unshare-all` then includes
   `--unshare-net`; source,
   [bwrap(1)](https://github.com/containers/bubblewrap/blob/main/bwrap.xml)).
   The helper brings `lo` up while it still holds capabilities. Whether
   bubblewrap already brings `lo` up needs a test.

2. **The checker listens on a Unix socket too.** `egress::start()` binds a
   second listener at a path in a folder only the user can enter, outside any
   write root. The mount plan binds that one socket file into the sandbox,
   read-only. Connecting through a read-only mount works (tested); it needs
   write permission on the socket file, which the user has (source,
   [unix(7)](https://man7.org/linux/man-pages/man7/unix.7.html)).

3. **A forwarder inside.** Tools speak to an HTTP proxy at a TCP address, not
   a Unix socket. So the helper listens on `127.0.0.1:<port>` inside the
   namespace, forks, and the child becomes the command. The parent stays as
   the forwarder: for each connection it accepts, it connects to the Unix
   socket and copies bytes both ways. It exits with the command's status.
   `HTTPS_PROXY=http://127.0.0.1:<port>` is unchanged, so npm, cargo, pip, git
   and curl need nothing new.

   The forwarder is not the enforcement point. If a command took it over, it
   would hold one thing: a connection to the checker, which the command is
   allowed anyway. The namespace stops direct traffic and the checker, outside
   the sandbox, decides. The forwarder should still be confined: no file
   writes, and a Landlock domain the command is not inside, so the command
   cannot trace it (source, kernel document, "Ptrace restrictions").

   Passing descriptors instead of binding a path would also work, but each
   proxied connection needs a new connection to the checker, so a path that
   can be connected to repeatedly is simpler than a channel that hands out
   descriptors. The end-to-end forwarder has not been built: needs a prototype.

4. **Other sockets in the tree.** The command's seccomp filter keeps refusing
   `AF_UNIX` when the network is checked, as `proxy_filter` does now, and the
   filter is tightened to accept only protocol 0 or `IPPROTO_TCP` on stream
   sockets. The mount plan replaces `/run` and `/tmp` as bubblewrap's does.
   Landlock ABI 9's `FS_RESOLVE_UNIX` is a third layer where the kernel has it.
   Refusing `AF_UNIX` also refuses tools that use Unix sockets among
   themselves; that is the case today and can be relaxed later.

5. **DNS.** There is none inside, by design. A resolver reachable from the
   sandbox is a way to send data out, one query at a time. Tools that use the
   proxy send it the host name, and the checker resolves it outside and
   connects to the address it checked, as it does now. A tool that resolves
   names itself fails. To make it fail at once instead of timing out, bind an
   `/etc/resolv.conf` that names `127.0.0.1`, where nothing listens. Needs a
   test with glibc and musl.

6. **Incoming connections.** Nothing outside can connect in (tested). A dev
   server a command starts is reachable only inside that namespace. This is a
   behaviour change; see the owner questions.

7. **Loopback at the checker.** `egress::check` allows loopback on any port so
   a command can reach "this machine's own servers". With a private namespace
   a command reaches its own servers directly, and loopback at the checker
   means the host's services. To meet requirement 1 the checker should refuse
   loopback unless `network.allow` names it. `NO_PROXY` must cover `localhost`
   and `127.0.0.1` so in-sandbox traffic does not go to the checker; whether
   `proxy_env` sets it was not checked.

With this, `can_check_egress` no longer depends on Landlock ABI 4 for these
two backends, so Ubuntu 22.04 and Debian 12 get a checked network instead of
none. `Net::Off` is the same namespace with no forwarder. `Net::Open` creates
no network namespace; the user asked for that.

The Landlock-only backend cannot do any of this: it has no namespace. It keeps
the port rule and the filter, and it does not meet requirement 1. The
capability table has to say so, or the network has to be off by default there.

### The five requirements

| Requirement | Bubblewrap and the built-in mount backend, after this | Landlock-only |
|---|---|---|
| 1. Private network: no direct route to the internet, the local network or host services | Private namespace, loopback only. Host services only through the checker, and only if `network.allow` names them | Not met. Shared namespace, filtered |
| 2. All authorized traffic through a trusted service outside | The checker, reached over a bound Unix socket by the forwarder | The checker, over host loopback |
| 3. Default deny for destinations, DNS and incoming | Checker's rules; no resolver inside; nothing can connect in | Destinations by the checker; DNS refused by the filter; incoming not blocked |
| 4. One policy for commands, MCP servers, plugins, hooks and fetch | See below | See below |
| 5. Escape attempts in the release suite | See the list below | Same list; expected failures recorded in the capability table |

Requirement 4 is roadmap 2.3 and 2.4, which are open. Today `sandbox::wrap`
is called for commands (`tools.rs`); the fetch tools apply their own address
checks (`net_guard`) and do not call `egress::check`; hooks build their own
`Command` (`hooks.rs`). This was read from call sites, not audited. The design
that fits this note:

- One function starts every child process: commands, stdio MCP servers, hooks,
  plugin helpers. It takes the session's policy object (2.3) and returns the
  process inside the helper. A long-lived MCP server gets a helper that lives
  as long as it does.
- Everything bwn connects to itself (fetch tools, HTTP MCP servers) goes
  through the same decision function as the checker, `egress::check`, so a host
  a command may not reach is one fetch may not reach either.
- A widening is per entry and shown in the startup check (2.4).

### Escape tests for the release suite

Each test runs inside the sandbox, on every backend the machine can run
(`BWN_TEST_SANDBOX_BACKEND`), with a listener on the host that fails the test
if anything arrives. A backend that cannot pass one says so in the capability
table, and the table's own check ties the row to the test.

- Direct TCP connect: a public address, the default gateway, a private-range
  address, the host's loopback listener.
- The checker's port number on an address that is not the checker.
- UDP `sendto` port 53; `getaddrinfo` of a name not in the rules; a lookup
  through systemd-resolved's and nscd's Unix sockets.
- Raw sockets: `SOCK_RAW` ICMP, the unprivileged ping socket
  (`SOCK_DGRAM`, `IPPROTO_ICMP`), `AF_PACKET`.
- IPv6: `::1` on a host listener's port, a public address, a link-local
  address.
- Stream sockets that are not TCP: MPTCP, SCTP, SMC.
- Unix sockets: an abstract name a host process listens on; pathname sockets
  under `/run` and `/tmp` (Docker, D-Bus, X11, the SSH agent).
- Netlink: dump routes, create an interface, move one between namespaces.
- Incoming: the command listens on `0.0.0.0`; the host tries to connect.
- Around the filter: TCP Fast Open (`sendto` with `MSG_FASTOPEN`), `io_uring`
  connect, an x32 socket call, `socketcall` on 32-bit.
- Around the namespace: a nested user and network namespace; `setns` to
  `/proc/<host pid>/ns/net`; a descriptor inherited from bwn.
- Against the checker: `CONNECT` to a host not in the rules, to an address
  literal, to a name that resolves to a private or metadata address, to a name
  whose address changes between lookups, to port 22; a request line for an
  allowed host with a different `Host` header; a request with the proxy
  variables unset.
- Against the forwarder: signal it, trace it, open `/proc/<its pid>/mem`.

## Recommendation

Build option D with the corrections. Do not build a supervisor.

The same helper carries the private network namespace and the forwarder the
network requirements need, on both bubblewrap and the built-in backend.

The stronger argument is that one mechanism closes most of what roadmap 2.2
still lists for Landlock, not only these two gaps: file metadata, `stat`
existence, read-only `.git` and policy files, a private `/tmp`, the agent
socket, and a store inside the workspace. It also removes the cost M5 paid:
with the store covered by a mount, Landlock no longer has to refuse listing
`/`, `/home` and the home folder.

### Implementation sketch

`harness/src/sandbox.rs`:

- Add `Backend::Userns` beside `Backend::Landlock` (enum at line 51, label at
  63). Treat it like `Bwrap` in `net_for` and the posture functions around
  lines 265 to 272 and 1030 to 1081.
- `detect_linux()` (line 414): after the bubblewrap probe fails or is skipped,
  try `landlock::userns_available()`, then `landlock::available()`.
  `detect()` accepts `BWN_TEST_SANDBOX_BACKEND=userns` so the invariant suite
  runs once on this backend too.
- Lift the mount list out of `bwrap_argv` (lines 501 to 563: read-only root,
  read-write workspace and add-dirs, read-only protected entries, covers for
  hidden paths) into a plain `MountPlan`. `bwrap_argv` renders it as arguments;
  the new backend executes it. One plan, so the two cannot drift.
- The run arm at line 937 gets a `Backend::Userns` case next to
  `Backend::Landlock`.

`harness/src/landlock.rs`:

- `command()` (line 381) gains the plan. The child does, in this order, and
  exits 126 on any failure:
  1. `unshare(CLONE_NEWUSER | CLONE_NEWNS)`, with `CLONE_NEWNET` unless the
     network is open. Bring `lo` up.
  2. Write `deny` to `/proc/self/setgroups`, then `uid uid 1` and `gid gid 1`
     to the id maps. Not root.
  3. `mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)`.
  4. Bind each write root over itself with `MS_BIND | MS_REC`.
  5. Mount a tmpfs over each hidden folder, bind `/dev/null` over each hidden
     file, bind each protected entry (`.git`, `.buildwithnexus`) over itself.
  6. `mount_setattr("/", AT_RECURSIVE, MOUNT_ATTR_RDONLY)`, clear it on the
     write roots, set it again on the covers and protected entries inside them.
  7. `chdir` to the workspace again.
  8. With the network checked: listen on `127.0.0.1:<port>`, fork; the parent
     becomes the forwarder to the checker's Unix socket.
  9. `engage()` (line 368), unchanged: `no_new_privs`, the Landlock ruleset
     (write rights; no hidden list needed), then seccomp.
  10. `execve`.
- `seccomp()` (line 503) adds the seven new mount syscalls to `deny`, for
  every backend, and `proxy_filter()` (line 585) checks the protocol argument
  of `socket()`.
- Prefer a helper, `bwn __confine-ns`, dispatched in `lib.rs` beside
  `__confine-net`, over doing steps 1 to 7 in `pre_exec`. The steps open and
  write files and may allocate, which is unsafe in a forked child of a threaded
  process. `engage()` alone is why `pre_exec` is acceptable today.

`harness/src/egress.rs`: `start()` (line 272) also binds the Unix socket;
`check` refuses loopback unless a rule allows it.

`harness/src/capabilities.rs`: a column or note for the new backend, with
tests named, as the table's own check requires.

Tests to write first, on this backend: the two existing gap tests inverted
(`chmod` outside refused, including through a descriptor; `stat` of a hidden
name returns `ENOENT`); a submount stays read-only; the command cannot unmount
a cover or call `open_tree`; a nested user namespace cannot either; relative
writes in the workspace work; `/tmp` is writable.

## Questions for the owner

1. Why does `detect_linux()` skip bubblewrap inside WSL? No commit or comment
   records the reason. WSL is where the Landlock backend is used most, so it is
   where this work pays off, and whatever broke bubblewrap there may break this
   too. A WSL cell has to run the probe before the work is scheduled.
2. Is the fallback worth its code where `apt install bubblewrap` gives the same
   result? It is if "works with nothing installed" is a product requirement.
   If not, a clearer "install bubblewrap" message and leaving Landlock-only as
   it is would be cheaper.

3. A private network namespace means a dev server started by one command is
   not reachable from the next command or from the user's browser. Two ways
   to keep that working: one namespace per session that every command joins,
   and an explicit, shown setting that publishes a port to the host. Which, or
   neither?
4. Landlock-only cannot give a private network. Should the network be off by
   default on that backend, or stay filtered with the capability table saying
   requirement 1 is not met?

## Residual risk if nothing is built

The exposure is bounded and already stated. It applies only to the Landlock
backend. There, auto mode asks before every command, so neither a metadata
change nor a name probe happens without the user approving the command that
does it, and the capability table and startup check name both limits. A
command that does run can learn whether `~/.ssh/id_ed25519` exists and can
change the mode, owner, times or xattrs of files the user owns. It cannot read
a hidden file. The metadata gap is the one with consequences: `chmod 000` on
files the user needs is a nuisance, and `chmod 644` on a private key makes it
readable by other local users and makes `ssh` refuse to use it. Neither gives
the command the key. Doing nothing leaves the fallback weaker than the default
and says so; it does not leave an undisclosed hole.

## Appendix: how the tests were run

Two small C programs, not committed. The first calls
`unshare(CLONE_NEWUSER | CLONE_NEWNS)`, writes the id maps (identity or root),
sets `/` recursively private, binds a workspace over itself, mounts a
read-only tmpfs over a stand-in `.ssh`, sets the tree read-only with
`mount_setattr` (or, in one variant, a non-recursive bind remount), clears
read-only on the workspace, changes directory, optionally applies a Landlock
ruleset that handles `WRITE_FILE`, `MAKE_REG` and `REMOVE_FILE` and allows the
workspace, `/dev` and `/tmp`, and then executes the second. The second makes
each call in the results table and prints the error. Both ran as uid 1000 with
no capabilities, on Linux 6.18.44 x86-64. A third program made the network probes after
`unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET)`, with a host process
listening on TCP, a pathname Unix socket and an abstract Unix socket. Nothing
was run on a kernel below
6.18, on Ubuntu 24.04 with AppArmor, on WSL, or in a Docker container; the
claims about those are marked as needing a test.
