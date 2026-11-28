//! One security policy per session (roadmap 2.3; analysis SO-5).
//!
//! Every limit a session places on what its surfaces may do is a field
//! here: the permission mode, the sandbox's mode and network switches, the
//! added folders commands may write, the credential stores the user exposed,
//! the hosts the user named, whether the session has read untrusted text,
//! and read-only. [`Policy::current`] reads them from where the session
//! keeps them; [`Policy::child`] is the one derivation every helper gets its
//! policy from; [`Policy::meet`] is the stricter of two policies field by
//! field, and [`Policy::within`] says whether one is no looser than another
//! in every field.
//!
//! A helper in this process shares the session's state, so its child
//! policy narrows what it is handed (permission, read-only). A background
//! workflow is a separate process: it gets the parent's policy as
//! `--inherit-policy`, and runs with the meet of its own settings and that,
//! so it can only ever be stricter. A value that doesn't parse means the
//! strictest policy.
//!
//! `meet` and `within` destructure the struct with no `..`, so a new field
//! does not compile until both say which way is stricter for it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::agent::Permission;
use crate::sandbox::Mode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Stricter first: ReadOnly, Ask, AcceptEdits, Auto.
    pub permission: Permission,
    /// Stricter first: Require, Auto, Off.
    pub sandbox: Mode,
    /// Commands may use the network at all (false is stricter).
    pub network: bool,
    /// The host checker holds commands' traffic (true is stricter).
    pub egress_check: bool,
    /// The user allowed the network on Landlock alone (false is stricter).
    pub landlock_network: bool,
    /// The user allowed commands to run unconfined (false is stricter).
    pub unconfined_allowed: bool,
    /// Added folders commands and file tools may write (fewer is stricter).
    pub roots: BTreeSet<PathBuf>,
    /// Credential stores the user made readable (fewer is stricter).
    pub expose: BTreeSet<PathBuf>,
    /// Hosts the user named, which commands may reach (fewer is stricter).
    pub user_hosts: BTreeSet<String>,
    /// The session read text someone else wrote (true is stricter): the
    /// tool it came through.
    pub untrusted: Option<String>,
    /// Only reads (true is stricter).
    pub read_only: bool,
}

/// What a helper asks for. It can only narrow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChildRequest {
    pub read_only: bool,
    /// A stricter permission for the child; a looser one is ignored.
    pub permission: Option<Permission>,
}

fn perm_rank(p: Permission) -> u8 {
    match p {
        Permission::ReadOnly => 0,
        Permission::Ask => 1,
        Permission::AcceptEdits => 2,
        Permission::Auto => 3,
    }
}

fn mode_rank(m: Mode) -> u8 {
    match m {
        Mode::Require => 0,
        Mode::Auto => 1,
        Mode::Off => 2,
    }
}

fn stricter_perm(a: Permission, b: Permission) -> Permission {
    if perm_rank(a) <= perm_rank(b) {
        a
    } else {
        b
    }
}

fn stricter_mode(a: Mode, b: Mode) -> Mode {
    if mode_rank(a) <= mode_rank(b) {
        a
    } else {
        b
    }
}

impl Policy {
    /// The strictest policy: what a session gets when an inherited policy
    /// can't be read.
    pub fn strictest() -> Policy {
        Policy {
            permission: Permission::ReadOnly,
            sandbox: Mode::Require,
            network: false,
            egress_check: true,
            landlock_network: false,
            unconfined_allowed: false,
            roots: BTreeSet::new(),
            expose: BTreeSet::new(),
            user_hosts: BTreeSet::new(),
            untrusted: Some("an unreadable inherited policy".into()),
            read_only: true,
        }
    }

