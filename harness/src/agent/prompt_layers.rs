//! The system prompt as labeled pieces. `context_prefix` joins the same
//! strings it always sent; `/system-prompt` is what reads the labels.

use std::path::Path;

use super::{
    discovery_policy, env_snapshot, persona, role, skill_manifest, tool_manifest,
    visible_reasoning_policy,
};

/// A window this small drops the long sections, matching `context_prefix`.
pub(crate) const COMPACT_CONTEXT: usize = 16_384;

pub(crate) const PLAN_CONTRACT: &str = "You are bwn in PLAN mode: an expert software architect and technical lead working out an implementation plan with the user. \
        You have full read access to inspect the codebase as needed. Do not write files, edit files, apply patches, spawn subagents, or run mutating shell commands while planning. \
        When the user gives you a high-level or underspecified task (or when key choices like tech stack, exit conditions, or edge case handling are ambiguous): \
        1. Interrogate the user to resolve design decisions, edge cases, tech stack choices, and requirements. Use the `question` tool or ask clear targeted questions before finalizing the plan. \
        2. Once requirements and edge cases are clear, call exit_plan or ExitPlanMode with a concise 3-to-5 step implementation plan describing concrete human implementation steps (e.g. 1. Create main.py with stdin loop 2. Handle Ctrl+C and exit keywords 3. Verify with python main.py). \
        Do not list raw tool names (like read_file or write_file) as plan steps. \
        If the user makes small talk or greets you, reply naturally without forcing a plan.";

pub(crate) const BRAINSTORM_CONTRACT: &str = "You are a sharp, concise thought partner with read access to the codebase and the internet. \
        Use tools freely to look things up, read files, grep for patterns, or run read-only commands — \
        whatever helps the conversation. \
        This mode is read-only: do not write or edit files, apply patches, spawn subagents, or run mutating shell commands. \
        If the user wants changes made, say so and suggest switching modes. \
        When you think the user is ready to stop discussing and start building or planning, \
        end your response with the exact token [SUGGEST:BUILD] or [SUGGEST:PLAN] on its own line. \
        Otherwise just respond naturally. No fluff.";

pub(crate) const CHAT_CONTRACT: &str = "You are buildwithnexus in a coding terminal. Answer the user's current message naturally and concisely. \
        If the user asks a normal conversational question or greeting, answer in plain text and do not call tools. \
        If answering well requires inspecting the workspace or environment, use tools, then summarize the result. \
        Do not emit JSON unless a tool call is actually required by the tool protocol.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptMode {
    Build,
    Plan,
    Brainstorm,
    Chat,
}

impl PromptMode {
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "build" => Ok(Self::Build),
            "plan" => Ok(Self::Plan),
            "brainstorm" => Ok(Self::Brainstorm),
            "chat" => Ok(Self::Chat),
            other => Err(format!(
                "unknown mode {other} - use build, plan, brainstorm or chat"
            )),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Plan => "plan",
            Self::Brainstorm => "brainstorm",
            Self::Chat => "chat",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Build => "BUILD",
            Self::Plan => "PLAN",
            Self::Brainstorm => "BRAINSTORM",
            Self::Chat => "chat",
        }
    }
}

pub(crate) struct PromptPiece {
    pub layer: &'static str,
    pub source: String,
    pub text: String,
}

pub(crate) fn join_pieces(pieces: &[PromptPiece]) -> String {
    let body = pieces
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{body}\n\n")
}

pub(crate) fn mode_contract(mode: PromptMode) -> &'static str {
    match mode {
        PromptMode::Build => role("engineer").system,
        PromptMode::Plan => PLAN_CONTRACT,
        PromptMode::Brainstorm => BRAINSTORM_CONTRACT,
        PromptMode::Chat => CHAT_CONTRACT,
    }
}

/// Mode contract, then every section `context_prefix` sends, in that order.
pub(crate) fn effective_pieces(
    cwd: &Path,
    mode: PromptMode,
    context_tokens: usize,
) -> Vec<PromptPiece> {
    let mut pieces = vec![PromptPiece {
        layer: "mode",
        source: format!("built-in ({})", mode.label()),
        text: mode_contract(mode).to_string(),
    }];
    pieces.extend(context_pieces(cwd, context_tokens));
    pieces
}

#[cfg(test)]
pub(crate) fn effective_system(cwd: &Path, mode: PromptMode, context_tokens: usize) -> String {
    join_pieces(&effective_pieces(cwd, mode, context_tokens))
}

