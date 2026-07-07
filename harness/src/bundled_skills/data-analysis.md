# Data Analysis

Use this skill for CSV/JSON/log analysis, metrics summaries, benchmark interpretation, or report generation.

## Workflow

1. Use `file_info`, `read_file`, and `read_many_files` to inspect inputs.
2. Use `run_command` with existing tools such as `jq`, `awk`, `python3`, or project scripts when available.
3. Use `write_file` for derived CSV/JSON/Markdown outputs.
4. Use `create_docx` for a Word-ready summary when requested.

## Standards

- Preserve raw data; write derived outputs separately.
- Explain assumptions and missing data.
- Include reproducible commands or scripts when analysis is non-trivial.
