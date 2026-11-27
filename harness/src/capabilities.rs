//! The per-backend sandbox guarantee table, in code (roadmap 2.1).
//!
//! `SECURITY.md`'s guarantee table is rendered from `CAPABILITIES` and a test
//! holds the file to it, so a cell can't drift from the code. Each row also
//! lists the tests that prove it (`tests`); another test checks every one of
//! those names exists as a `fn` in the source, so a citation can't name a
//! test that was renamed or removed. (That check was added after one such
//! citation went stale when a refactor dropped the test it named.)
//!
//! A cell is the prose shown in `SECURITY.md`. "No" and "Partial" cells are
//! documented gaps with a roadmap item, not bugs. The `status(backend, abi)`
//! shape the probes and posture rows will key on lands with roadmap 2.2; for
//! now the table is the prose and its citations.

/// One guarantee and what each backend does about it.
pub struct Cap {
    /// Stable id for probes and posture rows to key on.
    pub id: &'static str,
    /// First column: the guarantee.
    pub guarantee: &'static str,
    pub bwrap: &'static str,
    /// bwn's own user + mount namespace with Landlock and seccomp inside,
    /// used when bubblewrap is missing but user namespaces work.
    pub userns: &'static str,
    pub landlock: &'static str,
    pub seatbelt: &'static str,
    /// The last column, verbatim (may hold prose, not only test names).
    pub proven_by: &'static str,
    /// Test fns that prove this row; each must exist in the source and be
    /// named in `proven_by`.
    pub tests: &'static [&'static str],
}

