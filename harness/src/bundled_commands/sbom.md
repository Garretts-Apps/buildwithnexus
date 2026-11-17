---
description: Produce a software bill of materials from manifests
---

# SBOM

Scope: $ARGUMENTS

1. Read lockfiles and manifests. List direct dependencies with versions and licenses when present.
2. Prefer CycloneDX or SPDX JSON if the repo already uses one.
3. Write under `docs/sbom/` and note how to regenerate.
4. Do not publish externally unless asked.