pub(crate) fn context_pieces(cwd: &Path, context_tokens: usize) -> Vec<PromptPiece> {
    let compact = context_tokens <= COMPACT_CONTEXT;
    let mut parts = Vec::new();

    // Identity leads every prompt. It is short and always worth the tokens.
    push(&mut parts, "built-in base", "built-in", persona());

    let home_str = crate::config::user_home()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    let mut path_info = format!(
        "[Environment Paths]\n\
         Workspace (CWD): {}\n\
         User Home (~): {}\n",
        cwd.display(),
        home_str
    );
    if cwd.to_string_lossy().contains("/tmp/") || cwd.to_string_lossy().contains("/private/tmp/") {
        path_info.push_str(
            "Note: The current workspace is running in a temporary directory/mount. \
             To find personal folders or files belonging to the user, you must search starting from the User Home (~).\n",
        );
    }
    push(&mut parts, "environment", "built-in", path_info);

    // The tool manifest is what tells the model the built-ins exist.
    push(&mut parts, "built-in tools", "built-in", tool_manifest());
    if !compact {
        push(
            &mut parts,
            "visible reasoning",
            "built-in",
            visible_reasoning_policy(),
        );
        push(
            &mut parts,
            "filesystem discovery",
            "built-in",
            discovery_policy(),
        );
        push(&mut parts, "skill policy", "built-in", skill_manifest());
    }

    // Probe which common system tools are present. Skip for small contexts.
    if !compact {
        let env_snap = env_snapshot();
        if !env_snap.is_empty() {
            push(
                &mut parts,
                "environment tools",
                "probed on this machine",
                format!("[Environment — tools already installed]\n{env_snap}"),
            );
        }
    }

    if let Some(section) = crate::workdirs::prompt_section(cwd, compact) {
        push(&mut parts, "added folders", "added with --add-dir", section);
    }

    // Project instructions come before memory and the Agents.md roles.
    let instructions = crate::config::prompt_instructions(cwd);
    if let Some(text) = crate::config::instructions_prompt(&instructions) {
        crate::trace::record_visible(
            "instructions",
            format!("loaded {} instruction file(s)", instructions.len()),
            serde_json::json!({
                "files": instructions.iter().map(|f| f.label.clone()).collect::<Vec<_>>(),
                "bytes": text.len(),
            }),
        );
        let text = if compact && text.len() > 2_000 {
            format!("{}…", crate::config::cut_at_char_boundary(&text, 2_000))
        } else {
            text
        };
        let source = instructions
            .iter()
            .map(|f| f.label.clone())
            .collect::<Vec<_>>()
            .join(", ");
        push(&mut parts, "instructions", &source, text);
    }

    if let Some(mem) = crate::config::load_memory() {
        let mem_text = if compact && mem.len() > 300 {
            format!("{}…", crate::config::cut_at_char_boundary(&mem, 300))
        } else {
            mem
        };
        push(
            &mut parts,
            "memory",
            &crate::config::memory_path().display().to_string(),
            format!("[Memory from previous sessions]\n{mem_text}"),
        );
    }
    if let Some(agents) = crate::config::load_agents() {
        crate::trace::record_visible(
            "agents",
            "loaded Agents.md",
            serde_json::json!({"bytes": agents.len(), "preview": crate::trace::preview(&agents, 600)}),
        );
        let agents_text = if compact && agents.len() > 500 {
            format!("{}…", crate::config::cut_at_char_boundary(&agents, 500))
        } else {
            agents
        };
        push(
            &mut parts,
            "agent definitions",
            &agents_source(cwd),
            format!("[Agent knowledge — Agents.md]\n{agents_text}"),
        );
    }

    let (user_sys, project_sys) = crate::config::load_system_prompts(cwd);
    if let Some(sys_prompt) = user_sys {
        push(
            &mut parts,
            "user override",
            &crate::config::home()
                .join("system.md")
                .display()
                .to_string(),
            format!("[Custom User System Prompt — system.md]\n{sys_prompt}"),
        );
    }
    if let Some(sys_prompt) = project_sys {
        push(
            &mut parts,
            "project override",
            &cwd.join(".buildwithnexus")
                .join("system.md")
                .display()
                .to_string(),
            format!("[Project System Prompt — .buildwithnexus/system.md]\n{sys_prompt}"),
        );
    }

    // Rules, knowledge, hooks and skill descriptions are the long sections.
    if !compact {
        push(
            &mut parts,
            "operational rules",
            "built-in",
            "[Operational rules]\nProject engineering rules are enforced by a separate \
             verifier pass after you finish.\nViolations are reported back to you to address."
                .to_string(),
        );

        let kb = crate::knowledge::KnowledgeBase::new(".");
        if !kb.entities.is_empty() {
            push(
                &mut parts,
                "knowledge",
                ".buildwithnexus/knowledge/entities.json",
                format!(
                    "[Structured Knowledge Base — Known Project Entities]\n{}",
                    kb.generate_context_summary(&[])
                ),
            );
        }

        let active_hooks = crate::hooks::list_active();
        if !active_hooks.is_empty() {
            push(
                &mut parts,
                "hooks",
                "settings hooks",
                format!("[Active Hooks]\n{}", active_hooks.join("\n")),
            );
        }
        let skills = crate::config::discover_skills(cwd);
        if !skills.is_empty() {
            let joined = skills
                .iter()
                .map(|s| format!("• /{} — {}", s.name, s.description_or_default()))
                .collect::<Vec<_>>()
                .join("\n");
            let source = skills
                .iter()
                .map(skill_source)
                .collect::<Vec<_>>()
                .join(", ");
            push(
                &mut parts,
                "skills",
                &source,
                format!(
                    "[Available skills — descriptions only]\n{joined}\n\n\
    To use a skill, call load_skill with the skill name to load its full instructions. \
    Only load skills that are directly relevant to the current task. \
    Do not load skills for simple greetings or casual replies."
                ),
            );
        }
    }

    parts
}

fn push(parts: &mut Vec<PromptPiece>, layer: &'static str, source: &str, text: String) {
    parts.push(PromptPiece {
        layer,
        source: source.to_string(),
        text,
    });
}

fn agents_source(cwd: &Path) -> String {
    let proj = cwd.join(".buildwithnexus").join("Agents.md");
    if crate::config::read_project_file(&proj, cwd).is_some_and(|t| !t.trim().is_empty()) {
        return proj.display().to_string();
    }
    crate::config::home()
        .join("Agents.md")
        .display()
        .to_string()
}

fn skill_source(skill: &crate::config::Skill) -> String {
    match &skill.dir {
        Some(dir) => format!(
            "{} ({}, {})",
            skill.name,
            skill.source.label(),
            dir.display()
        ),
        None => format!("{} ({})", skill.name, skill.source.label()),
    }
}
