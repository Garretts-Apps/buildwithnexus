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
| 2.4 | Stdio MCP servers and hooks start inside the sandbox by default. Any widening is per entry and shown in the startup check. File-tool reads follow the same hidden-store policy as commands. Complete when this row's PR passes CI. `sandbox::extension_command` starts a stdio server's or a command/script hook's process through the same sandbox as a command; `"sandbox": false` on the entry runs it outside, and the startup check's MCP and hooks rows name it (the row key changes, so widening re-asks). Hook scripts in bwn's own folder and compiled Rust hooks run outside (the sandbox hides that folder) and are named too. With the sandbox on, the gate refuses file tools on a hidden store in every mode; bwn's own skills, agents and commands stay readable. Tests: `invariant_ih1_an_extension_starts_inside_the_sandbox`, `invariant_ih1_a_stdio_server_starts_inside_the_sandbox`, `invariant_ih1_a_hook_runs_inside_the_sandbox`, `invariant_ih1_file_tools_follow_the_hidden_store_policy` (fails with the check removed: auto only asked), `the_posture_check_names_an_mcp_server_run_outside_the_sandbox`. | H1, H2, H3 | IH-1 for MCP stdio, hooks and file tools |
| 2.5 | SO-7: a decision log of every gate decision, approval, exception, startup-check acceptance and verification result, with the D3 trust anchor chosen and implemented. Trust, startup-check and rules stores get an integrity check. In progress. D3 is decided by the owner: the OS keyring plus commit trailers. Done: the per-session hash-chained log (`harness/src/audit.rs`) and the separate verifier `bwn-audit-verify` (`harness/src/audit_verify.rs`, no shared code). `invariant_ac1_the_decision_log_lists_every_decision_and_catches_tampering` checks an edited line, a deleted line and a log cut short against a recorded head. The anchor: the head goes to the OS keyring at turn and session end and into a `Bwn-Decision-Log:` trailer on each commit bwn makes; `invariant_ac1_a_rewritten_chain_fails_against_the_anchored_head` shows a consistently rewritten log verifying alone and failing against it. `invariant_seatbelt_keychain_is_unreachable` failed before the Seatbelt keychain deny (a sandboxed command read an item back) and passes after it. The store integrity checks: `harness/src/seal.rs` seals the trust and posture stores on write and checks them on read (a rewritten or unsealed one reads as empty: `invariant_i3_a_store_rewritten_outside_bwn_is_caught`, and `invariant_i3_a_forged_trust_store_trusts_nothing`, which fails with the check removed); the rules folder's digest is in the posture check and sealed on acceptance. Complete when this row's PRs pass CI. Not claimed: a Linux Secret Service reachability test (the capability table says so). | C1, C2, C3, I3 | AC-1, including detection of a rewritten chain |
| 2.6 | D4, second part: Windows restricted execution mode. Complete when this row's PR passes CI: where no backend confines commands (Windows; a machine with none working) and the sandbox isn't off, every command asks in every mode, one at a time, and no allow rule, saved approval or auto mode skips it (`sandbox::restricted_execution`). `invariant_d4_restricted_mode_asks_before_every_command` runs in the Windows and `none` invariant cells; with the check removed, auto mode ran the command through an allow rule. | Windows | A Windows CI cell asks before every command in `auto` |
| 2.7 | Invariant suite: AU-3, AU-4, IN-1 (all surfaces), IH-1, IH-2, IH-3, AC-1. Complete when this row's PR passes CI. Every case is an `invariant_` test, run on each backend by the invariant cells, and the release workflow runs that CI before it tags or publishes (`release.yml`, job `ci`). AU-3: `invariant_au3_commands_cannot_write_bwns_policy_files` (direct path, symlink, variable, interpreter; with the sandbox off it fails, the settings file changed). AU-4: `invariant_au4_hosts_the_model_names_stay_refused` (the model's text, a tool result and a helper task; with the host in the user's own task the refusal changes). IN-1: file tools (`invariant_in1_file_tools_ask_before_changing_project_policy_files`), commands (`invariant_in1_a_command_cannot_change_a_project_policy_file`), helpers (through the same gate), MCP servers and hooks (inside the sandbox since 2.4, `invariant_ih1_*`; on Landlock alone a changed project file is untrusted until a person trusts it again). IH-1: `invariant_ih1_*`. IH-2: `invariant_ih2_*`. IH-3: `invariant_ih3_a_workflow_inherits_the_untrusted_mark`, `invariant_ih3_a_helper_under_ask_cannot_act_as_auto`. AC-1: `invariant_ac1_*`. | | Release gate |

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
| 3.1 | SO-3: mandatory checks come from project policy, not the model's `command`. Changes are detected from the working tree, not from tool names. A failed mandatory check is a failure in every interface. Helpers' changes count. Done (PR 1): the working tree is snapshotted at turn start and compared at finish (`harness/src/tree.rs`: `git status` with content hashes in a work tree, sizes and times outside one, never equal when too large to take), so an edit through `run_command` or a helper counts; the project's detected checks run whenever the tree changed since their last pass, up to two rounds, and a run that still fails exits 7; a `check_work` with the model's own `command` or `only` is advisory and never satisfies them. `invariant_ve1_*`, `invariant_ve2_*` (plain folder and git work tree), `invariant_ve3_*` and `invariant_ve5_*` fail on the old code. PR 2: the `checks` setting names the mandatory checks (detected ones without it, none for `[]`); a project file's list waits for trust and the trust screen shows each command (`invariant_so3_the_checks_setting_names_the_mandatory_checks`, `invariant_so3_an_untrusted_projects_checks_do_not_replace_the_detected_ones`, which fails with `checks` treated as harmless; AU-1 classifies the key). The checks are pinned at turn start, so rewriting the script they run or deleting the manifest fails them (`invariant_so3_a_turn_cannot_rewrite_or_remove_its_own_checks`), and a named check that can't run fails (`invariant_so3_a_named_check_that_cannot_run_fails`). V6: checks a rule refuses end the run `approval_blocked` and are listed among its denials (`invariant_ve3_checks_a_rule_refuses_do_not_end_in_success`). V7: failing checks make the verifier's status `failed` (asserted in `invariant_ve3_*`). Interfaces: headless runs, the GitHub Action and background workflows read the exit code and outcome; ACP has no failure stop reason and ends such a turn with `end_turn`. V3's commit half is 3.8. Complete when this row's PRs pass CI. | V1, V2, V3, V5, V6, V7 | VE-1, VE-2, VE-3, VE-5 |
| 3.2 | Verification facts come from checks, not tool names; security requirements are satisfiable only by a real check. Done: `tests_run` comes only from the project's checks; `caller_search` takes bwn's code-search tools by exact name; the license, vulnerability, maintenance and size reviews are met only by a person's `BWN_CHECKS_DONE` until 3.4 and 3.5 add scanners; the security review is met by a passing `security` check from the `checks` setting on the final tree. Dependencies added during the turn are read from the manifests. `invariant_ve4_tool_names_do_not_meet_requirements`, `invariant_ve4_a_web_search_meets_no_review` and `invariant_ve4_a_named_security_check_meets_the_security_review` fail on the old code. Complete when this row's PR passes CI. | V4 | VE-4 |
| 3.3 | SO-6: `add`, `commit`, `fetch`, `pull`, `rebase` and `stash` work in the sandbox. Hooks and config stay byte-identical, and credentials go through a broker. Done: `git` in a sandboxed command is bwn's shim, reaching bwn through the egress proxy with a session token; bwn runs git outside for the repositories pinned at start, with a per-subcommand option allowlist, path arguments in the workspace and out of the hidden stores, a fixed environment and no hooks, fsmonitor, pager, editor or external diff; refused for read-only sessions and reviews and on Landlock alone; logged. `invariant_au5_planted_git_hooks_and_config_never_run` (the commit goes through, `.git/config` and hooks byte-identical, nothing planted runs when bwn and the user run git), `invariant_au5_only_listed_git_subcommands_and_options_pass`, `invariant_au5_a_repository_made_later_or_above_is_not_served`, `invariant_au5_the_brokered_git_runs_no_pager_diff_program_or_hook` (each override shown load-bearing). Fetch and pull: a configured remote only (no URL or path), its URL resolved with `ls-remote --get-url`, HTTPS or SSH only, its host through the egress check; credentials are the user's SSH agent and helpers, used outside the sandbox (`invariant_au5_fetch_reaches_only_allowed_remote_hosts`, `remote_urls_name_their_host_and_only_https_and_ssh_pass`). Complete when this row's PR passes CI. | A2, A4, the commit half of V3 | AU-5 |
| 3.4 | The assurance tool contract (analysis 7.4): declarative checks, SARIF 2.1.0 or JSON results, exit-code gates, run through the sandbox. The model cannot add, remove or reorder entries.  Done: a `checks` entry takes `format` (`exit`, `sarif`, `json`), `fail_on` (a SARIF level) and `output` (a report file, removed before the run); a report decides the verdict, an unreadable one fails; entries run through the sandbox like every check and come only from the user's settings or a trusted project file. `reports_decide_checks_that_name_a_format` drives a stub scanner through every case. Complete when this row's PR passes CI. | | Contract tests with stub scanners |
| 3.5 | SD-1, first slice: secrets (gitleaks) on each edit and dependencies (osv-scanner or the ecosystem audit) on manifest changes, as blocking gates. A required scanner that is missing blocks.  Done: `security_gates` (`secrets`, `dependencies`) adds gitleaks (JSON report file) to every mandatory round and osv-scanner (JSON) when a manifest or lockfile changed, through the 3.4 report contract, so a missing scanner leaves no report and blocks; the model's own `check_work` never stands in for a gated round. `invariant_sd1_secret_and_dependency_gates_block_and_a_missing_scanner_blocks` (stub scanners) covers a seeded secret, a seeded vulnerable dependency, a non-manifest change and a missing scanner. Opt-in, not on by default. Complete when this row's PR passes CI. | | A seeded secret and a seeded vulnerable dependency each block completion |
| 3.6 | SD-1, second slice: SAST (semgrep) on changed files before finish; configuration scanning for Docker, Terraform, Kubernetes and CI files.  Done: `security_gates` `sast` (semgrep, SARIF, over the turn's changed files) and `config` (`trivy config`, SARIF, when a Dockerfile, Terraform, Kubernetes or CI file changed), through the 3.4 contract. `invariant_sd1_sast_and_configuration_findings_block` (stub scanners). Complete when this row's PR passes CI. | | Seeded findings block |
| 3.7 | SD-3: an evidence record per completed change, referenced from a commit trailer, checked by `bwn evidence verify <commit>`. SBOM regenerated by a real tool when dependencies change.  In progress. Done: the record (`harness/src/evidence.rs`) is written when a turn's change is committed (3.8), named by its SHA-256 in a `Bwn-Evidence:` trailer, and `buildwithnexus evidence verify <commit>` checks its hash and each file's blob against the commit; `invariant_ac2_a_commit_verifies_against_its_evidence_record` covers an edited record, an amended commit and a commit with no record. The SBOM: the `sbom` gate regenerates `bom.json` with syft (CycloneDX) when a manifest or lockfile changed, a missing tool blocks, and the evidence record names its blob (`the_sbom_is_regenerated_when_dependencies_change`, stub syft). Complete when this row's PRs pass CI. | C4 | AC-2 |
| 3.8 | Commits wait for verification. Per-edit auto-commits are replaced or held until the change passes. Done: file-tool edits are held and committed once at the end of a top-level turn whose checks didn't fail and whose verifier didn't block; held edits carry to the next turn. `invariant_ve3_no_commit_is_made_for_a_failing_change` fails on the old code. Complete when this row's PR passes CI. | V3 | VE-3, no commit made for a failing change |
| 3.9 | Invariant suite: VE-1 to VE-5, AU-5, AC-2. Done: every case is an `invariant_` test, run on each backend by the invariant cells, and the release workflow runs that CI before it tags or publishes (2.7). VE-1 `invariant_ve1_*`, VE-2 `invariant_ve2_*`, VE-3 `invariant_ve3_*` (failing checks, refused checks, no commit for a failing change), VE-4 `invariant_ve4_*`, VE-5 `invariant_ve5_*`, AU-5 `invariant_au5_*`, AC-2 `invariant_ac2_*`; the gates add `invariant_sd1_*`. Complete when this row's PR passes CI. | | Release gate |

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
| 4.1 | A parity audit against the backlog below, scoring each row against the current releases of the leading harnesses. It turns *To verify* rows into shipped, partial or gap. Done 2026-11-28: every row has a status and sources; the new rows are ranked for 4.6. | Backlog table updated with sources |
| 4.2 | CP-1: editor, ACP and headless parity. One suite of gate decisions gives the same answers through the terminal, ACP and `--json`. The question tool and the startup check are answerable in all three.  In progress. Done: `gate_decisions_are_the_same_in_json_and_acp` runs one table (read, edit, command, a dangerous command, read-only, a deny rule, across ask, auto and readonly) through `--json` and ACP and gets the same allow, ask or deny in both; the terminal shares the gate function. The question tool is answered in the editor as a choice between its options (`acp_the_question_tool_is_answered_in_the_editor`; it was refused for want of a terminal, and a chat turn reported it unknown in every interface). The startup check is asked in the editor before the first model request, and a refusal keeps the turn from the model (`acp_the_startup_check_is_answered_in_the_editor`). Complete when this row's PR passes CI. | Cross-interface decision suite |
| 4.3 | CP-2: per-artifact verification. A project file maps artifact types to checks, the harness picks them by changed paths, and the verdicts feed 3.1.  Done: a `checks` entry takes `paths` globs, matched against each changed file's path and name; the matching checks run in the mandatory round and their verdicts are the 3.1 verdict. `per_artifact_checks_run_for_their_files_and_block` seeds a failure in a stylesheet and a script and changes a README. Complete when this row's PR passes CI. | Each artifact type has a seeded failure that blocks |
| 4.4 | CP-3: Bedrock, Vertex and Azure credential flows, signed in-tree (no vendor SDKs), with keys bound to their endpoints. Subscription sign-in only after the providers' terms for third-party clients are read. Done: `azure` (the `api-key` header), `vertex` (Application Default Credentials; a service-account assertion signed RS256 in-tree, or a user's refresh token, exchanged only at Google's token endpoint whatever the file's `token_uri` says) and `bedrock` (a Bedrock API key as a Bearer token, or SigV4 in-tree; InvokeModel with Anthropic's body, the streamed reply's event-stream frames checked by both CRCs). Each credential goes only to its cloud's hosts, checked before any credential is read: `invariant_ih1_an_azure_key_goes_only_to_an_azure_endpoint`, `invariant_ih1_vertex_credentials_go_only_to_a_vertex_endpoint`, `invariant_ih1_bedrock_credentials_go_only_to_a_bedrock_endpoint`, `invariant_ih1_a_key_files_token_uri_does_not_redirect_the_exchange`. SigV4 matches AWS's published test suite (`sigv4_matches_the_aws_test_suite`); a corrupt or oversized frame is refused (`invariant_a_corrupt_or_oversized_event_stream_frame_is_refused`); Bedrock throttling reads as busy for `fallback_model`. The variables are stripped from commands, and the key and credentials files are hidden stores. The endpoint is the startup check's model-endpoint row. The terms were read: AWS's, Google's and Microsoft's service terms don't restrict client applications; Anthropic doesn't permit third-party apps to offer Claude subscription sign-in, so bwn doesn't (docs/providers.md). Verified against tests and AWS's vectors, not a live account. Complete when this row's PRs pass CI. | Credentials never reach commands (IH-1); the endpoint is shown in the startup check |
| 4.5 | Context management: compaction controls and policy (thresholds, what is kept, untrusted labels kept through compaction). Done: the summary of turns that read untrusted text carries the label a resumed session looks for (`invariant_ih3_the_untrusted_mark_survives_compaction_and_resume`, which fails without it); the `compaction` setting sets the threshold (`threshold_percent`, held to 20–95, default 80) and what stays verbatim (`keep_recent`, at least 2) (`the_compaction_setting_moves_the_threshold_and_the_kept_tail`). Complete when this row's PR passes CI. | The untrusted mark survives compaction and resume |
| 4.6 | Backlog items the audit ranks highest, each as its own item with tests. Done: `/voice` reads only the transcript whisper writes in this run, never a file beside the audio (`invariant_in1_a_planted_transcript_is_never_the_voice_input`). Model routing per role: an agent file's `model:` runs that helper on its own model at the session's endpoint (`an_agent_files_model_runs_that_helper`). A fallback model: `fallback_model` on the same endpoint is tried once when the session's model is busy or limiting past its retries, never to an unpriced model under a spending cap (`a_busy_model_falls_back_to_the_fallback_model`). Plugins as one unit: `plugins` (`{"kit": true}` is NEXUS_HOME/plugins/kit, or a `path`) loads a plugin's `commands/`, `skills/`, `agents/` and the `hooks` and `mcp_servers` of its `plugin.json` together; `"enabled": false` loads none of it (`a_plugin_loads_its_agents_and_hooks_as_one_unit`). LSP diagnostics: an `lsp` entry names a language server for some extensions; after an edit its errors and warnings for that file are added to the edit's result (`an_edits_result_carries_the_language_servers_diagnostics`, a stub server). The ranked list's other two rows are placed: cloud delegation goes with Milestone 5's background agents, and a browser with the user's signed-in session is planned after 5.1 (see its row). Complete when this row's PRs pass CI. | Per item |