    /// The session's policy now, under `permission`.
    pub fn current(permission: Permission) -> Policy {
        Policy {
            permission,
            sandbox: crate::sandbox::mode(),
            network: crate::sandbox::network(),
            egress_check: crate::sandbox::egress_check(),
            landlock_network: crate::sandbox::landlock_network_allowed(),
            unconfined_allowed: crate::sandbox::unconfined_allowed(),
            roots: crate::workdirs::list().into_iter().collect(),
            expose: crate::sandbox::exposed().into_iter().collect(),
            user_hosts: crate::egress::user_hosts().into_iter().collect(),
            untrusted: crate::agent::untrusted_read(),
            read_only: permission == Permission::ReadOnly,
        }
    }

    /// The policy a helper runs under: this one, narrowed by `req`.
    pub fn child(&self, req: &ChildRequest) -> Policy {
        let mut c = self.clone();
        if let Some(p) = req.permission {
            c.permission = stricter_perm(c.permission, p);
        }
        c.read_only = c.read_only || req.read_only || c.permission == Permission::ReadOnly;
        c
    }

    /// The stricter of `self` and `other` in every field.
    pub fn meet(&self, other: &Policy) -> Policy {
        let Policy {
            permission,
            sandbox,
            network,
            egress_check,
            landlock_network,
            unconfined_allowed,
            roots,
            expose,
            user_hosts,
            untrusted,
            read_only,
        } = self;
        let permission = stricter_perm(*permission, other.permission);
        Policy {
            permission,
            sandbox: stricter_mode(*sandbox, other.sandbox),
            network: *network && other.network,
            egress_check: *egress_check || other.egress_check,
            landlock_network: *landlock_network && other.landlock_network,
            unconfined_allowed: *unconfined_allowed && other.unconfined_allowed,
            roots: roots.intersection(&other.roots).cloned().collect(),
            expose: expose.intersection(&other.expose).cloned().collect(),
            user_hosts: user_hosts
                .intersection(&other.user_hosts)
                .cloned()
                .collect(),
            untrusted: untrusted.clone().or_else(|| other.untrusted.clone()),
            read_only: *read_only || other.read_only || permission == Permission::ReadOnly,
        }
    }

    /// Whether `self` is no looser than `parent` in any field.
    pub fn within(&self, parent: &Policy) -> bool {
        let Policy {
            permission,
            sandbox,
            network,
            egress_check,
            landlock_network,
            unconfined_allowed,
            roots,
            expose,
            user_hosts,
            untrusted,
            read_only,
        } = self;
        perm_rank(*permission) <= perm_rank(parent.permission)
            && mode_rank(*sandbox) <= mode_rank(parent.sandbox)
            && (!*network || parent.network)
            && (*egress_check || !parent.egress_check)
            && (!*landlock_network || parent.landlock_network)
            && (!*unconfined_allowed || parent.unconfined_allowed)
            && roots.is_subset(&parent.roots)
            && expose.is_subset(&parent.expose)
            && user_hosts.is_subset(&parent.user_hosts)
            && (untrusted.is_some() || parent.untrusted.is_none())
            && (*read_only || !parent.read_only)
    }

    /// The policy as one argument for a child process.
    pub fn encode(&self) -> String {
        let paths = |s: &BTreeSet<PathBuf>| -> Vec<String> {
            s.iter().map(|p| p.to_string_lossy().into_owned()).collect()
        };
        serde_json::json!({
            "permission": crate::agent::permission_name(self.permission),
            "sandbox": self.sandbox.as_str(),
            "network": self.network,
            "egress_check": self.egress_check,
            "landlock_network": self.landlock_network,
            "unconfined_allowed": self.unconfined_allowed,
            "roots": paths(&self.roots),
            "expose": paths(&self.expose),
            "user_hosts": self.user_hosts,
            "untrusted": self.untrusted,
            "read_only": self.read_only,
        })
        .to_string()
    }

