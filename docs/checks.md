# Checks

When a turn changes the working tree, bwn runs the project's checks before
the run can end as `success`. Any change counts, whether a file edit, a
command, or a helper's work. Failing checks go back to the model, at most
twice a turn, and a run whose checks still fail exits 7. A `check_work` call
with the model's own `command` is advisory: it never stands in for them.

bwn detects the checks from `Cargo.toml`, `package.json` (its `typecheck`,
`build`, `test` and `lint` scripts), `go.mod` or Python project files. To
name them yourself, set `checks`:

```json
{ "checks": ["npm test", { "name": "types", "command": "npx tsc --noEmit", "kind": "build" }] }
```

`kind` (`build`, `test`, `lint` or `security`, default `test`) is what
`check_work`'s `only` filters on. A passing `security` check (a SAST or
secret scanner you run) is what meets the verifier's security review for an
auth change; a web search doesn't. `"checks": []` means no checks, and a named check that
can't run fails. The checks are the ones the turn started with: a turn that
rewrites the `package.json` script they run fails them. A project's `checks` run
code, so like its hooks they wait until you trust the file; until then bwn
runs the checks it detects.

A check can be for some files only: `"paths": ["*.css", "web/*"]` runs it
when a changed file's path or name matches one of the globs, so styles,
scripts or templates each get their own checks. A check without `paths`
runs for every change. The commit's evidence record names the checks that
applied and a digest of the policy they came from (`buildwithnexus evidence
verify <commit>` shows it).

A check can be read from a scanner's report instead of its exit status:
`"format": "sarif"` (SARIF 2.1.0) or `"json"` (a list of findings), with
`"fail_on"` the lowest SARIF level that fails (`error` by default) and
`"output"` a report file the command writes. It passes only when the report
parses and holds no finding; a scanner that crashed fails.

```json
{ "checks": [{ "name": "sast", "command": "semgrep scan --sarif --quiet", "format": "sarif", "kind": "security" }] }
```

`"security_gates"` adds scanners every change must pass, each read from its
report: `"secrets"` (gitleaks, on every change), `"dependencies"`
(osv-scanner, when a manifest or lockfile changed), `"sast"` (semgrep over
the changed files, warnings and errors block), `"config"` (`trivy config`,
when a Dockerfile, Terraform, Kubernetes or CI file changed) and `"sbom"`
(syft regenerates `bom.json` as CycloneDX when a manifest or lockfile
changed; the commit's evidence record names it). A
gate whose scanner isn't installed blocks. semgrep's `--config auto`
downloads its rules, so its host must be allowed in `network.allow`.

## Architectural boundaries

`boundaries` records decisions about what may depend on what, and checks
them on every round (roadmap 5.7):

```json
{
  "boundaries": [
    {
      "name": "ui-no-db",
      "from": ["src/ui/**"],
      "forbid": ["crate::db", "../db/"],
      "why": "ADR 7: the UI reads through the service layer"
    }
  ]
}
```

- **What breaks a boundary:** a changed file matching a `from` glob that
  has a line containing one of the `forbid` texts.
- **What happens then:** the break fails the round like a failing check,
  even when the project has no other checks. The model gets each break as
  `file:line uses `text` (name: why)`, and a run that still breaks one
  exits 7.
- **When the rules are read:** they are pinned when the turn starts, so a
  turn that edits them doesn't change the round it is checked by.
- **In a project file:** a boundary only adds a check and runs no program,
  so it applies without trust.

## Risky changes need a plan

With `"risk_planning": "require"`, a change to any of these waits for an
approved plan with a threat-model step (roadmap 5.5):
- authentication or authorization;
- a database schema or migration;
- a public API (an OpenAPI or Swagger file, `.proto`, `.graphql`);
- CI;
- the dependencies (a manifest, or `npm install`, `cargo add` and the
  like).

How it works:
- **Classification:** bwn decides the kind of change from the paths and
  the command, not from anything the model says.
- **Before a plan:** such a change is refused in every mode. No approval
  or allow rule lets it through.
- **The plan:** switch to PLAN (`/plan`) and approve a plan that includes
  a step threat-modelling the change. The plan prompt asks for one
  whenever the work touches these areas. Once it's approved, risky
  changes go ahead for the rest of that session.
- **Default:** `"off"`. A project file may set `"require"` without trust,
  since that only tightens. It can't turn the setting off once you've set
  `"require"`.
