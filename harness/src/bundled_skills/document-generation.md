# Document Generation

Use this skill when creating `.docx`, Markdown reports, user guides, runbooks, handoff docs, release notes, or executive summaries.

## Tools

- Use `create_docx` for simple Word documents.
- Use `write_file` for Markdown, plain text, JSON, or CSV artifacts.
- Use `read_many_files` to gather source material.
- Use `fetch_url` or `web_search` only when the document needs current external facts.

## DOCX guidance

The built-in `create_docx` tool supports a title and markdown-like body:

- `# Heading`
- `## Heading`
- `### Heading`
- `- bullet`
- blank lines
- paragraphs

For complex layout, tables, images, tracked changes, or strict corporate templates, say that native output is basic and generate a Markdown source plus DOCX if useful.

## Writing standard

- Lead with the answer.
- Use short sections and concrete headings.
- Avoid invented citations. If factual claims depend on current information, use web tools and cite URLs in the document text.
