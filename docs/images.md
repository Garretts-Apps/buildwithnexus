# Inline images

The rest of the [Inline images](../README.md#inline-images) section.

Attach an image three ways — `Ctrl+V` with a screenshot on the clipboard,
drag a file onto the terminal (or paste its path), or type `@shot.png` — and
it is drawn inside the transcript, above the composer, before you send it:

| Terminal | What you see |
|---|---|
| kitty ≥ 0.28, Ghostty, WezTerm (placeholder support) | the real pixels, at up to the full width of the window, scrolling with the text; works inside tmux with `set -g allow-passthrough on` |
| Sixel terminals: Windows Terminal 1.22+, WezTerm, foot, mlterm, xterm `-ti vt340` | the real pixels, detected at startup; needs `ffmpeg` on PATH |
| any other truecolor terminal (iTerm2, Alacritty, older Windows Terminal, VS Code…) | half-block art (needs `ffmpeg` on PATH to decode) |
| `NO_COLOR`, non-truecolor | no preview, never garbage |

A pasted PNG needs no external tool at all. JPEG, WebP, GIF and a video's
first frame are converted with ffmpeg when it is installed. Control it with
the `images` settings key (`"auto"` default, `"kitty"`, `"sixel"`, `"blocks"`,
`"off"`) or `BWN_IMAGES=…` for one run. Sixel and block previews take at most
half the window's width and a third of its height, and re-fit when the
window is resized. Uploads are freed when you `/clear` or exit.

## Pictures, PDFs and screenshots the agent reads

The model can look at pictures too, when it takes images (see `vision` under
[Models](../README.md#models)). `read_file` on a PNG, JPEG, GIF or WebP (up to 5 MB)
returns the picture: Anthropic gets it inside the tool result, and
OpenAI-compatible servers and Ollama get it in a user message right after
the tool results. A text-only model is told why it sees none, and the
transcript shows what the picture was (`image shot.png (image/png,
1280x800, …)`). `read_file` on a PDF returns its text through
`pdftotext` from poppler when that is installed (`apt install
poppler-utils`, `brew install poppler`), and otherwise says what to install;
a line range works as for any file.

`screenshot_url` opens a page served on this machine
(`http://localhost:3000`, a dev server the agent started) in a local
headless Chrome, Chromium or Edge and shows the model the picture (`width`
and `height` optional, default 1280×800). The browser is `BWN_CHROME` when
set, else one found on PATH, in the usual install folders, or a Playwright
build. It runs with a throwaway profile and without provider keys in its
environment. Only loopback addresses are opened unless `network.allow` (or
an allow rule for `screenshot_url`) names the host, and outside `auto` each
host is approved like a fetch. Every request the page or the browser makes
to any other host, link-local and cloud-metadata addresses included, goes to
a local proxy that refuses it, and the result lists the hosts the page
tried. The tool is offered only to models that take images.

`"vision": false` in settings.json makes bwn treat the model as text-only:
`read_file` then returns a notice instead of a picture and `screenshot_url`
is not offered. Pictures a tool returned are kept in the session file, like
attached images.

A related setting: `notify` (`"auto"` — desktop notification when a turn of
8 s or longer ends, or an approval or a question is waiting, while the window
is unfocused; `"always"`; `"off"`).

