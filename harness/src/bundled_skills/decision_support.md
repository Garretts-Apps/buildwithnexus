# Decision Support

When asked to evaluate a design decision, architecture choice, or strategic question, follow this structured workflow. Do NOT answer from intuition alone.

## Workflow

1. **Classify the decision**
   - What type? (architecture, implementation, deployment, security, data model, dependency, process)
   - What scope? (single file, module, service, system-wide)
   - What urgency? (blocking, important, can-defer)

2. **Gather evidence** — call these tools BEFORE forming an opinion:
   - `search` / `grep`: Find relevant code, configs, and docs
   - `read_file`: Read architecture decisions, README, existing implementations
   - `glob`: Map the affected file/module structure
   - `bash`: Run `git log` for history, check CI status, list dependencies

3. **Identify constraints** — document what limits the decision:
   - Existing API contracts
   - Database schema dependencies
   - Team capacity and operational maturity
   - Security and compliance requirements
   - Performance requirements
   - Timeline pressures

4. **Evaluate options** — for each option:
   - Describe the approach
   - List evidence supporting it
   - List evidence against it
   - Assess: risk, cost, reversibility, blast radius
   - Note assumptions made

5. **Apply decision rules**:
   - If the change shares lifecycle, data, and failure domain → prefer extending existing
   - If different scaling, security, or release needs → consider separation
   - If team lacks operational maturity → avoid fragmentation
   - If critical path → require feature flags and staged rollout
   - If dependency is unmaintained → avoid adding
   - If data is destructive → require rollback plan

6. **Produce a decision memo** with these sections:
   - **Recommendation**: clear, actionable statement
   - **Rationale**: why this option, grounded in evidence
   - **Evidence**: what was inspected (files, tests, docs, history)
   - **Tradeoffs**: what you gain and what you give up
   - **Risks**: what could go wrong and mitigations
   - **Assumptions**: what you assumed but couldn't verify
   - **Confidence**: high/medium/low with explanation
   - **Next actions**: concrete steps to implement the decision

## Rules

- Never recommend without inspecting relevant code and docs first
- Always state your confidence level
- Always list assumptions
- If evidence is missing, say so explicitly
- Prefer reversible decisions when confidence is medium or low
- If the decision could cause data loss, require a rollback plan
- If the decision affects security boundaries, flag for security review