**Dependencies:** needs Milestone 3 for 4.3. 4.2 can start after
Milestone 2.

#### Parity backlog

Audited in 4.1 (2026-11-28) against bwn's code and the other harnesses'
current documentation. Numbers in brackets are the sources listed below.

| Capability | bwn today | Others | Note |
|---|---|---|---|
| Manual compaction | Shipped (`/compact`) | Copilot CLI, also automatic [3]; Codex (unverified) [9] | |
| Compaction controls | Shipped (4.5: the `compaction` setting; untrusted labels survive it) | OpenCode `compaction.auto/keep/buffer` [6]; Claude Code summarizes from a checkpoint [11] | |
| Model routing per role (helpers, compaction, review) | Partial: an agent file's `model:` routes its helper (4.6); compaction and review use the session model | Claude Code subagent `model:` [2]; Aider `--weak-model`, `--editor-model` [5]; OpenCode per-agent model [6] | Each route's endpoint in the startup check |
| Session branching | Partial: `/fork`, `/rewind`; no parent link or tree | Claude Code `/branch`, `--fork-session` [11]; Codex `codex fork` (unverified) [9] | A browsable tree is the gap |
| Checkpoints and undo | Shipped | Claude Code `/rewind` [11]; Gemini CLI `/restore`, off by default [12] | Shell changes aren't checkpointed, as in Claude Code [11] |
| Plugins: skills, hooks, MCP servers and agents as one unit | Shipped (4.6: `plugins` loads a folder's commands, skills, agents, hooks and MCP servers together; trust-gated) | Claude Code plugins [1]; Gemini CLI extensions [4]; Codex plugins [10] | Local and trust-gated; no marketplace |
| Images | Shipped (paste, `read_file`) | Codex `--image` [10] | |
| PDFs, audio, video | Partial: PDFs as text (pdftotext), video as ffmpeg frames, audio through a local whisper in `/voice` | Gemini CLI `read_file` reads images, audio and PDFs [7] | `/voice` sent a transcript file a checkout could plant; fixed in 4.6 |
| Browser automation beyond `screenshot_url` | Partial: `screenshot_url` is loopback-only; `headless_browser` fetches text | Claude Code Chrome integration [8] | Needs the browser surface in the invariant suite |
| Editor integration | Shipped (4.2: ACP; questions and the startup check answered in the editor) | Copilot CLI ACP [3] | |
| Bedrock, Vertex, Azure; subscription sign-in | Shipped (4.4: `bedrock`, `vertex`, `azure`); no Claude subscription sign-in, which Anthropic's terms don't permit for third-party apps | Claude Code Bedrock, Vertex, Foundry [13]; Crush Bedrock, Vertex, Azure [14]; Gemini CLI Vertex and Google sign-in [15] | 4.4 |
| Cross-session memory | Partial: hand-written `memory.md`, AGENTS.md, a project knowledge base | Claude Code auto memory [16]; Copilot Memory [3] | Milestone 5: treated as untrusted content |
| Background agents | Partial: timed workflows while bwn is open | Claude Code background subagents [2]; Codex `codex cloud` [10]; Copilot `--cloud` [3] | Milestone 5 |
| Reasoning effort control | Shipped (`/effort`) | Claude Code `/effort` [17] | |
| Headless JSON contract | Shipped | | |

Found by the audit and not in the backlog before, ranked for 4.6 by how
many of the leading harnesses ship them and how much they matter for
professional use:

| Capability | bwn today | Others |
|---|---|---|
| LSP diagnostics fed back to the model | Shipped (4.6: the `lsp` setting) | OpenCode [18], Crush [14] |
| Model routing per role (above) | Partial (4.6: agent files) | Claude Code [2], Aider [5], OpenCode [6] |
| Plugins as one unit (above) | Shipped (4.6) | Claude Code [1], Gemini CLI [4], Codex [10] |
| A fallback model on outages and rate limits | Shipped (4.6: `fallback_model`) | Claude Code `fallbackModel` [17] |
| Cloud or remote delegation | Gap: with background agents in Milestone 5 | Codex `codex cloud` [10], Copilot `--cloud` [3] |
| A browser with the user's signed-in session | Gap, planned after 5.1 (owner's decision, 2026-11-28): pages are external text under 5.1, and the browser surface joins the invariant suite before it ships | Claude Code Chrome [8] |
| Hooks, custom subagents, custom commands and skills, web search | Shipped | Claude Code [1][2], Copilot [3], Gemini CLI [4], Codex [10] |

