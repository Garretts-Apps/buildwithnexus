# Roadmap

This is the implementation plan for bwn. It sets the order of work, the
dependencies between items, the release milestones and the acceptance
criteria for each. It is one of three documents:

| Document | Role | Changes |
|---|---|---|
| [`docs/mandate.md`](mandate.md) | The product strategy. Governs architectural decisions, feature proposals and release criteria. | Rarely, and only by an explicit, documented decision. |
| [`docs/competitor-analysis.md`](competitor-analysis.md) | The dated evidence: where bwn and the field stand against the mandate. | Refreshed when the field or bwn changes materially. |
| `docs/roadmap.md` (this file) | The plan that follows from the evidence. | As work lands and evidence changes. |

Built from the 27 November 2026 analysis. Item IDs (M1–M13, A1, V3, SO-2,
CP-1, SD-1 and so on) are the analysis's; look them up there for the code
references and the evidence.

## Objective

Make bwn's existing security guarantees true, testable and consistent first.
Build feature parity and differentiation on that base, never around it.

## How this roadmap is governed

- **The mandate governs.** A proposal that departs materially from
  `docs/mandate.md` needs a decision record, committed beside this file and
  linked from the item, before work starts. A departure without one is not
  accepted, whatever its benefit.
- **Evidence is labelled.** Every finding an item closes is marked
  *Reproduced*, *Code inspection* or *Untested*. A finding that is not
  *Reproduced* is reproduced in a matching field cell (docker or
  `field-adhoc`) before it is fixed. If it does not reproduce, the item is
  rewritten to match what does.
- **Every security change is traceable.** Its PR and commit name the
  analysis finding IDs it closes (for example M12, A1), the invariant it
  restores (Authority, Integrity, Verification, Inheritance or
  Accountability), the roadmap item, and the acceptance tests that prove it.
  Tests carry the same IDs in a comment. A fix with no finding ID either
  adds one to the analysis or explains why it is outside it.
- **Standing release requirements.** Every release meets all three:
  1. Every security fix includes an adversarial regression test that fails
     against the code before the fix.
  2. Security documentation describes shipped, tested behaviour, including
     each platform's and backend's limitations. Nothing is documented ahead
     of the code.
  3. A roadmap item is marked complete only when its acceptance criteria
     pass in CI.
- **Acceptance criteria are tests.** Each item's "done" is a set of named
  tests that pass, not a description. The test IDs (AU-1, IN-2, VE-3 and so
  on) are defined in section 5 of the analysis.
- **Levels are claimed per surface.** A milestone may say a control is
  Level 3 or 4 only for the execution surfaces and backends its tests cover.
- **The invariant suite is a release gate from Milestone 1.** It starts with
  the first fixes, not after them. Each milestone adds the cases for what it
  closes. A failing case blocks the release workflow. Every new feature,
  including background agents, plugins, memory, browser automation and model
  integrations, adds its own surface to every case before it ships.
- **Fail-open conditions are security defects,** not feature requests. They
  are fixed in the milestone that owns the surface and are not traded off
  against features.

## Decisions taken before Milestone 1

| ID | Decision | Effect |
|---|---|---|
| D1 | **When confinement is unavailable, bwn fails closed.** Operations that depend on the sandbox are refused unless an authorised human explicitly approves an exception. The exception is per session, shown in the startup check and part of its fingerprint. Recorded in `docs/mandate.md`. | Default `auto` with no working backend stops running commands unconfined. `--sandbox off` and an answered prompt remain the human's exception path. Windows needs D4 to stay usable. |
| D2 | **The invariant suite starts with the first security fixes.** | Milestone 1 ships its own cases. |
| D3 | **Tamper evidence needs a protected trust anchor.** A hash chain stored where an unrestricted process can rewrite it proves nothing, because the whole chain can be rewritten. | The anchor is chosen in Milestone 2, on evidence, within the local-first constraint. Candidates: the chain head in commit trailers, so pushed history witnesses it; an OS-protected store outside the agent's reach; an opt-in external witness. |
| D4 | **Windows states its reduced guarantees until a native backend exists.** It also offers a restricted execution mode. | The startup check, `doctor` and SECURITY.md say commands are not confined on Windows. Restricted mode is D1's exception path for Windows: every command asks, even in `auto`, and auto mode never runs one unasked. |

