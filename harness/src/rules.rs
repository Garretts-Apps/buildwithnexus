use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::fs;

/// Task classifications for rules engine evaluation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    BugFix,
    Feature,
    Refactor,
    Migration,
    SecurityPatch,
    Documentation,
    DecisionSupport,
    CodeReview,
    Debugging,
    RiskReview,
}

impl fmt::Display for TaskType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::BugFix => "bug_fix",
            Self::Feature => "feature",
            Self::Refactor => "refactor",
            Self::Migration => "migration",
            Self::SecurityPatch => "security_patch",
            Self::Documentation => "documentation",
            Self::DecisionSupport => "decision_support",
            Self::CodeReview => "code_review",
            Self::Debugging => "debugging",
            Self::RiskReview => "risk_review",
        };
        write!(f, "{}", s)
    }
}

/// Severity level of a rule violation or constraint.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        };
        write!(f, "{}", s)
    }
}

/// Conditions under which an engineering rule applies.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    TaskTypeIs(TaskType),
    ChangeTouches(String),
    FileMatches(String),
    DependencyAdded(bool),
    MigrationType(String),
    Custom(String, Value),
}

/// An engineering or business constraint rule.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Rule {
    pub id: String,
    pub description: String,
    pub severity: Severity,
    #[serde(default)]
    pub applies_when: Vec<Condition>,
    #[serde(default)]
    pub requires: Vec<String>,
    pub message: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Represents a triggered rule violation during verification or execution.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RuleViolation {
    pub rule_id: String,
    pub rule_description: String,
    pub severity: Severity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
}

/// The context against which rules are evaluated.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EvaluationContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<TaskType>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub tools_called: Vec<String>,
    #[serde(default)]
    pub tests_added: Vec<String>,
    #[serde(default)]
    pub tests_run: bool,
    #[serde(default)]
    pub dependencies_added: Vec<String>,
    #[serde(default)]
    pub dependencies_removed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_type: Option<String>,
    #[serde(default)]
    pub has_rollback_plan: bool,
    #[serde(default)]
    pub has_changelog_entry: bool,
    #[serde(default)]
    pub security_review_done: bool,
    #[serde(default)]
    pub custom_facts: HashMap<String, Value>,
}

/// Rule engine that loads, evaluates, and enforces engineering rules.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RuleEngine {
    pub rules: Vec<Rule>,
}

impl RuleEngine {
    /// Creates a new empty RuleEngine.
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Loads built-in default engineering constraints.
    pub fn load_defaults() -> Self {
        let mut engine = Self::new();

        engine.add_rule(Rule {
            id: "bug_fix_requires_regression_test".to_string(),
            description: "Bug fixes must include a regression test".to_string(),
            severity: Severity::Medium,
            applies_when: vec![Condition::TaskTypeIs(TaskType::BugFix)],
            requires: vec![
                "failing_test_before_fix".to_string(),
                "passing_test_after_fix".to_string(),
            ],
            message: "Bug fix does not include a regression test.".to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "public_api_contract_change".to_string(),
            description: "Public API changes require caller analysis and changelog".to_string(),
            severity: Severity::High,
            applies_when: vec![Condition::ChangeTouches("public_api".to_string())],
            requires: vec![
                "caller_search".to_string(),
                "changelog_entry".to_string(),
                "compatibility_assessment".to_string(),
            ],
            message: "Public API contract changed without caller search or changelog entry."
                .to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "destructive_database_migration".to_string(),
            description: "Destructive migrations require rollback plan".to_string(),
            severity: Severity::Critical,
            applies_when: vec![Condition::MigrationType("destructive".to_string())],
            requires: vec![
                "rollback_plan".to_string(),
                "backup_plan".to_string(),
                "approval".to_string(),
            ],
            message: "Destructive database migration proposed without a rollback plan.".to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "new_dependency_review".to_string(),
            description: "New dependencies require security and license review".to_string(),
            severity: Severity::Medium,
            applies_when: vec![Condition::DependencyAdded(true)],
            requires: vec!["license_check".to_string(), "vulnerability_check".to_string(), "maintenance_check".to_string(), "size_impact_review".to_string()],
            message: "New dependency added without required reviews (license, vulnerability, maintenance).".to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "auth_change_requires_security_review".to_string(),
            description: "Authentication changes require security review".to_string(),
            severity: Severity::High,
            applies_when: vec![Condition::ChangeTouches("auth".to_string())],
            requires: vec!["security_review".to_string()],
            message: "Authentication or authorization code changed without security review."
                .to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "no_secrets_in_logs".to_string(),
            description: "Changes touching logging must not expose secrets".to_string(),
            severity: Severity::Critical,
            applies_when: vec![Condition::ChangeTouches("logging".to_string())],
            requires: vec!["secret_scan".to_string()],
            message: "Logging code changed — verify no secrets or credentials are logged."
                .to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "feature_flag_for_critical_path".to_string(),
            description: "Critical path changes should use feature flags".to_string(),
            severity: Severity::Medium,
            applies_when: vec![Condition::ChangeTouches("critical_path".to_string())],
            requires: vec![
                "feature_flag".to_string(),
                "staged_rollout_plan".to_string(),
            ],
            message:
                "Critical path service modified without a feature flag or staged rollout plan."
                    .to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "test_coverage_for_new_code".to_string(),
            description: "New source files require corresponding tests".to_string(),
            severity: Severity::Low,
            applies_when: vec![Condition::TaskTypeIs(TaskType::Feature)],
            requires: vec!["tests_for_new_files".to_string()],
            message: "New code implemented without corresponding unit tests.".to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "artifact_quality_check".to_string(),
            description: "Generated web artifacts must enforce quality design and real physics"
                .to_string(),
            severity: Severity::Medium,
            applies_when: vec![Condition::FileMatches(".*\\.html$".to_string())],
            requires: vec![
                "responsive_controls".to_string(),
                "real_physics".to_string(),
            ],
            message:
                "HTML artifact generated without responsive controls or complete interactive logic."
                    .to_string(),
            enabled: true,
        });

        engine
    }

