use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;

/// Software engineering primitives represented in the knowledge base.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Function,
    Class,
    Module,
    Package,
    Service,
    Endpoint,
    DatabaseTable,
    Migration,
    Test,
    BuildArtifact,
    Dependency,
    Configuration,
    EnvironmentVariable,
    Secret,
    Permission,
    Interface,
    Contract,
    ArchitectureDecision,
}

impl fmt::Display for EntityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Function => "function",
            Self::Class => "class",
            Self::Module => "module",
            Self::Package => "package",
            Self::Service => "service",
            Self::Endpoint => "endpoint",
            Self::DatabaseTable => "database_table",
            Self::Migration => "migration",
            Self::Test => "test",
            Self::BuildArtifact => "build_artifact",
            Self::Dependency => "dependency",
            Self::Configuration => "configuration",
            Self::EnvironmentVariable => "environment_variable",
            Self::Secret => "secret",
            Self::Permission => "permission",
            Self::Interface => "interface",
            Self::Contract => "contract",
            Self::ArchitectureDecision => "architecture_decision",
        };
        write!(f, "{}", s)
    }
}

/// Relations between software engineering entities.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    DependsOn,
    Owns,
    Calls,
    Implements,
    Contains,
    Tests,
    Deploys,
    Configures,
    Documents,
    Extends,
    Overrides,
    Produces,
    Consumes,
}

impl fmt::Display for RelationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::DependsOn => "depends_on",
            Self::Owns => "owns",
            Self::Calls => "calls",
            Self::Implements => "implements",
            Self::Contains => "contains",
            Self::Tests => "tests",
            Self::Deploys => "deploys",
            Self::Configures => "configures",
            Self::Documents => "documents",
            Self::Extends => "extends",
            Self::Overrides => "overrides",
            Self::Produces => "produces",
            Self::Consumes => "consumes",
        };
        write!(f, "{}", s)
    }
}

/// A directed relationship from one entity to another.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Relationship {
    pub target_id: String,
    pub relation: RelationType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Represents a known entity in the codebase or system architecture.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Entity {
    pub id: String,
    pub entity_type: EntityType,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
    pub last_updated: String,
}

/// Risk level for decision support judgment primitives.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

/// Cost level for decision support judgment primitives.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CostLevel {
    Low,
    Medium,
    High,
}

/// Reversibility level for decision support judgment primitives.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReversibilityLevel {
    Easy,
    Moderate,
    Hard,
    Irreversible,
}

/// Judgment primitives used to evaluate operational decisions and tradeoffs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct JudgmentPrimitive {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reversibility: Option<ReversibilityLevel>,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operational_impact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_impact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_impact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintainability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_to_implement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_impact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blast_radius: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compliance_concern: Option<String>,
}

fn default_confidence() -> f64 {
    1.0
}

/// Confidence level classification for answers and recommendations.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    High,
    Medium,
    Low,
    Blocked,
}

impl fmt::Display for ConfidenceLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let desc = match self {
            Self::High => "High confidence: Supported by code, tests, docs, and rules.",
            Self::Medium => "Medium confidence: Supported by partial evidence with reasonable assumptions.",
            Self::Low => "Low confidence: Based mostly on inference, incomplete context, or ambiguous requirements.",
            Self::Blocked => "Blocked: Cannot safely recommend or modify without missing required evidence.",
        };
        write!(f, "{}", desc)
    }
}

/// Records a piece of evidence inspected during reasoning or decision making.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EvidenceRecord {
    pub source: String,
    pub content: String,
    pub timestamp: String,
    pub confidence: f64,
    #[serde(default)]
    pub relevant_entities: Vec<String>,
}

/// The local project knowledge base storing structured entities and relationships.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct KnowledgeBase {
    pub entities: HashMap<String, Entity>,
    #[serde(skip)]
    workdir: PathBuf,
}

