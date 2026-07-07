# Spec Writing

Use this skill when the user asks for a product spec, implementation plan, technical design, PRD, RFC, acceptance criteria, or issue-ready task breakdown.

## Workflow

1. Use `read_file`, `read_many_files`, `list_tree`, and `grep_files` to understand the existing product and architecture.
2. Use `todo_write` for multi-section specs.
3. Write specs with clear sections: problem, goals, non-goals, users, behavior, UX/API contract, data model, rollout, risks, tests, open questions.
4. Use `write_file` for Markdown specs in `docs/`, `.buildwithnexus/`, or the repo’s established planning location.
5. Use `create_docx` when the user asks for Word/docx output.

## Quality bar

- Make requirements testable.
- Separate decisions from open questions.
- Include concrete acceptance criteria.
- Name migration and compatibility risks.
- Keep implementation details aligned with the codebase, not generic architecture advice.