Sources: [1] https://code.claude.com/docs/en/plugins [2]
https://code.claude.com/docs/en/sub-agents [3]
https://docs.github.com/en/copilot/concepts/agents/about-copilot-cli [4]
https://geminicli.com/docs/extensions/ [5]
https://aider.chat/docs/config/options.html [6]
https://opencode.ai/docs/config/ [7]
https://geminicli.com/docs/tools/file-system/ [8]
https://code.claude.com/docs/en/chrome [9]
https://developers.openai.com/codex/glossary (search results only,
unverified) [10] https://developers.openai.com/codex/cli/features [11]
https://code.claude.com/docs/en/checkpointing [12]
https://geminicli.com/docs/cli/checkpointing/ [13]
https://code.claude.com/docs/en/third-party-integrations [14]
https://github.com/charmbracelet/crush [15]
https://geminicli.com/docs/get-started/authentication/ [16]
https://code.claude.com/docs/en/memory [17]
https://code.claude.com/docs/en/settings [18]
https://opencode.ai/docs/lsp/

### Milestone 5: secure autonomous engineering

**Goal:** autonomy and learning that stay inside the policy and leave
evidence.

| # | Item | Depends on | Acceptance |
|---|---|---|---|
| 5.1 | SO-8: a source label on every tool result that survives compaction and resume. Repository files and command output are covered. Tiered (decided by the owner, 2026-11-28): external text (web, search, MCP results, files in dependency folders or outside the workspace, output of a command that reached the network) makes commands, fetches and MCP calls ask, as today; the project's own files and offline command output make fetches, MCP writes and edits of CI, hook or policy files ask, while sandboxed commands run. | 4.5 | A seeded set of planted instructions (web page, MCP result, dependency README, code comment) leads to no unasked fetch, MCP write, or edit of CI, hook or policy files in `auto`, and from an external source to no unasked command |
| 5.2 | CP-4: background agents as children of the session (no daemon), each in its own worktree and sandbox. A background agent cannot answer its own approvals. | 2.3, 3.1, 3.7 | IH-2, VE-5, an evidence record before merge |
| 5.3 | CP-5: cross-session memory as untrusted content that is labelled, stored out of policy reach and shown for review. It never carries rules. | 5.1 | Planted memory text triggers 5.1's protections |
| 5.4 | SD-6: recurring findings become proposed rules and regression tests, accepted by a human through the trusted path. | 3.4, 2.5 | A rule enters the policy only through a human answer |
| 5.5 | SD-4: risk-aware planning. The harness classifies a change by what it touches, and requires an approved plan with a threat-model step for schema, public API, auth, CI and dependency changes. The model cannot lower the classification. | 3.1 | A seeded auth change cannot start without an approved plan |
| 5.6 | SD-5: path-based quality policy, protected as a policy file. | 1.4, 3.4 | The commit records which policy applied |
| 5.7 | SD-7: architectural governance. Recorded decisions and dependency boundaries are run as checks through the tool contract. | 3.4 | A seeded boundary break blocks |
| 5.8 | SD-8: measured experiments in isolated worktrees or behind flags, scored by the verification engine. | 3.1, 3.7 | Result in the evidence record |
| 5.9 | A browser driven with the user's signed-in session (planned by the owner, 2026-11-28). Every page is external text under 5.1; the browser runs in its own profile copy, reaches only hosts the policy allows, and its surface joins the invariant suite. | 5.1 | A planted instruction on a page leads to no request to another site with the user's cookies and no form submission the user didn't ask for |

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
