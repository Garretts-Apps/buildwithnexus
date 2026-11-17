#!/usr/bin/env python3
"""UserPromptSubmit: warn when the prompt may contain a credential. Exit 2 to block."""
import json
import re
import sys

PATTERNS = [
    (r"\bsk-[A-Za-z0-9]{20,}\b", "API key shape (sk-)"),
    (r"\bAKIA[0-9A-Z]{16}\b", "AWS access key id"),
    (r"\bghp_[A-Za-z0-9]{20,}\b", "GitHub token"),
    (r"\bgho_[A-Za-z0-9]{20,}\b", "GitHub OAuth token"),
    (r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b", "Slack token shape"),
    (r"(?i)-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----", "private key block"),
    (r"(?i)\b(password|api[_-]?key|secret|token)\s*[:=]\s*\S{8,}", "labeled secret"),
]


def deny(msg):
    print(msg, file=sys.stderr)
    sys.exit(2)


def main():
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)
    prompt = payload.get("prompt") or ""
    if not prompt.strip():
        sys.exit(0)
    for pat, label in PATTERNS:
        if re.search(pat, prompt):
            deny(f"prompt may contain a credential ({label}); remove it and use env or a secret store")
    sys.exit(0)


if __name__ == "__main__":
    main()