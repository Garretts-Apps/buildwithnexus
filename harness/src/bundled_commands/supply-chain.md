---
description: Review dependency provenance, pinning, and SBOM hygiene
---

# Supply chain

Scope: $ARGUMENTS

1. Check lockfiles are committed, versions are pinned, and install scripts are not running remote curls.
2. Compare against the last SBOM if one exists (`/sbom` to generate). Note license gaps you can see from manifests only.
3. Cross-check `/deps-audit` results when audits were not run yet.
4. Output a short posture table: pinned, audited, SBOM present, CI gates present. No vendor-specific cloud assumptions.