impl KnowledgeBase {
    /// Creates or loads a KnowledgeBase from `.buildwithnexus/knowledge/entities.json`.
    pub fn new(workdir: &str) -> Self {
        let root = PathBuf::from(workdir);
        let path = root
            .join(".buildwithnexus")
            .join("knowledge")
            .join("entities.json");

        let mut kb = if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|data| serde_json::from_str::<KnowledgeBase>(&data).ok())
                .unwrap_or_else(KnowledgeBase::default)
        } else {
            KnowledgeBase::default()
        };
        kb.workdir = root;
        kb
    }

    /// Adds or updates an entity in the knowledge base.
    pub fn add_entity(&mut self, entity: Entity) {
        self.entities.insert(entity.id.clone(), entity);
    }

    /// Removes an entity by ID.
    pub fn remove_entity(&mut self, id: &str) -> Option<Entity> {
        self.entities.remove(id)
    }

    /// Gets an entity by ID.
    pub fn get_entity(&self, id: &str) -> Option<&Entity> {
        self.entities.get(id)
    }

    /// Searches entities by name, type, or description (case-insensitive substring match).
    pub fn search(&self, query: &str) -> Vec<&Entity> {
        let q = query.to_lowercase();
        self.entities
            .values()
            .filter(|e| {
                e.name.to_lowercase().contains(&q)
                    || e.id.to_lowercase().contains(&q)
                    || e.entity_type.to_string().to_lowercase().contains(&q)
                    || e.description
                        .as_ref()
                        .is_some_and(|d| d.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// Finds all entities of a given EntityType.
    pub fn find_by_type(&self, entity_type: EntityType) -> Vec<&Entity> {
        self.entities
            .values()
            .filter(|e| e.entity_type == entity_type)
            .collect()
    }

    /// Finds all entities related to the given entity ID (outgoing relationships).
    pub fn find_related(&self, id: &str) -> Vec<(&Entity, &Relationship)> {
        let mut result = Vec::new();
        if let Some(source) = self.entities.get(id) {
            for rel in &source.relationships {
                if let Some(target) = self.entities.get(&rel.target_id) {
                    result.push((target, rel));
                }
            }
        }
        result
    }

    /// Persists the knowledge base to `.buildwithnexus/knowledge/entities.json`.
    pub fn save(&self) -> Result<(), String> {
        let dir = self.workdir.join(".buildwithnexus").join("knowledge");
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create knowledge dir: {}", e))?;
        let path = dir.join("entities.json");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize knowledge base: {}", e))?;
        fs::write(&path, json).map_err(|e| format!("Failed to write knowledge base: {}", e))?;
        Ok(())
    }

    /// Parses symbol output (e.g. from tree-sitter or ctags formatted as JSON) and populates entities.
    /// Expects a JSON array of objects with fields: name, kind (function/class/etc.), path, description.
    pub fn index_from_tree_sitter_output(&mut self, symbols_json: &str) -> Result<usize, String> {
        let items: Vec<Value> = serde_json::from_str(symbols_json)
            .map_err(|e| format!("Invalid JSON symbol output: {}", e))?;

        let mut count = 0;
        let now = chrono_now_iso();

        for item in items {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let kind_str = item
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("function");
            let path = item
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let desc = item
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let entity_type = match kind_str.to_lowercase().as_str() {
                "class" | "struct" => EntityType::Class,
                "module" | "mod" => EntityType::Module,
                "package" => EntityType::Package,
                "service" => EntityType::Service,
                "endpoint" | "route" => EntityType::Endpoint,
                "interface" | "trait" => EntityType::Interface,
                "test" => EntityType::Test,
                _ => EntityType::Function,
            };

            let id = format!("{}:{}", entity_type, name);
            let entity = Entity {
                id,
                entity_type,
                name: name.to_string(),
                path,
                description: desc,
                metadata: item.clone(),
                relationships: Vec::new(),
                last_updated: now.clone(),
            };
            self.add_entity(entity);
            count += 1;
        }

        Ok(count)
    }

    /// Generates a structured Markdown context summary for injection into LLM prompts.
    ///
    /// Selection and ordering are deterministic (curated descriptions first,
    /// then by name) so the injected block is byte-stable across processes and
    /// keeps provider prompt-cache prefixes warm.
    pub fn generate_context_summary(&self, relevant_ids: &[String]) -> String {
        if relevant_ids.is_empty() && self.entities.is_empty() {
            return "No structured project knowledge available.".to_string();
        }

        let mut out = String::from("### Structured Project Knowledge\n\n");
        let ids: Vec<&String> = if relevant_ids.is_empty() {
            // HashMap iteration order is randomized per instance, which would
            // inject a different symbol sample into every system prompt and
            // break prompt-cache byte stability. Sort so the sampled subset
            // (and its order) is identical run to run, preferring entities
            // with real descriptions over auto-extracted placeholder noise.
            let mut all: Vec<&Entity> = self.entities.values().collect();
            all.sort_by(|a, b| {
                has_curated_description(b)
                    .cmp(&has_curated_description(a))
                    .then_with(|| a.name.cmp(&b.name))
                    .then_with(|| a.id.cmp(&b.id))
            });
            all.into_iter().take(20).map(|e| &e.id).collect()
        } else {
            relevant_ids.iter().collect()
        };

        for id in ids {
            if let Some(e) = self.entities.get(id) {
                out.push_str(&format!(
                    "- **{}** (`{}`): {}\n",
                    e.name, e.entity_type, e.id
                ));
                if let Some(ref p) = e.path {
                    out.push_str(&format!("  - Path: {}\n", p));
                }
                if let Some(ref d) = e.description {
                    out.push_str(&format!("  - Description: {}\n", d));
                }
                if !e.relationships.is_empty() {
                    out.push_str("  - Relationships:\n");
                    for rel in &e.relationships {
                        out.push_str(&format!("    - {} -> {}\n", rel.relation, rel.target_id));
                    }
                }
            }
        }
        out
    }
}

/// True when an entity carries a curated description rather than the
/// auto-extracted "Extracted symbol from ..." placeholder produced by
/// indexing, so context summaries surface real knowledge first.
fn has_curated_description(e: &Entity) -> bool {
    e.description
        .as_deref()
        .is_some_and(|d| !d.trim().is_empty() && !d.starts_with("Extracted symbol from"))
}

/// Current UTC time as an ISO 8601 string (e.g. "2026-07-06T08:00:00Z"),
/// derived from the system clock without pulling in a date-time crate.
pub fn chrono_now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    iso_from_epoch(secs)
}

// Civil-from-days conversion (Howard Hinnant's algorithm); exact for the whole
// unix era, so no leap-year edge cases to worry about.
fn iso_from_epoch(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, min, s) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_knowledge_base_crud() {
        let mut kb = KnowledgeBase::default();
        let entity = Entity {
            id: "function:test_fn".to_string(),
            entity_type: EntityType::Function,
            name: "test_fn".to_string(),
            path: Some("src/main.rs".to_string()),
            description: Some("A test function".to_string()),
            metadata: Value::Null,
            relationships: vec![],
            last_updated: "2026-07-06T08:00:00Z".to_string(),
        };
        kb.add_entity(entity.clone());
        assert_eq!(kb.get_entity("function:test_fn"), Some(&entity));
        assert_eq!(kb.search("test_fn").len(), 1);
        assert_eq!(kb.find_by_type(EntityType::Function).len(), 1);
        assert!(kb.remove_entity("function:test_fn").is_some());
        assert_eq!(kb.get_entity("function:test_fn"), None);
    }

    #[test]
    fn test_generate_context_summary_is_deterministic_and_prefers_descriptions() {
        fn entity(id: &str, name: &str, desc: Option<&str>) -> Entity {
            Entity {
                id: id.to_string(),
                entity_type: EntityType::Function,
                name: name.to_string(),
                path: Some("src/lib.rs".to_string()),
                description: desc.map(|d| d.to_string()),
                metadata: Value::Null,
                relationships: vec![],
                last_updated: "2026-07-06T08:00:00Z".to_string(),
            }
        }
        let items = [
            (
                "function:zeta",
                "zeta",
                Some("Extracted symbol from src/lib.rs"),
            ),
            ("function:mid", "mid", None),
            ("function:alpha", "alpha", Some("Parses config files")),
            (
                "function:beta",
                "beta",
                Some("Extracted symbol from src/lib.rs"),
            ),
        ];
        // Two knowledge bases with the same contents but opposite insertion
        // order: separate HashMap instances hash differently, so this fails
        // whenever selection leans on map iteration order.
        let mut kb1 = KnowledgeBase::default();
        for (id, name, desc) in items {
            kb1.add_entity(entity(id, name, desc));
        }
        let mut kb2 = KnowledgeBase::default();
        for (id, name, desc) in items.into_iter().rev() {
            kb2.add_entity(entity(id, name, desc));
        }
        let s1 = kb1.generate_context_summary(&[]);
        assert_eq!(s1, kb1.generate_context_summary(&[]));
        assert_eq!(s1, kb2.generate_context_summary(&[]));
        // The curated description sorts ahead of auto-extracted placeholders.
        let alpha = s1.find("**alpha**").expect("alpha listed");
        let beta = s1.find("**beta**").expect("beta listed");
        assert!(alpha < beta, "curated entity should be listed first:\n{s1}");
    }

    #[test]
    fn test_iso_from_epoch_known_values() {
        assert_eq!(iso_from_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_epoch(1_735_689_600), "2025-01-01T00:00:00Z");
        // Leap day, with a non-zero time-of-day component.
        assert_eq!(iso_from_epoch(1_709_209_530), "2024-02-29T12:25:30Z");
    }

    #[test]
    fn test_chrono_now_iso_is_clock_derived() {
        let now = chrono_now_iso();
        assert_eq!(now.len(), 20);
        assert!(now.ends_with('Z'));
        // Sanity: the year comes from the system clock, not a constant.
        let year: i32 = now[..4].parse().unwrap();
        assert!(year >= 2024);
    }
}
