#!/usr/bin/env python3
"""
PreToolUse hook: Rule-based verification gate.

This hook receives tool call details on stdin as JSON and evaluates
engineering rules before allowing the tool to execute.

Install: Place in ~/.buildwithnexus/hooks/PreToolUse/rule_gate.py
         or reference in settings.json hooks config.

Exit codes:
  0 — continue (let the normal permission gate decide)
  2 — deny (block the tool call, rule violation)

Stdout (JSON):
  { "permissionDecision": "deny", "reason": "..." }  — block with reason
  { "permissionDecision": "allow" }                   — skip user prompt
  (empty or non-JSON)                                 — continue normally
"""

import json
import sys
import os
import re


def load_rules(workdir):
    """Load rules from .buildwithnexus/rules/default_rules.json or custom rules file."""
    rules_path = os.path.join(workdir, ".buildwithnexus", "rules", "default_rules.json")
    if not os.path.exists(rules_path):
        return []
    try:
        with open(rules_path, "r") as f:
            data = json.load(f)
            return data.get("rules", [])
    except (json.JSONDecodeError, IOError):
        return []


def check_sensitive_patterns(tool_name, tool_input):
    """Check for patterns that indicate sensitive operations."""
    violations = []

    if tool_name in ("write_file", "edit_file", "write", "edit", "str_replace_editor"):
        content = json.dumps(tool_input).lower()

        # Check for secrets in file content
        secret_patterns = [
            (r'(?:api[_-]?key|secret|token|password)\s*[:=]\s*["\'][^"\']{8,}', "Potential secret in file content"),
            (r'(?:aws_access_key|aws_secret)', "Potential AWS credential in file content"),
            (r'sk-[a-zA-Z0-9]{20,}', "Potential API key in file content"),
            (r'-----BEGIN (?:RSA |EC )?PRIVATE KEY-----', "Private key in file content"),
        ]

        for pattern, message in secret_patterns:
            if re.search(pattern, content, re.IGNORECASE):
                violations.append({
                    "rule": "no_secrets_in_code",
                    "severity": "critical",
                    "message": message
                })

        # Check for weakening validation
        file_path = tool_input.get("path", tool_input.get("file_path", ""))
        if any(kw in file_path.lower() for kw in ["valid", "sanitiz", "auth", "permission"]):
            # Flag for review — don't block, just warn
            violations.append({
                "rule": "validation_change_review",
                "severity": "medium",
                "message": f"Change touches validation/auth file: {file_path}. Verify no validation is weakened."
            })

    if tool_name in ("bash", "run_command"):
        cmd = tool_input.get("command", tool_input.get("cmd", ""))

        # Check for destructive database operations
        destructive_db_patterns = [
            (r'\b(?:DROP\s+(?:TABLE|DATABASE|INDEX|COLUMN))', "Destructive database operation"),
            (r'\b(?:TRUNCATE\s+TABLE)', "Destructive database operation"),
            (r'\b(?:DELETE\s+FROM\s+\w+\s*(?:;|$))', "Unbounded DELETE (no WHERE clause)"),
            (r'\balembic\s+downgrade\b', "Database migration downgrade"),
        ]

        for pattern, message in destructive_db_patterns:
            if re.search(pattern, cmd, re.IGNORECASE):
                violations.append({
                    "rule": "destructive_database_operation",
                    "severity": "critical",
                    "message": message + ". Requires rollback plan and explicit approval."
                })

    return violations


def check_dependency_additions(tool_name, tool_input):
    """Check if the tool call adds new dependencies."""
    violations = []

    if tool_name in ("write_file", "edit_file", "write", "edit"):
        file_path = tool_input.get("path", tool_input.get("file_path", ""))
        content = tool_input.get("content", tool_input.get("new_string", ""))

        dep_files = ["package.json", "Cargo.toml", "requirements.txt", "pyproject.toml",
                      "Gemfile", "go.mod", "pom.xml", "build.gradle"]

        if any(file_path.endswith(f) for f in dep_files):
            violations.append({
                "rule": "new_dependency_review",
                "severity": "medium",
                "message": f"Dependency file modified: {file_path}. Verify license, vulnerabilities, maintenance status, and size impact."
            })

    if tool_name in ("bash", "run_command"):
        cmd = tool_input.get("command", tool_input.get("cmd", ""))
        install_patterns = [
            r'\bnpm\s+install\b',
            r'\byarn\s+add\b',
            r'\bpip\s+install\b',
            r'\bcargo\s+add\b',
            r'\bgo\s+get\b',
            r'\bgem\s+install\b',
        ]
        for pattern in install_patterns:
            if re.search(pattern, cmd, re.IGNORECASE):
                violations.append({
                    "rule": "new_dependency_review",
                    "severity": "medium",
                    "message": f"Package install command detected. Verify license, vulnerabilities, and maintenance status."
                })
                break

    return violations


def main():
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, IOError):
        # Can't read input — let it through
        sys.exit(0)

    tool_name = payload.get("tool_name", payload.get("name", ""))
    tool_input = payload.get("tool_input", payload.get("input", {}))
    workdir = payload.get("cwd", os.getcwd())

    if isinstance(tool_input, str):
        try:
            tool_input = json.loads(tool_input)
        except json.JSONDecodeError:
            tool_input = {}

    all_violations = []
    all_violations.extend(check_sensitive_patterns(tool_name, tool_input))
    all_violations.extend(check_dependency_additions(tool_name, tool_input))

    critical = [v for v in all_violations if v.get("severity") == "critical"]

    if critical:
        # Block critical violations
        messages = "; ".join(v["message"] for v in critical)
        result = {
            "permissionDecision": "deny",
            "reason": f"[Rule Engine] BLOCKED — {messages}",
            "violations": all_violations
        }
        print(json.dumps(result))
        sys.exit(2)

    if all_violations:
        # Non-critical violations: warn but don't block
        messages = "; ".join(v["message"] for v in all_violations)
        # Print warnings to stderr so the user sees them
        print(f"[Rule Engine] ⚠ {messages}", file=sys.stderr)
        sys.exit(0)

    # No violations — continue normally
    sys.exit(0)


if __name__ == "__main__":
    main()