    /// Loads rules from a JSON file (`{"rules": [...]}`); YAML is not supported.
    pub fn load_from_file(path: &str) -> Result<Self, String> {
        let data = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read rules file {}: {}", path, e))?;
        #[derive(Deserialize)]
        struct RulesWrapper {
            rules: Vec<Rule>,
        }
        let wrapper: RulesWrapper = serde_json::from_str(&data)
            .map_err(|e| format!("Failed to parse rules file {}: {}", path, e))?;
        Ok(Self {
            rules: wrapper.rules,
        })
    }

    /// Adds a rule to the engine.
    pub fn add_rule(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// Evaluates all enabled rules against the given context and returns violations.
    pub fn evaluate(&self, ctx: &EvaluationContext) -> Vec<RuleViolation> {
        let mut violations = Vec::new();
        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }
            if let Some(violation) = self.evaluate_rule(rule, ctx) {
                violations.push(violation);
            }
        }
        violations
    }

    /// Evaluates a single rule against the context.
    pub fn evaluate_rule(&self, rule: &Rule, ctx: &EvaluationContext) -> Option<RuleViolation> {
        // 1. Check if rule applies
        let mut applies = false;
        for cond in &rule.applies_when {
            match cond {
                Condition::TaskTypeIs(tt) => {
                    if let Some(ref ctt) = ctx.task_type {
                        if ctt == tt {
                            applies = true;
                        }
                    }
                }
                Condition::ChangeTouches(keyword) => {
                    if ctx
                        .changed_files
                        .iter()
                        .chain(&ctx.tools_called)
                        .any(|f| name_touches(f, keyword))
                    {
                        applies = true;
                    }
                }
                Condition::FileMatches(pattern) => {
                    let pat = pattern.replace("**/*", "").replace("*", "");
                    if ctx.changed_files.iter().any(|f| f.contains(&pat)) {
                        applies = true;
                    }
                }
                Condition::DependencyAdded(expected) => {
                    if ctx.dependencies_added.is_empty() != *expected {
                        applies = true;
                    }
                }
                Condition::MigrationType(mtype) => {
                    if let Some(ref cmt) = ctx.migration_type {
                        if cmt.to_lowercase() == mtype.to_lowercase() {
                            applies = true;
                        }
                    }
                }
                Condition::Custom(key, val) => {
                    if let Some(cval) = ctx.custom_facts.get(key) {
                        if cval == val {
                            applies = true;
                        }
                    }
                }
            }
        }

        if !applies && !rule.applies_when.is_empty() {
            return None;
        }

        // 2. Check if requirements are satisfied
        let mut missing_reqs = Vec::new();
        for req in &rule.requires {
            let satisfied = match req.as_str() {
                "failing_test_before_fix" | "passing_test_after_fix" | "tests_for_new_files" => {
                    !ctx.tests_added.is_empty() || ctx.tests_run
                }
                "caller_search" => ctx
                    .tools_called
                    .iter()
                    .any(|t| t.contains("grep") || t.contains("search") || t.contains("find")),
                "changelog_entry" => {
                    ctx.has_changelog_entry
                        || ctx.changed_files.iter().any(|f| {
                            f.to_lowercase().contains("changelog")
                                || f.to_lowercase().contains("release_notes")
                        })
                }
                "rollback_plan" | "backup_plan" => ctx.has_rollback_plan,
                "security_review" | "secret_scan" => {
                    ctx.security_review_done || fact_true(&ctx.custom_facts, req)
                }
                "license_check"
                | "vulnerability_check"
                | "maintenance_check"
                | "size_impact_review" => ctx
                    .tools_called
                    .iter()
                    .any(|t| t.contains("search") || t.contains("web") || t.contains("fetch")),
                _ => {
                    // Check if custom fact claims it's satisfied
                    ctx.custom_facts
                        .get(req)
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                }
            };

            if !satisfied {
                missing_reqs.push(req.clone());
            }
        }

        if !missing_reqs.is_empty() {
            Some(RuleViolation {
                rule_id: rule.id.clone(),
                rule_description: rule.description.clone(),
                severity: rule.severity,
                message: format!(
                    "{} Missing required checks: {}",
                    rule.message,
                    missing_reqs.join(", ")
                ),
                evidence: Some(format!(
                    "Changed files: {:?}, Tools called: {:?}",
                    ctx.changed_files, ctx.tools_called
                )),
                suggested_action: Some(how_to_clear(&rule.id, &missing_reqs)),
            })
        } else {
            None
        }
    }

    /// The built-in rules with the user's overrides from `dir` (normally
    /// NEXUS_HOME/rules) applied, plus one (file name, error) per file that
    /// could not be used. The checkout cannot turn a rule off: the agent may
    /// write there, so a rule it fails could not be trusted to stay on.
    pub fn load_with_overrides(dir: &std::path::Path) -> (Self, Vec<(String, String)>) {
        let mut engine = Self::load_defaults();
        let mut failures = Vec::new();
        let Ok(rd) = fs::read_dir(dir) else {
            return (engine, failures);
        };
        let mut paths: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        for path in paths {
            if let Err(e) = engine.apply_overrides_file(&path) {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                failures.push((name, e));
            }
        }
        (engine, failures)
    }

    /// Applies `{"rules": [...]}` from `path`: an entry whose id names a
    /// loaded rule changes only the fields it gives (`{"id": …, "enabled":
    /// false}` turns it off); any other entry must be a whole rule and is
    /// added.
    pub fn apply_overrides_file(&mut self, path: &std::path::Path) -> Result<(), String> {
        let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let entries = v["rules"].as_array().ok_or("expected {\"rules\": [...]}")?;
        for entry in entries {
            let id = entry["id"].as_str().ok_or("a rule without an id")?;
            match self.rules.iter_mut().find(|r| r.id == id) {
                Some(rule) => {
                    let mut merged = serde_json::to_value(&*rule).map_err(|e| e.to_string())?;
                    for (k, val) in entry.as_object().into_iter().flatten() {
                        merged[k] = val.clone();
                    }
                    *rule = serde_json::from_value(merged).map_err(|e| format!("{id}: {e}"))?;
                }
                None => self.add_rule(
                    serde_json::from_value(entry.clone()).map_err(|e| format!("{id}: {e}"))?,
                ),
            }
        }
        Ok(())
    }

    /// Returns true if any High or Critical violations exist in the context.
    pub fn is_blocked(&self, ctx: &EvaluationContext) -> bool {
        self.evaluate(ctx)
            .iter()
            .any(|v| v.severity == Severity::Critical || v.severity == Severity::High)
    }

    /// Formats a list of violations into human-readable text.
    pub fn format_violations(violations: &[RuleViolation]) -> String {
        if violations.is_empty() {
            return "No rule violations detected.".to_string();
        }
        let mut out = String::from("### Rule Violations\n\n");
        for v in violations {
            out.push_str(&format!(
                "- **[{}]** `{}`: {}\n",
                v.severity.to_string().to_uppercase(),
                v.rule_id,
                v.message
            ));
            if let Some(ref act) = v.suggested_action {
                out.push_str(&format!("  - *Suggested Action*: {}\n", act));
            }
        }
        out
    }

    /// Returns violations formatted as a JSON Value.
    pub fn format_violations_json(violations: &[RuleViolation]) -> Value {
        serde_json::to_value(violations).unwrap_or(Value::Null)
    }
}

