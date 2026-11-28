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
