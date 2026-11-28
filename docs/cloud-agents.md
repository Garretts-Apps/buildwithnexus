# Cloud background agents

A cloud background agent runs on your repository's GitHub Actions runners
instead of your machine. It works like `/bg`: it gets a branch of its own,
it can't answer approvals, and its work comes back only through
`buildwithnexus bg merge`. That command merges only when every commit
verifies against its evidence record.

## Set up once

```bash
buildwithnexus bg cloud init
git add .github/workflows/bwn-cloud.yml
git commit -m "Add the bwn cloud agent workflow"
git push
```

`bg cloud init` writes `.github/workflows/bwn-cloud.yml`, which uses bwn's
GitHub Action at this bwn's version. It refuses to replace an existing
file unless you pass `--force`.

In the repository's settings, set:

- **Variables:** `BWN_PROVIDER` and `BWN_MODEL`, plus `BWN_BASE_URL` for a
  gateway.
- **A secret** with the provider's key: `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `OPENROUTER_API_KEY` or `CUSTOM_API_KEY`.

Dispatching needs the GitHub CLI (`gh`), signed in.

## Run a task

```bash
buildwithnexus bg run --cloud "add input validation to the signup form"
```

bwn dispatches the workflow on your current commit. That commit and the
workflow file must already be on `origin`, and bwn says so if they aren't.
The task reaches the workflow as an input and its scripts as an
environment variable, never as text in a script.

On the runner:

1. **The branch.** bwn starts a branch named `bwn/cloud-...` from your
   commit. The workflow refuses any other branch name.
2. **The task.** bwn runs it in auto mode with `--auto-commit`, so a change
   is committed only once its checks pass. Each commit names its evidence
   record in a `Bwn-Evidence:` trailer.
3. **Publishing.** `bwn bg publish` pushes the branch. It also pushes
   `bwn-evidence/<branch>`, which holds the records those commits name.

## Bring it back

```bash
buildwithnexus bg fetch bwn/cloud-...
buildwithnexus bg merge bwn/cloud-...
```

`bg fetch` fetches the branch and its evidence. It keeps a record only when
the record's content hashes to its name, and says which ones it refused.
`bg merge` then checks every commit on the branch against its record and
merges only if all of them match. If records are missing, nothing is
merged.

## What a cloud record shows

A record says what bwn saw on the runner, under the workflow's settings:
the checks that ran there and their results. The merge still refuses a
commit whose files don't match the record it names. The runner, its
settings and its secrets are your repository's.