    /// The policy from [`Policy::encode`]'s output. Anything missing,
    /// mistyped or unknown is an error; the caller then runs with
    /// [`Policy::strictest`].
    pub fn decode(s: &str) -> Result<Policy, String> {
        let v: serde_json::Value = serde_json::from_str(s).map_err(|e| format!("not JSON: {e}"))?;
        let o = v.as_object().ok_or("not an object")?;
        const KEYS: &[&str] = &[
            "permission",
            "sandbox",
            "network",
            "egress_check",
            "landlock_network",
            "unconfined_allowed",
            "roots",
            "expose",
            "user_hosts",
            "untrusted",
            "read_only",
        ];
        if let Some(k) = o.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(format!("unknown field {k}"));
        }
        let b = |k: &str| o.get(k).and_then(|v| v.as_bool()).ok_or(k.to_string());
        let list = |k: &str| -> Result<Vec<String>, String> {
            o.get(k)
                .and_then(|v| v.as_array())
                .ok_or(k.to_string())?
                .iter()
                .map(|x| x.as_str().map(str::to_string).ok_or(k.to_string()))
                .collect()
        };
        let s = |k: &str| o.get(k).and_then(|v| v.as_str()).ok_or(k.to_string());
        let untrusted = match o.get("untrusted") {
            Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(t)) => Some(t.clone()),
            _ => return Err("untrusted".into()),
        };
        Ok(Policy {
            permission: crate::agent::parse_permission(s("permission")?)?,
            sandbox: Mode::parse(s("sandbox")?).ok_or("sandbox")?,
            network: b("network")?,
            egress_check: b("egress_check")?,
            landlock_network: b("landlock_network")?,
            unconfined_allowed: b("unconfined_allowed")?,
            roots: list("roots")?.into_iter().map(PathBuf::from).collect(),
            expose: list("expose")?.into_iter().map(PathBuf::from).collect(),
            user_hosts: list("user_hosts")?.into_iter().collect(),
            untrusted,
            read_only: b("read_only")?,
        })
    }

    /// Make this the session's policy (a child process, after its own
    /// settings are applied and met with the inherited policy). Returns the
    /// permission to run under.
    pub fn apply(&self) -> Permission {
        crate::sandbox::set_mode(self.sandbox);
        crate::sandbox::set_network(self.network);
        crate::sandbox::set_egress_check(self.egress_check);
        if !self.landlock_network {
            crate::sandbox::forget_landlock_network();
        }
        if !self.unconfined_allowed {
            crate::sandbox::forget_unconfined();
        }
        crate::workdirs::retain(|p| self.roots.contains(p));
        let expose: Vec<String> = self
            .expose
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        crate::sandbox::set_expose(&expose);
        // The parent's hosts, frozen: the child's task is the parent model's
        // text, not the user's, so it names no host.
        crate::egress::set_user_hosts(self.user_hosts.iter().cloned().collect());
        crate::egress::freeze_user_hosts();
        if let Some(t) = &self.untrusted {
            crate::agent::inherit_untrusted(t);
        }
        if self.read_only {
            Permission::ReadOnly
        } else {
            self.permission
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small seeded generator: the same cases every run, no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn bit(&mut self) -> bool {
            self.next() & 1 == 1
        }
        fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
            xs[(self.next() % xs.len() as u64) as usize]
        }
        fn subset(&mut self, pool: &[&str]) -> BTreeSet<String> {
            pool.iter()
                .filter(|_| self.bit())
                .map(|s| s.to_string())
                .collect()
        }
    }

    const PERMS: &[Permission] = &[
        Permission::ReadOnly,
        Permission::Ask,
        Permission::AcceptEdits,
        Permission::Auto,
    ];
    const MODES: &[Mode] = &[Mode::Off, Mode::Auto, Mode::Require];

    fn random_policy(r: &mut Rng) -> Policy {
        let paths = |set: BTreeSet<String>| set.into_iter().map(PathBuf::from).collect();
        let permission = r.pick(PERMS);
        Policy {
            permission,
            sandbox: r.pick(MODES),
            network: r.bit(),
            egress_check: r.bit(),
            landlock_network: r.bit(),
            unconfined_allowed: r.bit(),
            roots: paths(r.subset(&["/w/a", "/w/b", "/w/c"])),
            expose: paths(r.subset(&["/h/.ssh", "/h/.aws"])),
            user_hosts: r.subset(&["a.example", "b.example", "c.example"]),
            untrusted: r.bit().then(|| "fetch_url".to_string()),
            read_only: r.bit() || permission == Permission::ReadOnly,
        }
    }

    // IH-2 (roadmap 2.3): for random parent policies and random requests,
    // every child policy is a subset of its parent in every field; and the
    // meet of two policies (a workflow's own settings with what it
    // inherited) is within both.
    #[test]
    fn invariant_ih2_every_child_policy_is_within_its_parent() {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..5000 {
            let parent = random_policy(&mut r);
            let req = ChildRequest {
                read_only: r.bit(),
                permission: r.bit().then(|| r.pick(PERMS)),
            };
            let child = parent.child(&req);
            assert!(child.within(&parent), "{parent:?}\n{req:?}\n{child:?}");
            // A grandchild too.
            let grand = child.child(&ChildRequest::default());
            assert!(grand.within(&parent));
            let own = random_policy(&mut r);
            let met = own.meet(&parent);
            assert!(
                met.within(&parent) && met.within(&own),
                "{own:?}\n{parent:?}\n{met:?}"
            );
        }
    }

    // `within` itself must refuse each looser field, or IH-2 proves
    // nothing: loosen one field at a time from the strictest policy.
    #[test]
    fn within_refuses_each_looser_field() {
        let base = Policy::strictest();
        let looser: Vec<(&str, Policy)> = vec![
            (
                "permission",
                Policy {
                    permission: Permission::Auto,
                    read_only: false,
                    ..base.clone()
                },
            ),
            (
                "sandbox",
                Policy {
                    sandbox: Mode::Off,
                    ..base.clone()
                },
            ),
            (
                "network",
                Policy {
                    network: true,
                    ..base.clone()
                },
            ),
            (
                "egress_check",
                Policy {
                    egress_check: false,
                    ..base.clone()
                },
            ),
            (
                "landlock_network",
                Policy {
                    landlock_network: true,
                    ..base.clone()
                },
            ),
            (
                "unconfined_allowed",
                Policy {
                    unconfined_allowed: true,
                    ..base.clone()
                },
            ),
            (
                "roots",
                Policy {
                    roots: [PathBuf::from("/x")].into(),
                    ..base.clone()
                },
            ),
            (
                "expose",
                Policy {
                    expose: [PathBuf::from("/h/.ssh")].into(),
                    ..base.clone()
                },
            ),
            (
                "user_hosts",
                Policy {
                    user_hosts: ["a.example".to_string()].into(),
                    ..base.clone()
                },
            ),
            (
                "untrusted",
                Policy {
                    untrusted: None,
                    ..base.clone()
                },
            ),
            (
                "read_only",
                Policy {
                    read_only: false,
                    ..base.clone()
                },
            ),
        ];
        for (field, p) in looser {
            assert!(
                !p.within(&base),
                "{field}: a looser value counted as within"
            );
            assert!(base.within(&p), "{field}");
        }
    }

    #[test]
    fn a_policy_round_trips_and_a_bad_one_is_refused() {
        let mut r = Rng(7);
        for _ in 0..200 {
            let p = random_policy(&mut r);
            assert_eq!(Policy::decode(&p.encode()).unwrap(), p);
        }
        for bad in [
            "",
            "{}",
            "[]",
            r#"{"permission":"auto"}"#,
            &Policy::strictest()
                .encode()
                .replace("\"require\"", "\"sometimes\""),
            &Policy::strictest().encode().replace("{", "{\"extra\":1,"),
        ] {
            assert!(Policy::decode(bad).is_err(), "{bad}");
        }
    }
}
