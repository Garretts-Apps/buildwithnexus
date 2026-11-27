# Product mandate

The canonical product strategy for buildwithnexus (`bwn`): the philosophy, the architectural principles, the security invariants and the design constraints that every feature is judged against. It changes rarely and deliberately.

It is one of three documents:

| Document | Purpose |
|---|---|
| `docs/mandate.md` (this file) | Enduring product philosophy, architectural principles, security invariants and design constraints. |
| [`docs/competitor-analysis.md`](competitor-analysis.md) | A dated, evidence-based assessment of competitive positioning and capability gaps, refreshed periodically. |
| `docs/roadmap.md` | Sequenced implementation priorities, dependencies, release milestones and acceptance criteria, which change as capabilities ship and new evidence comes in. |

The roadmap follows the evidence in the competitor analysis and is measured against this mandate.

## Philosophy

bwn is a security-first, open-source AI coding harness for professional software engineering, combining modern agent capabilities with independently verifiable security, software quality and engineering integrity.

- **Security** is an architectural constraint.
- **Competitive feature parity** is a product requirement.
- **Engineering assurance** is a strategic differentiator.
- **Runtime efficiency and local-first operation** are design constraints.

## Vision

Build the most capable open-source, security-first AI coding harness for professional software engineering, with enforceable security boundaries, modern agent capabilities and continuous verification of software quality, security and maintainability.

## Core responsibilities

1. **Secure agent execution:** protect the developer, workstation, credentials, source, infrastructure and connected systems from unintended or malicious agent behaviour.
2. **Secure software development:** help produce software that is secure by design; find vulnerabilities, prevent regressions, and verify security throughout implementation.
3. **Engineering integrity over time:** keep the codebase maintainable, architecturally consistent and of high quality through repeated agent-driven changes.

## Five pillars

| Pillar | Objective |
|---|---|
| Execution security | Least privilege, isolation, credential protection, controlled network access. |
| Software security | Prevent, detect and remediate vulnerabilities throughout development. |
| Engineering integrity | Maintainability, architectural consistency and code quality across successive changes. |
| Agent capabilities | Feature parity with leading modern coding harnesses. |
| Runtime efficiency | Lightweight, fast, portable, economical. |

## Target architecture

This is the target, not a description of bwn today; the competitor analysis assesses how far the code is from it. Three layers. Orchestration operates within the constraints enforced by the two layers below it.

- **Agent orchestration:** models, planning, context, tools, subagents, memory, workflows.
- **Engineering assurance:** acceptance criteria, review, testing, security scanning, architectural rules, verification evidence.
- **Security enforcement:** sandboxing, permissions, isolation, credential protection, network policy, audit logging.

The model has no unilateral authority to waive security controls, redefine acceptance criteria, or declare failed verification successful.

## Target capabilities

- **Risk-aware planning:** scale planning and approval to change scope, sensitivity and reversibility.
- **Verification engine:** artifact-specific validation (tests, browsers, API requests, compilers, scanners).
- **Independent review:** in a separate agent context, against explicit acceptance criteria.
- **Quality gates:** deterministic checks before work is declared complete.
- **Path-based policies:** different requirements for production, experimental and test code.
- **Persistent learning:** recurring failures become durable, reviewable project rules and regression tests.
- **Architectural governance:** design decisions, dependency boundaries and established patterns are preserved.
- **Evidence collection:** decisions, test results, security findings and approvals are associated with changes.
- **Experimentation:** isolated worktrees, feature flags, measurable acceptance criteria.
- **Model-independent validation:** verification criteria stay outside the model's discretion.
- **Longitudinal benchmarking:** security, maintainability and correctness measured over successive tasks.

Secure-development orchestration covers SAST, SCA, secret scanning, supply-chain and provenance checks, threat modelling in planning, security regression tests, infrastructure scanning, project security policy, SBOMs and security evidence. It is integrated into the agent lifecycle, not offered only as optional commands.

## Engineering principles

- **Closed loops:** forward pressure (requests, incidents) and backpressure (automated review, tests, CI, linters, verification).
- **Maintainability over time,** not one-shot success.
- **Planning scaled to the expected cost of a change.**
- **Visual alignment:** plans and results as rendered artifacts.
- **Agents verify their own work with the right tool for each artifact.**
- **Recurring defects become deterministic checks.**
- **Throwaway code only where labelled;** production held to the bar.
- **Experiments behind feature flags,** measured.
- **Lessons compound across sessions.**
- **Decisions and the "why" live with the code.**
- **The project owns its verification,** and it is portable across models.

## Enforcement levels

| Level | Name | Meaning |
|---|---|---|
| 0 | Absent | No native capability. |
| 1 | Advisory | Instructions encourage the model to comply. |
| 2 | Assisted | Dedicated tools support compliance; the model can bypass them. |
| 3 | Enforced | The harness independently requires compliance. |
| 4 | Verifiable | Enforcement is independently testable, auditable and reproducible. |

Level 3 or 4 is valid only within an explicit threat model and execution scope, scored per surface (shell commands per sandbox backend, file tools, fetch and browser, MCP, hooks, subagents, plugins and skills, model requests), never as one blanket score.

Each guarantee states:

- protected assets;
- threat actors (malicious instructions, compromised dependencies, untrusted extensions, unintended model behaviour);
- enforcement boundary;
- bypass paths;
- failure behaviour (fails closed or open);
- verification evidence.

## Architectural invariants

Authorized humans keep a trusted control path to change policy and approve exceptions.

- **Authority:** an agent cannot grant itself additional permissions.
- **Integrity:** an agent cannot unilaterally modify the policies governing its own execution.
- **Verification:** an agent cannot override failed mandatory checks or bypass required verification.
- **Inheritance:** delegated agents and extensions cannot exceed their authorized scope.
- **Accountability:** security decisions, exceptions and verification results are independently inspectable.

The invariants become automated acceptance tests for the harness. Every new feature (background agents, plugins, memory, browser automation, model integrations) must pass them before release.

Reducing security friction is not a goal in itself. For example, a remembered posture acceptance is tied to a fingerprint of the effective security policy and is asked again when permissions or exposure change materially.

## Roadmap categories

| Category | Purpose |
|---|---|
| Security obligations | Close invariant violations and incomplete enforcement boundaries. |
| Competitive parity | Modern capabilities developers expect; pursued aggressively, without architectural debt against the security model. |
| Strategic differentiation | Secure software development, engineering assurance and independently verifiable guarantees beyond competitors. |

## Constraints

- Keep the single Rust binary, lightweight runtime, local-first operation and open-source architecture wherever practical.
- Engineering assurance integrates external scanners and verification tools through standardized interfaces, and does not reimplement them.
- Deprioritized: a proprietary cloud service, a desktop app, a hosted model gateway, a plugin marketplace.

## Success metrics

- **Higher:** functional task completion, verified acceptance criteria, unauthorized actions blocked.
- **Lower:** vulnerabilities introduced per change, security policy violations, regression rate across successive tasks, architectural violations, maintainability degradation over time, human intervention required, cost per verified successful task.

**Headline benchmark:** give each harness the same repository and a sequence of 20 dependent development tasks. Evaluate the codebase after every task, and compare its final state with the original baseline.
