# Hook examples (optional)

bwn runs hooks from `~/.buildwithnexus/settings.json` and from a trusted project's `.buildwithnexus/settings.json`. These examples mirror the public workflow kit: shell guards, write sandboxing, prompt credential screening, and invisible-character stripping.

Copy scripts from `../scripts/` into `~/.buildwithnexus/hooks/` or reference them by absolute path in settings. Merge the fragment below into your `hooks` object. Paths use `$NEXUS_HOME` or your own directory.

See `settings.fragment.json` for matchers and commands. Adjust timeouts and `onFailure` to match how strictly you want hooks to block.