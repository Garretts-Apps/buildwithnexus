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

`kind` (`build`, `test` or `lint`, default `test`) is what `check_work`'s
`only` filters on. `"checks": []` means no checks, and a named check that
can't run fails. The checks are the ones the turn started with: a turn that
rewrites the `package.json` script they run fails them. A project's `checks` run
code, so like its hooks they wait until you trust the file; until then bwn
runs the checks it detects.
