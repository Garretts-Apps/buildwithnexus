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
            requires: vec!["failing_test_before_fix".to_string(), "passing_test_after_fix".to_string()],
            message: "Bug fix does not include a regression test.".to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "public_api_contract_change".to_string(),
            description: "Public API changes require caller analysis and changelog".to_string(),
            severity: Severity::High,
            applies_when: vec![Condition::ChangeTouches("public_api".to_string())],
            requires: vec!["caller_search".to_string(), "changelog_entry".to_string(), "compatibility_assessment".to_string()],
            message: "Public API contract changed without caller search or changelog entry.".to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "destructive_database_migration".to_string(),
            description: "Destructive migrations require rollback plan".to_string(),
            severity: Severity::Critical,
            applies_when: vec![Condition::MigrationType("destructive".to_string())],
            requires: vec!["rollback_plan".to_string(), "backup_plan".to_string(), "approval".to_string()],
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
            message: "Authentication or authorization code changed without security review.".to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "no_secrets_in_logs".to_string(),
            description: "Changes touching logging must not expose secrets".to_string(),
            severity: Severity::Critical,
            applies_when: vec![Condition::ChangeTouches("logging".to_string())],
            requires: vec!["secret_scan".to_string()],
            message: "Logging code changed — verify no secrets or credentials are logged.".to_string(),
            enabled: true,
        });

        engine.add_rule(Rule {
            id: "feature_flag_for_critical_path".to_string(),
            description: "Critical path changes should use feature flags".to_string(),
            severity: Severity::Medium,
            applies_when: vec![Condition::ChangeTouches("critical_path".to_string())],
            requires: vec!["feature_flag".to_string(), "staged_rollout_plan".to_string()],
            message: "Critical path service modified without a feature flag or staged rollout plan.".to_string(),
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

        engine
    }

    /// Loads rules from a JSON or YAML file (using JSON parser here for minimal deps).
    pub fn load_from_file(path: &str) -> Result<Self, String> {
        let data = fs::read_to_string(path).map_err(|e| format!("Failed to read rules file {}: {}", path, e))?;
        #[derive(Deserialize)]
        struct RulesWrapper {
            rules: Vec<Rule>,
        }
        let wrapper: RulesWrapper = serde_json::from_str(&data)
            .map_err(|e| format!("Failed to parse rules file {}: {}", path, e))?;
        Ok(Self { rules: wrapper.rules })
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
                        if ctt == tt { applies = true; }
                    }
                }
                Condition::ChangeTouches(keyword) => {
                    let kw = keyword.to_lowercase();
                    if ctx.changed_files.iter().any(|f| f.to_lowercase().contains(&kw))
                        || ctx.tools_called.iter().any(|t| t.to_lowercase().contains(&kw)) {
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
                    if !ctx.dependencies_added.is_empty() == *expected {
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
                        if cval == val { applies = true; }
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
                "caller_search" => {
                    ctx.tools_called.iter().any(|t| t.contains("grep") || t.contains("search") || t.contains("find"))
                }
                "changelog_entry" => {
                    ctx.has_changelog_entry || ctx.changed_files.iter().any(|f| f.to_lowercase().contains("changelog") || f.to_lowercase().contains("release_notes"))
                }
                "rollback_plan" | "backup_plan" => ctx.has_rollback_plan,
                "security_review" | "secret_scan" => ctx.security_review_done,
                "license_check" | "vulnerability_check" | "maintenance_check" | "size_impact_review" => {
                    ctx.tools_called.iter().any(|t| t.contains("search") || t.contains("web") || t.contains("fetch"))
                }
                _ => {
                    // Check if custom fact claims it's satisfied
                    ctx.custom_facts.get(req).and_then(|v| v.as_bool()).unwrap_or(false)
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
                message: format!("{} Missing required checks: {}", rule.message, missing_reqs.join(", ")),
                evidence: Some(format!("Changed files: {:?}, Tools called: {:?}", ctx.changed_files, ctx.tools_called)),
                suggested_action: Some(format!("Satisfy requirements: {}", missing_reqs.join(", "))),
            })
        } else {
            None
        }
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
            out.push_str(&format!("- **[{}]** `{}`: {}\n", v.severity.to_string().to_uppercase(), v.rule_id, v.message));
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
        assert!(violations.iter().any(|v| v.rule_id == "bug_fix_requires_regression_test"));
    }
}