## Milestones

Each milestone closes with a minor release. A fix for a *Reproduced*
critical finding ships as a patch release as soon as it passes, without
waiting for its milestone.

Benchmark and invariant work runs through every milestone. Milestone 6 is
where the results are published, not where the work starts.

### Milestone 1: security stabilization

**Goal:** close the configuration vulnerabilities, protect policy files,
remove the known fail-open paths and make the security documentation
truthful.

| # | Item | Closes | Evidence | Acceptance |
|---|---|---|---|---|
| 1.1 | A missing user `sandbox` value is compared as `auto`, not `off`. Ships first, as a patch. Complete in #171 (AU-1 for `sandbox` passes, for omitted and explicit user settings), which also fixed `doctor` showing the startup check before reading settings. This closes M12 and breach A1 only: the Authority invariant stays partially enforced until 1.2 (AU-1 for every loosening key), A2 (Landlock `.git` and policy files; 1.8, 2.2), A3 (no backend; 1.6, 2.3) and A4 (1.9) are closed. | A1, M12 | Reproduced | AU-1 for `sandbox`; `invariant_au1_untrusted_project_keys_only_tighten` extended to a base that names nothing |
| 1.2 | Every loosening project key is checked against the effective default, not a missing value. Complete in #173: a table-driven AU-1 suite (`config_authority_tests.rs`) covers every settings key and project hooks, with omitted, default, explicit and malformed user values against loosening, tightening and malformed project values, trusted and untrusted. It found and fixed one more gap: an untrusted project could repair a user value that failed to load. | A1, AU-1 class | Reproduced by the suite | AU-1 for every loosening key: an untrusted project never expands effective authority beyond the user baseline |
| 1.3 | A bad `sandbox` value falls back to the strictest mode; an unparseable settings file refuses to start or applies the strictest policy, and its deny rules are never dropped. Complete in #175: a bad `sandbox` value runs as `require` and a bad `permission` value as `readonly`; a settings file that exists but doesn't load (user or project, including unreadable) stops bwn from starting; mid-session, the gate, the egress checker and http hooks refuse until it loads. | M13, I4 | Reproduced (debian:12 cell) | IN-2 |
| 1.4 | Project policy files (`.buildwithnexus/settings.json`, rules, agents, skills, commands, tools) are a protected class for file tools. A change needs a human answer through the trust prompt. Complete in #177: file-tool changes to a project's policy files ask a person in every mode, one change at a time; no saved approval or allow rule covers them, and spelling, `..` and symlinks are resolved. Commands under Landlock or with no backend remain for 2.2. | I1 | Code inspection | AU-2; IN-1 for the file-tool surface |
| 1.5 | The startup check fingerprints the effective policy (exposed stores, allowed hosts, added folders, sandbox mode, permission mode, backend capabilities). It asks again within a session after `/sandbox`, `/permissions` or `/add-dir`. Complete in #178: row keys carry the added folders, hidden stores and allowed hosts, and an accepted session is asked again when what it can reach changes. | M11 | Code inspection | IN-3 |
| 1.6 | D1 in code. No backend in `auto` means confined tools are refused unless the user grants the exception. A missing bubblewrap network limiter means no network. No resolvable home folder is reported and treated as a missing guarantee. Also: with `require` and no backend (Windows), the startup-check row says "commands run unsandboxed" though `require` refuses them; it should say commands are refused (noted in #175). Complete in #180 and #181: no limiter means no network; no home folder hides nothing; with sandbox auto and no backend, commands are refused until a person allows them for the session. | Fail-open list; M8(b) | Code inspection | A no-backend cell refuses `run_command` without an exception; a limiter-missing cell has no network |
| 1.7 | Landlock asks only for the rights its ABI supports, and ABI 1 is treated as not confining truncation. Complete in #182: on 6.1 (ABI 2) every Landlock ruleset failed with "Invalid argument" because bwn asked for the truncate right a release early; it now asks only for what the ABI has. Below ABI 3 truncation outside the workspace is not confined, and the startup check and SECURITY.md say so. | M2, M3 | Reproduced (Ubuntu 22.04 on 5.15 and Debian 12 on 6.1, in VMs) | Field cells on 5.15 and 6.1 run a confined command and refuse a truncate outside |
| 1.8 | The Landlock `.git` and policy-file gap is shown in the startup check, and auto mode asks before commands on that backend. The full fix is in Milestone 2. Complete in #179. | M1 (disclosure) | Code inspection | Startup-check row test per backend |
| 1.9 | A blocked git write no longer tells the model to use `--sandbox off`. Complete in #177. | A4 | Code inspection | Message test |
| 1.10 | SECURITY.md and the README carry a per-backend guarantee table, with every exception the analysis found (M1, M4–M10). Hand-written here; generated or tested in Milestone 2. Complete in #181: a per-backend table in SECURITY.md, each row citing its test or its roadmap item. | Documentation mismatches | Code inspection | Every row cites the test that proves it, or says "not guaranteed" |
| 1.11 | D4, first part: Windows states its reduced guarantees in the startup check, `doctor` and SECURITY.md. Complete in #179. | Windows L0, disclosure | Code inspection | Windows CI asserts the row |
| 1.12 | Invariant suite, first cases, wired as a release gate: AU-1, AU-2, IN-1 (file tools), IN-2, IN-3. Complete in #179: the `invariant_*` tests run per backend (bubblewrap, Landlock, none, Seatbelt, Windows) in a CI job that release.yml calls before publishing. | D2 | n/a | The release workflow fails when a case fails |
| 1.13 | Benchmark groundwork: the invariant suite reported as a score per surface and backend (analysis 9.3), and the repository and task selection for the 20-task benchmark. Complete in #181: `bench/invariants.py` (baseline committed) and `bench/longitudinal/select_tasks.py`. | SD-2 start | n/a | A stored baseline under `bench/results/` |

**Dependencies:** 1.1 first. 1.2 builds on 1.1. 1.12 lands with 1.1–1.5,
not after them.

**Exit criteria:** every case in 1.12 passes on bubblewrap, Landlock,
Seatbelt and no backend. The reproductions for M2, M3, M5 and M8(c) are
recorded, and whatever they turn up is planned. No fail-open condition from
the analysis remains undisclosed.

Recorded: M2 and M3 in Ubuntu 22.04 (5.15) and Debian 12 (6.1) VMs (#182:
every ruleset failed with "Invalid argument" on 6.1; truncation outside is
not confined below ABI 3, now disclosed). M5 on 6.18 (#183: `ls ~/.ssh`
listed the key names). M8(c) on 6.18 without x32 enabled: x32 `getpid` and
`socket` calls passed the seccomp filter and reached the kernel (ENOSYS),
so on a kernel with x32 enabled they would run; fixed in 2.2. The V3 and V6
fail-opens (a failed or refused check round) are disclosed in SECURITY.md
and closed by 3.1.

### Milestone 2: enforcement foundation

**Goal:** one effective security policy, conformance tested on every
backend and kernel, with extensions inside it and security decisions on
record.

| # | Item | Closes | Acceptance |
|---|---|---|---|
| 2.1 | SO-2: a capability table in code (backend and ABI against each guarantee); SECURITY.md generated from it or tested against it; a conformance suite running the same probes in field cells on kernels 5.15, 6.1, 6.8 and current, WSL and macOS. In progress: `harness/src/capabilities.rs` holds the table; `security_md_table_matches_code` renders SECURITY.md's guarantee table from it, and `every_cited_test_exists_and_is_listed` checks each cited test exists (it caught one citation left stale when a refactor dropped the test it named; the three tests were restored). `every_claimed_cell_cites_a_test_of_its_backend` makes each cell that claims a guarantee cite a test that runs on its own backend. The per-syscall write-path probe runs in-process (`invariant_landlock_confines_write_syscall_paths_outside`), and `scripts/field/landlock-kernel-cells/` reproduces the Landlock + `invariant_m2`/`m5`/`m8c` tests in QEMU cells for kernel 5.15 (ABI 1) and 6.1 (ABI 2) from pinned cloud images, and for 6.8 (ABI 4, Ubuntu 24.04), where an unprivileged user also probes AppArmor's user-namespace restriction. Remaining: wire the cells and WSL/macOS into CI (a cost decision — a TCG run is ~10 min per cell). | M1, M4–M9 | Probes: write outside, truncate outside, write `.git/hooks`, create `.git`, read and list `~/.ssh`, connect to the agent socket, unlisted host, UDP, inbound connection, x32 |
| 2.2 | Close the backend gaps the suite finds. In progress: listing hidden folders under Landlock (M5) is closed, at the cost of listing the folders a store sits in; x32 syscalls (M8c) are refused by the seccomp filter on x86-64; below Landlock ABI 3, seccomp refuses `truncate(2)` and a read-only `O_TRUNC` open (M2). The two Landlock gaps (a file's metadata outside the workspace is writable; `stat` or `open` of a guessed name in a hidden store confirms it exists) are closed by bwn's namespace backend, used where bubblewrap is missing and unprivileged user namespaces work: the tree is read-only, the workspace, added folders, `/tmp` and `/dev` are read-write, `.git` and `.buildwithnexus` stay read-only and each store folder is covered by an empty tmpfs, with Landlock and seccomp inside and every capability dropped (design note `docs/design/landlock-metadata-and-name-hiding.md`; decided by the owner). SEC-1 (an MPTCP socket bypassed the checker) is fixed. Decided for the network (owner): one private network namespace per session reached through the checker, and no network by default on Landlock alone. Landlock alone now gives commands no network until the user allows it for the session (`invariant_landlock_alone_has_no_network_until_allowed`). On the namespace backend and bubblewrap every command joins the session's private network namespace (bubblewrap is started in it: `invariant_bwrap_netns_*`) (`harness/src/netns.rs`): only loopback and the checker, on any ABI, with escape tests (`invariant_netns_*`) for the checker's port on another address, host loopback, IPv6, UDP, DNS, raw, netlink, packet and Unix sockets, joining the host's namespace, reading the holder, the holder dying, and bwn's own tools reaching session servers while provider clients don't. Ubuntu 24.04 with AppArmor's default restriction allows no unprivileged namespace at all (field cell `noble`), so there bwn runs on Landlock alone. Complete when this row's PR passes CI. What no backend can close is disclosed: the capability table says "not guaranteed" and the startup check shows it. On Landlock alone that covers the agent socket with the checker off, `.git` and policy files (auto mode asks), file metadata, `stat` existence and inbound listening; each is closed where the namespace backend runs. Everywhere it covers a hidden file's existence (M-1) and a store that contains the workspace, where the startup check says readable and auto mode asks (`invariant_a_store_holding_the_workspace_is_never_claimed_hidden`; this was claimed hidden on every backend before). It also covers nested `.git` past the scan's bounds, where the startup check says so (auto mode does not ask: a Python `.venv` alone can pass the bounds; `invariant_nested_git_past_the_bounds_is_disclosed`), and a setuid bubblewrap where bwn can't make the namespace, which keeps the port-held checker; the startup check says commands listen on this machine's network there. Bubblewrap in the private namespace accepts no inbound connection (`invariant_bwrap_netns_listener_is_not_reachable_from_the_host`). The IPv6 `localhost` test fails instead of skipping in the forced-namespace CI cell, and that cell passed on #193, so it ran there. | M1, M4–M9 | 2.1 probes pass, or the capability table says "not guaranteed" and the startup check shows it |
| 2.3 | SO-5: one policy object per session for commands, file tools, fetch, MCP, hooks and helpers. Complete when this row's PR passes CI. `harness/src/policy.rs` holds every field that limits a surface: permission, sandbox mode, network, egress check, the Landlock-network and unconfined allowances, added folders, exposed stores, user-named hosts, the untrusted mark and read-only. `Policy::child` is the one derivation helpers use; background workflows get the policy as `--inherit-policy` and run with `meet(own, inherited)`, which closes H5 for them: they got only the permission and added folders before, and their model-written task could name hosts the checker then allowed. Tests: `invariant_ih2_every_child_policy_is_within_its_parent` (property, 5,000 random cases), `within_refuses_each_looser_field`, `invariant_ih3_a_workflow_inherits_the_untrusted_mark`, `invariant_ih2_an_inherited_policy_only_narrows`, `invariant_ih2_an_unreadable_inherited_policy_is_the_strictest`, `invariant_ih2_a_frozen_session_takes_no_hosts_from_text`. Not claimed: A3 (no backend) is unchanged by a policy object; MCP servers and hooks under the sandbox are 2.4. | A3, H5 | IH-2 property test |
| 2.4 | Stdio MCP servers and hooks start inside the sandbox by default. Any widening is per entry and shown in the startup check. File-tool reads follow the same hidden-store policy as commands. | H1, H2, H3 | IH-1 for MCP stdio, hooks and file tools |
| 2.5 | SO-7: a decision log of every gate decision, approval, exception, startup-check acceptance and verification result, with the D3 trust anchor chosen and implemented. Trust, startup-check and rules stores get an integrity check. | C1, C2, C3, I3 | AC-1, including detection of a rewritten chain |
| 2.6 | D4, second part: Windows restricted execution mode. | Windows | A Windows CI cell asks before every command in `auto` |
| 2.7 | Invariant suite: AU-3, AU-4, IN-1 (all surfaces), IH-1, IH-2, IH-3, AC-1. | | Release gate |

**Dependencies:** needs Milestone 1. 2.1 before 2.2. 2.3 before 2.4.
2.5 needs D3 resolved.

**Exit criteria:** the guarantee table is generated or tested, never
hand-asserted. Every surface in the analysis's table 3.3 is L3 or states
why not. The decision log survives an adversarial rewrite test.

### Milestone 3: engineering assurance

**Goal:** verification the model cannot pass, skip or outlast; external
scanners as mandatory gates; git that works inside the sandbox; and
evidence attached to changes.

| # | Item | Closes | Acceptance |
|---|---|---|---|
| 3.1 | SO-3: mandatory checks come from project policy, not the model's `command`. Changes are detected from the working tree, not from tool names. A failed mandatory check is a failure in every interface. Helpers' changes count. | V1, V2, V3, V5, V6, V7 | VE-1, VE-2, VE-3, VE-5 |
| 3.2 | Verification facts come from checks, not tool names; security requirements are satisfiable only by a real check. | V4 | VE-4 |
| 3.3 | SO-6: `add`, `commit`, `fetch`, `pull`, `rebase` and `stash` work in the sandbox. Hooks and config stay byte-identical, and credentials go through a broker. | A2, A4, the commit half of V3 | AU-5 |
| 3.4 | The assurance tool contract (analysis 7.4): declarative checks, SARIF 2.1.0 or JSON results, exit-code gates, run through the sandbox. The model cannot add, remove or reorder entries. | | Contract tests with stub scanners |
| 3.5 | SD-1, first slice: secrets (gitleaks) on each edit and dependencies (osv-scanner or the ecosystem audit) on manifest changes, as blocking gates. A required scanner that is missing blocks. | | A seeded secret and a seeded vulnerable dependency each block completion |
| 3.6 | SD-1, second slice: SAST (semgrep) on changed files before finish; configuration scanning for Docker, Terraform, Kubernetes and CI files. | | Seeded findings block |
| 3.7 | SD-3: an evidence record per completed change, referenced from a commit trailer, checked by `bwn evidence verify <commit>`. SBOM regenerated by a real tool when dependencies change. | C4 | AC-2 |
| 3.8 | Commits wait for verification. Per-edit auto-commits are replaced or held until the change passes. | V3 | VE-3, no commit made for a failing change |
| 3.9 | Invariant suite: VE-1 to VE-5, AU-5, AC-2. | | Release gate |

**Dependencies:** needs Milestone 2's policy object and decision log. 3.1
before 3.5, because a gate means nothing while the model can satisfy it.
3.4 before 3.5. 2.5 before 3.7. 3.3 before 3.8.

**Exit criteria:** software security and quality gates are L3 in the
session for the surfaces covered. The SusVibes one-shot measurement (6.2)
runs with gates off and on.

### Milestone 4: professional developer experience

**Goal:** close the parity gaps developers expect, inside the enforcement
layer. Every item inherits the session policy and passes the invariant
suite before release.

| # | Item | Acceptance |
|---|---|---|
| 4.1 | A parity audit against the backlog below, scoring each row against the current releases of the leading harnesses. It turns *To verify* rows into shipped, partial or gap. | Backlog table updated with sources |
| 4.2 | CP-1: editor, ACP and headless parity. One suite of gate decisions gives the same answers through the terminal, ACP and `--json`. The question tool and the startup check are answerable in all three. | Cross-interface decision suite |
| 4.3 | CP-2: per-artifact verification. A project file maps artifact types to checks, the harness picks them by changed paths, and the verdicts feed 3.1. | Each artifact type has a seeded failure that blocks |
| 4.4 | CP-3: Bedrock, Vertex and Azure credential flows, signed in-tree (no vendor SDKs), with keys bound to their endpoints. Subscription sign-in only after the providers' terms for third-party clients are read. | Credentials never reach commands (IH-1); the endpoint is shown in the startup check |
| 4.5 | Context management: compaction controls and policy (thresholds, what is kept, untrusted labels kept through compaction). | The untrusted mark survives compaction and resume |
| 4.6 | Backlog items the audit ranks highest, each as its own item with tests. | Per item |

**Dependencies:** needs Milestone 3 for 4.3. 4.2 can start after
Milestone 2.

#### Parity backlog

"Today" is from the README and the analysis; *To verify* rows are confirmed
in 4.1.

| Capability | Today | Note |
|---|---|---|
| Manual compaction | Shipped (`/compact`) | |
| Compaction controls (thresholds, retention rules, per-session policy) | To verify | Untrusted labels must survive it (4.5) |
| Model routing (a different model per role: helpers, compaction, review, planning) | To verify | Each route's endpoint shown in the startup check |
| Session branching | Partial: `/fork`, `/rewind` | A browsable session tree is the gap |
| Checkpoints and undo | Shipped | Shell changes are not checkpointed |
| Plugins: skills, hooks, MCP servers and agents installed as one unit | To verify | Local and trust-gated; a marketplace is deprioritized by the mandate |
| Images | Shipped (paste, `read_file`) | |
| Other multimodal input (PDFs, audio, video frames) | To verify | Video through ffmpeg exists for attachments |
| Browser automation beyond `screenshot_url` | Partial | Needs the fetch and browser surface in the invariant suite |
| Editor integration | Partial (ACP; the question tool gets no answer) | 4.2 |
| Bedrock, Vertex, Azure; subscription sign-in | Gap | 4.4 |
| Cross-session memory | Partial (`/memory`, AGENTS.md) | Milestone 5: treated as untrusted content |
| Background agents | Partial (timed workflows) | Milestone 5 |
| Reasoning effort control | Shipped (`/effort`) | |
| Headless JSON contract | Shipped | |

### Milestone 5: secure autonomous engineering

**Goal:** autonomy and learning that stay inside the policy and leave
evidence.

| # | Item | Depends on | Acceptance |
|---|---|---|---|
| 5.1 | SO-8: a source label on every tool result that survives compaction and resume. Repository files and command output are covered. | 4.5 | A seeded set of planted instructions leads to no unasked command, fetch, MCP write, or edit of CI, hook or policy files in `auto` |
| 5.2 | CP-4: background agents as children of the session (no daemon), each in its own worktree and sandbox. A background agent cannot answer its own approvals. | 2.3, 3.1, 3.7 | IH-2, VE-5, an evidence record before merge |
| 5.3 | CP-5: cross-session memory as untrusted content that is labelled, stored out of policy reach and shown for review. It never carries rules. | 5.1 | Planted memory text triggers 5.1's protections |
| 5.4 | SD-6: recurring findings become proposed rules and regression tests, accepted by a human through the trusted path. | 3.4, 2.5 | A rule enters the policy only through a human answer |
| 5.5 | SD-4: risk-aware planning. The harness classifies a change by what it touches, and requires an approved plan with a threat-model step for schema, public API, auth, CI and dependency changes. The model cannot lower the classification. | 3.1 | A seeded auth change cannot start without an approved plan |
| 5.6 | SD-5: path-based quality policy, protected as a policy file. | 1.4, 3.4 | The commit records which policy applied |
| 5.7 | SD-7: architectural governance. Recorded decisions and dependency boundaries are run as checks through the tool contract. | 3.4 | A seeded boundary break blocks |
| 5.8 | SD-8: measured experiments in isolated worktrees or behind flags, scored by the verification engine. | 3.1, 3.7 | Result in the evidence record |

### Milestone 6: competitive validation

**Goal:** publish independently reproducible results for security,
engineering integrity and runtime efficiency.

| # | Item | Acceptance |
|---|---|---|
| 6.1 | SD-2: the 20-dependent-task longitudinal benchmark (analysis 9.2), across bwn and the competitors that can be pointed at the same model. | Manifest-pinned runs, three per sequence, raw records under `bench/results/`, held-out task portion |
| 6.2 | One-shot security: SusVibes through bwn with gates off and on, and through competitors on the same model. Run first at the end of Milestone 3. | Published with the method |
| 6.3 | The invariant suite as a cross-harness score: the share of seeded forbidden actions blocked, per surface. | Published per surface, never as one blanket number |
| 6.4 | CP-6: the efficiency bench refreshed, with Copilot CLI, Crush and Goose added and tokens per task measured. | Stored run |
| 6.5 | Refresh `docs/competitor-analysis.md` against the results. Competitor levels that rest on documentation are labelled as such, and adversarially tested where the harness can be run. | Dated report |

**Dependencies:** 6.1's tooling starts in Milestone 1 (1.13). 6.2 needs
3.5. Publication needs Milestones 1–3, so the published numbers describe
guarantees that hold.

## Dependency summary

```
M1 policy integrity + fail-closed + truthful docs + suite gate
 └─ M2 conformance suite, one policy object, extensions confined, decision log (D3)
     └─ M3 verification integrity → scanner gates → evidence; sandboxed git
         ├─ M4 parity (ACP from M2; per-artifact checks from M3)
         └─ M5 provenance → memory, background agents, learning, governance
M6 benchmark tooling from M1; publication after M3
```

## Measuring progress

Each milestone reports the mandate's success metrics that its tests can
measure:

- **From Milestone 1:** unauthorised actions blocked (the invariant suite
  score).
- **From Milestone 3:** verified acceptance criteria, and vulnerabilities
  introduced per change on the one-shot benchmark.
- **From Milestone 6:** the rest of the longitudinal benchmark's metrics,
  from task completion through to cost per verified successful task.

A milestone that cannot measure an improvement says so instead of claiming
one.
