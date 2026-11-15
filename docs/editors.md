# Editors (Agent Client Protocol)

The rest of the [Editors](../README.md#editors-agent-client-protocol) section.

`buildwithnexus acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com)
(version 1) on stdin and stdout, so Zed, JetBrains IDEs and Neovim plugins
that run ACP agents can drive bwn: the editor shows the streamed reply, each
tool call with its diff, the plan, and the approval questions. In Zed, add it
to `settings.json`, then pick it from the Agent Panel's new-thread menu:

```json
{
  "agent_servers": {
    "buildwithnexus": {
      "type": "custom",
      "command": "bwn",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

Other ACP clients take the same command and arguments. bwn uses the provider,
model and key you set up in the terminal (`~/.buildwithnexus`); put an API key
or `NEXUS_HOME` in `env` to use others, and the options of
[Headless and CI](../README.md#headless-and-ci) in `args` (for example
`["acp", "--permission-mode", "accept-edits"]`). If the editor cannot find
`bwn`, give the full path that `command -v bwn` prints.

What carries over from the terminal:

- **Approvals.** Every approval the terminal would ask for is asked in the
  editor with *Allow once*, *Always allow* (the terminal's `a`: remembered for
  this project in your settings) and *Reject* (told to the model as a
  denial). Permission modes, allow/ask/deny rules, hooks and the sandbox apply
  as in the terminal. The model's `question` tool gets no answer.
- **Folder trust.** A project's `.buildwithnexus` settings are asked about
  in the editor at the first prompt, with the same questions; a yes is
  remembered as in the terminal, and `--trust-project` works too. A project
  hook whose script changed since you trusted it is skipped with a warning,
  as in headless runs.
- **Modes.** Build, Plan and Brainstorm are the session's modes. In Plan the
  plan arrives as the editor's plan, and building it is a question of its own;
  approving it switches the session to Build.
- **Files.** When the editor offers it, `read_file`, `write_file`,
  `edit_file` and `multi_edit` read and write through the editor, so they see
  unsaved changes and the editor tracks each edit. Commands run in bwn's own
  shell.
- **Sessions.** Each editor thread is a bwn session saved like any other;
  the editor can reopen one (`session/load` replays it), and `bwn resume`
  continues it in the terminal.
- **Cancel** stops the turn at once, including an open question.
- **MCP servers** the editor passes (stdio or HTTP) join the ones in your
  settings; SSE servers are skipped.

One `bwn acp` process serves one folder, and runs one prompt at a time;
helpers a reply starts run one after another, so the editor can show each
one's calls under it.
Everything bwn prints besides the protocol goes to stderr, which the editor
keeps as the agent's log. Images and embedded files in a prompt are sent to
the model (images only when it takes them); audio is not.