/// Environment variable naming required checks that were done outside the
/// run (`BWN_CHECKS_DONE=security_review`), for a pipeline whose own review
/// step covers them.
pub const CHECKS_DONE_ENV: &str = "BWN_CHECKS_DONE";

/// The checks `BWN_CHECKS_DONE` names, as facts for `EvaluationContext`.
pub fn checks_done_facts(raw: Option<&str>) -> HashMap<String, Value> {
    raw.unwrap_or("")
        .split([',', ' '])
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| (c.to_string(), Value::Bool(true)))
        .collect()
}

fn fact_true(facts: &HashMap<String, Value>, key: &str) -> bool {
    facts.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// How a person clears a violation: record the checks as done for the run,
// or turn the rule off in their own rules folder.
fn how_to_clear(rule_id: &str, missing: &[String]) -> String {
    let dir = crate::config::home().join("rules");
    format!(
        "If {} {} done, set {CHECKS_DONE_ENV}={} for the run; to turn this rule off, \
         put {{\"rules\": [{{\"id\": \"{rule_id}\", \"enabled\": false}}]}} in a .json file in {}",
        missing.join(", "),
        if missing.len() == 1 { "was" } else { "were" },
        missing.join(","),
        dir.display()
    )
}

// Words of a path or tool name: split at separators, dots, dashes,
// underscores and camelCase humps, lower-cased (`src/OAuthClient.ts` →
// src, o, auth, client, ts).
fn name_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let hump = c.is_uppercase()
            && !cur.is_empty()
            && (chars[i - 1].is_lowercase() || chars.get(i + 1).is_some_and(|n| n.is_lowercase()));
        if hump {
            words.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

// Endings that keep a word about the same thing (`authn`, `loggers`);
// `author` and `authors` are not among them.
const SAME_THING_ENDINGS: &[&str] = &["", "s", "n", "z", "ed", "er", "ers", "ing"];

// Starts of endings whose every form is the same thing: authenticate,
// authorize and authorise, however conjugated (`authorising`,
// `authenticates`, `authorizations`).
const SAME_THING_STEMS: &[&str] = &["entic", "oriz", "oris"];

fn word_is(word: &str, kw: &str) -> bool {
    let word = word.trim_end_matches(|c: char| c.is_ascii_digit());
    word.strip_prefix(kw)
        .is_some_and(|rest| {
            SAME_THING_ENDINGS.contains(&rest) || SAME_THING_STEMS.iter().any(|s| rest.starts_with(s))
        })
        // A compound ending in the keyword: `oauth`, `basicauth`.
        || (word.len() > kw.len() && word.ends_with(kw))
}

/// Does a changed path (or tool name) name `keyword`? Matched by whole words
/// of the name, so `AUTHORS.md` is not auth code and `logging/` is not a
/// login; a keyword of several words (`public_api`) needs them in order.
pub fn name_touches(name: &str, keyword: &str) -> bool {
    let kw = name_words(keyword);
    let words = name_words(name);
    if kw.is_empty() || words.len() < kw.len() {
        return false;
    }
    if let [k] = kw.as_slice() {
        return words.iter().any(|w| word_is(w, k));
    }
    words.windows(kw.len()).any(|w| w == kw.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_engine_defaults_and_evaluation() {
        let engine = RuleEngine::load_defaults();
        assert!(!engine.rules.is_empty());

        let ctx = EvaluationContext {
            task_type: Some(TaskType::BugFix),
            tests_added: vec![],
            tests_run: false,
            ..Default::default()
        };
        let violations = engine.evaluate(&ctx);
        assert!(violations
            .iter()
            .any(|v| v.rule_id == "bug_fix_requires_regression_test"));
    }

    #[test]
    fn change_touches_matches_path_names_not_substrings() {
        for p in [
            "AUTHORS.md",
            "docs/author.py",
            "src/logging/setup.py",
            "authority.txt",
        ] {
            assert!(!name_touches(p, "auth"), "{p}");
        }
        for p in [
            "src/auth/login.py",
            "auth.py",
            "lib/oauth_client.py",
            "src/OAuthClient.ts",
            "middleware/authn.go",
            "app/authorization.rb",
            "oauth2/provider.py",
        ] {
            assert!(name_touches(p, "auth"), "{p}");
        }
        // Every spelling and form of authenticate and authorize still counts,
        // as it did when any substring did.
        for p in [
            "src/authorisation.py",
            "lib/authorise.rb",
            "auth/authenticating.go",
            "src/authorizing_middleware.ts",
            "policies/Authorizes.java",
            "app/authentications.py",
            "app/authorisers.py",
        ] {
            assert!(name_touches(p, "auth"), "{p}");
        }
        assert!(!name_touches("docs/authorship.md", "auth"));
        assert!(!name_touches("src/authoritative_dns.rs", "auth"));
        assert!(name_touches("src/logging/setup.py", "logging"));
        assert!(name_touches("api/public_api.rs", "public_api"));
        assert!(!name_touches("api/public.rs", "public_api"));
        assert!(!name_touches("republic/api.rs", "public_api"));
    }

    #[test]
    fn an_authors_edit_does_not_trip_the_auth_rule_and_auth_code_does() {
        let engine = RuleEngine::load_defaults();
        let ctx = |f: &str| EvaluationContext {
            changed_files: vec![f.to_string()],
            ..Default::default()
        };
        let fired = |f: &str| {
            engine
                .evaluate(&ctx(f))
                .into_iter()
                .find(|v| v.rule_id == "auth_change_requires_security_review")
        };
        assert!(fired("AUTHORS.md").is_none());
        let v = fired("src/auth/login.py").expect("auth rule");
        let act = v.suggested_action.unwrap();
        assert!(act.contains("BWN_CHECKS_DONE=security_review"), "{act}");
        assert!(act.contains("\"enabled\": false"), "{act}");
        // Recorded as done for the run: the rule is satisfied.
        let done = EvaluationContext {
            changed_files: vec!["src/auth/login.py".into()],
            custom_facts: checks_done_facts(Some("security_review, changelog_entry")),
            ..Default::default()
        };
        assert!(!engine
            .evaluate(&done)
            .iter()
            .any(|v| v.rule_id == "auth_change_requires_security_review"));
    }

    #[test]
    fn user_overrides_turn_a_rule_off() {
        let dir = std::env::temp_dir().join(format!("bwn-rules-ovr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("off.json"),
            r#"{"rules": [{"id": "auth_change_requires_security_review", "enabled": false}]}"#,
        )
        .unwrap();
        fs::write(dir.join("broken.json"), "{").unwrap();
        let (engine, failures) = RuleEngine::load_with_overrides(&dir);
        let auth = engine
            .rules
            .iter()
            .find(|r| r.id == "auth_change_requires_security_review")
            .unwrap();
        assert!(!auth.enabled);
        assert_eq!(auth.severity, Severity::High, "untouched fields stay");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "broken.json");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_file_is_json_only_and_names_the_file() {
        let dir = std::env::temp_dir().join(format!("bwn-rules-load-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let yaml = dir.join("rules.yaml");
        fs::write(&yaml, "rules:\n  - id: nope\n").unwrap();
        let err = RuleEngine::load_from_file(&yaml.to_string_lossy()).unwrap_err();
        assert!(err.contains("rules.yaml"), "{err}");
        assert!(err.starts_with("Failed to parse rules file"), "{err}");
        let missing = dir.join("missing.json");
        let err = RuleEngine::load_from_file(&missing.to_string_lossy()).unwrap_err();
        assert!(err.starts_with("Failed to read rules file"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