/// The guarantee table. The order is the order shown in `SECURITY.md`.
pub const CAPABILITIES: &[Cap] = &[
    Cap {
        id: "writes_confined",
        guarantee: "Writes only in the workspace, added folders and temp",
        bwrap: "Yes",
        userns: "Yes, file metadata included: the rest of the tree is read-only. Plus all of `/dev` and the real `/tmp`",
        landlock: "Contents yes — a write, rename, link, create, `mkdir` or remove outside is refused — plus all of `/dev` and the real `/tmp`. A file's metadata outside stays writable (`chmod`, `chown`, `utime`, xattrs), and below ABI 3 truncation is seccomp's (see Older kernels). bwn uses its namespace backend instead where user namespaces are allowed",
        seatbelt: "Yes",
        proven_by: "`bwrap_confines_writes_to_the_workspace`, `invariant_userns_confines_metadata_outside`, `invariant_userns_keeps_the_workspace_writable_and_git_read_only`, `a_confined_command_can_write_inside_and_not_outside`, `invariant_landlock_confines_write_syscall_paths_outside`, `landlock_does_not_confine_file_metadata_outside`, `seatbelt_profile_denies_writes_except_workspace_and_tmp`",
        tests: &[
            "invariant_userns_confines_metadata_outside",
            "invariant_userns_keeps_the_workspace_writable_and_git_read_only",
            "bwrap_confines_writes_to_the_workspace",
            "a_confined_command_can_write_inside_and_not_outside",
            "invariant_landlock_confines_write_syscall_paths_outside",
            "landlock_does_not_confine_file_metadata_outside",
            "seatbelt_profile_denies_writes_except_workspace_and_tmp",
        ],
    },
    Cap {
        id: "policy_files_readonly",
        guarantee: "`.git` and `.buildwithnexus` read-only",
        bwrap: "Yes",
        userns: "Yes",
        landlock: "No: the posture check says so and auto mode asks before every command (roadmap 2.2)",
        seatbelt: "Yes",
        proven_by: "`bwrap_argv_keeps_git_and_settings_read_only`, `invariant_userns_keeps_the_workspace_writable_and_git_read_only`, `invariant_au5_the_landlock_policy_gap_is_disclosed_and_auto_asks`",
        tests: &[
            "invariant_userns_keeps_the_workspace_writable_and_git_read_only",
            "bwrap_argv_keeps_git_and_settings_read_only",
            "invariant_au5_the_landlock_policy_gap_is_disclosed_and_auto_asks",
        ],
    },
    Cap {
        id: "nested_git_readonly",
        guarantee: "Nested repositories' `.git` read-only",
        bwrap: "Up to 64 found in 10,000 folders, skipping `node_modules` and `target`",
        userns: "Same bounds as bubblewrap",
        landlock: "No",
        seatbelt: "Same bounds as bubblewrap",
        proven_by: "`nested_repositories_stay_read_only_too`, `invariant_userns_keeps_the_workspace_writable_and_git_read_only`",
        tests: &[
            "nested_repositories_stay_read_only_too",
            "invariant_userns_keeps_the_workspace_writable_and_git_read_only",
        ],
    },
    Cap {
        id: "credential_stores_hidden",
        guarantee: "Credential stores unreadable",
        bwrap: "Folders yes, contents and names. A hidden file is covered by an empty file, so its contents are hidden but whether it exists still shows",
        userns: "Folders: contents and names (an empty folder covers each store, so a guessed name is not found). A hidden file's contents are hidden, but whether it exists still shows",
        landlock: "Contents yes. Listing the folder is refused, so names can't be enumerated, but `stat` of a guessed name confirms it exists and a file's metadata stays writable (bwn's namespace backend closes both where user namespaces are allowed). Listing is blocked by also refusing to list `/`, `/home`, your home folder and `~/.config` (files in them stay readable by name)",
        seatbelt: "Yes",
        proven_by: "`bwrap_hides_credential_stores`, `landlock_hides_credential_stores`, `invariant_m5_landlock_does_not_list_hidden_store_names`, `landlock_credential_names_leak_via_stat`, `invariant_userns_hides_store_names`, `invariant_userns_command_cannot_undo_the_mounts`, `seatbelt_hides_credential_stores`",
        tests: &[
            "invariant_userns_hides_store_names",
            "invariant_userns_command_cannot_undo_the_mounts",
            "bwrap_hides_credential_stores",
            "landlock_hides_credential_stores",
            "invariant_m5_landlock_does_not_list_hidden_store_names",
            "landlock_credential_names_leak_via_stat",
            "seatbelt_hides_credential_stores",
        ],
    },
    Cap {
        id: "store_contains_workspace",
        guarantee: "A store that contains the workspace hidden",
        bwrap: "No",
        userns: "No (auto mode asks)",
        landlock: "No (auto mode asks)",
        seatbelt: "No",
        proven_by: "`invariant_g3_no_home_folder_means_nothing_is_claimed_hidden` covers the no-home case",
        tests: &["invariant_g3_no_home_folder_means_nothing_is_claimed_hidden"],
    },
    Cap {
        id: "agent_socket_unreachable",
        guarantee: "SSH agent socket unreachable",
        bwrap: "Yes",
        userns: "Only while the network is checked or off",
        landlock: "Only while the network is checked or off",
        seatbelt: "Yes",
        proven_by: "`seatbelt_profile_denies_reads_of_hidden_paths`",
        tests: &["seatbelt_profile_denies_reads_of_hidden_paths"],
    },
    Cap {
        id: "network_through_checker",
        guarantee: "Network only through the host checker",
        bwrap: "Through the checker, held by port: the checker's port on another address also connects until the private network namespace lands (roadmap 2.2). Only plain TCP sockets. No network if bwn's own binary can't be found",
        userns: "As bubblewrap: through the checker, held by port, only plain TCP sockets (the private network namespace is roadmap 2.2)",
        landlock: "No network, unless the user allows it for the session (`/sandbox network`, `--sandbox-network`): Landlock alone holds a command by port only, and no private network namespace is possible here. Once allowed, with Landlock ABI 4 (kernel 6.7), as bubblewrap",
        seatbelt: "Yes",
        proven_by: "`bwrap_limits_the_network_to_the_proxy`, `landlock_limits_the_network_to_the_proxy`, `seatbelt_limits_the_network_to_the_proxy`, `invariant_m8b_no_network_limiter_means_no_network`, `invariant_sec1_mptcp_socket_is_refused_under_the_proxy`, `invariant_m8c_x32_syscalls_are_refused`, `invariant_landlock_alone_has_no_network_until_allowed`",
        tests: &[
            "invariant_landlock_alone_has_no_network_until_allowed",
            "bwrap_limits_the_network_to_the_proxy",
            "landlock_limits_the_network_to_the_proxy",
            "seatbelt_limits_the_network_to_the_proxy",
            "invariant_m8b_no_network_limiter_means_no_network",
            "invariant_sec1_mptcp_socket_is_refused_under_the_proxy",
            "invariant_m8c_x32_syscalls_are_refused",
        ],
    },
    Cap {
        id: "no_inbound",
        guarantee: "No inbound connections",
        bwrap: "No: a command can listen",
        userns: "No",
        landlock: "No",
        seatbelt: "No",
        proven_by: "Not guaranteed",
        tests: &[],
    },
    Cap {
        id: "provider_keys_stripped",
        guarantee: "Provider keys and tokens stripped from the environment",
        bwrap: "Yes",
        userns: "Yes",
        landlock: "Yes",
        seatbelt: "Yes (and with no backend)",
        proven_by: "`sandboxed_children_lose_sockets_and_provider_keys`",
        tests: &["sandboxed_children_lose_sockets_and_provider_keys"],
    },
    Cap {
        id: "older_kernels",
        guarantee: "Older kernels",
        bwrap: "n/a",
        userns: "As Landlock alone; the namespace needs `mount_setattr` (kernel 5.12), which every Landlock kernel has",
        landlock: "ABI 1 (5.13 to 5.18, Ubuntu 22.04) and ABI 2 (5.19 to 6.1, Debian 12): writes, credential stores and no-network hold. Landlock can't confine truncation there, so seccomp refuses `truncate(2)` and a read-only `open` with `O_TRUNC`, inside the workspace too (write-mode truncation — `: > file`, `ftruncate`, opening for writing with `O_TRUNC` — still works, because Landlock confines it); the posture check says so. Confirmed on 5.15 and 6.1",
        seatbelt: "n/a",
        proven_by: "`a_confined_command_can_write_inside_and_not_outside`, `invariant_m2_a_confined_command_cannot_truncate_outside`, `invariant_m2_seccomp_refuses_truncate_below_abi_3`, `invariant_m2_seccomp_refuses_otrunc_open_below_abi3`, run in 5.15 and 6.1 VMs",
        tests: &[
            "a_confined_command_can_write_inside_and_not_outside",
            "invariant_m2_a_confined_command_cannot_truncate_outside",
            "invariant_m2_seccomp_refuses_truncate_below_abi_3",
            "invariant_m2_seccomp_refuses_otrunc_open_below_abi3",
        ],
    },
];

/// The markers in `SECURITY.md` the rendered table sits between.
pub const BEGIN: &str =
    "<!-- capabilities:begin (generated from harness/src/capabilities.rs; do not edit by hand) -->";
pub const END: &str = "<!-- capabilities:end -->";

/// The guarantee table as the Markdown that appears in `SECURITY.md`,
/// between the markers (markers not included).
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str(
        "| Guarantee | bubblewrap | Namespace (built-in) | Landlock alone | Seatbelt | Proven by |\n",
    );
    out.push_str("|---|---|---|---|---|---|\n");
    for c in CAPABILITIES {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            c.guarantee, c.bwrap, c.userns, c.landlock, c.seatbelt, c.proven_by
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        // harness/ is the crate; the repo is its parent.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    // Every test named in a `proven_by` must exist as a `fn` in the source,
    // and every `tests` entry must be named in `proven_by`. This catches a
    // citation that names a renamed or deleted test (which would otherwise
    // read as proven while nothing ran).
    #[test]
    fn every_cited_test_exists_and_is_listed() {
        let harness = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut sources = String::new();
        for dir in ["src", "tests"] {
            collect_rs(&harness.join(dir), &mut sources);
        }
        for c in CAPABILITIES {
            for t in c.tests {
                assert!(
                    c.proven_by.contains(t),
                    "{}: test `{t}` is in `tests` but not named in `proven_by`",
                    c.id
                );
                assert!(
                    sources.contains(&format!("fn {t}(")),
                    "{}: cited test `{t}` has no `fn {t}` under harness/src or harness/tests",
                    c.id
                );
            }
        }
    }

    fn collect_rs(dir: &std::path::Path, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_rs(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    out.push_str(&s);
                    out.push('\n');
                }
            }
        }
    }

    // SECURITY.md's table must equal what the code renders. Set
    // BWN_REWRITE_CAPABILITIES=1 to rewrite the file from the code.
    #[test]
    fn security_md_table_matches_code() {
        let path = repo_root().join("SECURITY.md");
        let file = std::fs::read_to_string(&path).expect("read SECURITY.md");
        let want = render_markdown();
        let begin = file.find(BEGIN).unwrap_or_else(|| {
            panic!("SECURITY.md is missing the capabilities begin marker:\n{BEGIN}")
        });
        let end = file[begin..]
            .find(END)
            .map(|i| begin + i)
            .expect("SECURITY.md is missing the capabilities end marker");
        // The block between the markers, trimmed of the surrounding newlines.
        let between = file[begin + BEGIN.len()..end].trim_matches('\n');

        if std::env::var_os("BWN_REWRITE_CAPABILITIES").is_some() {
            let new = format!(
                "{}{}\n{}\n{}{}",
                &file[..begin + BEGIN.len()],
                "\n",
                want.trim_end_matches('\n'),
                "\n",
                &file[end..]
            );
            std::fs::write(&path, new).expect("rewrite SECURITY.md");
            return;
        }

        assert_eq!(
            between,
            want.trim_end_matches('\n'),
            "SECURITY.md's guarantee table is out of date; run the test with \
             BWN_REWRITE_CAPABILITIES=1 to regenerate it from harness/src/capabilities.rs"
        );
    }
}
