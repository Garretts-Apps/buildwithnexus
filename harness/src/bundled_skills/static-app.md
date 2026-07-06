# Static App and Canvas Game

Use this skill when the user asks for a small website, standalone browser app, canvas game, playable demo, visualization, animation, or prototype that can run without a build step.

## Output Contract

- Build the actual runnable thing, not an explanation.
- Prefer a single self-contained HTML file with embedded CSS and JavaScript unless the user explicitly asks for a framework.
- Use `Artifact` or `publish_artifact` with `type: "html"` for standalone deliverables.
- After publishing, use `open_browser` when possible so the user can try it immediately.
- Never respond with source code only in markdown when the task asks you to build the app.

## Canvas Game Standard

A canvas game should include:

- A visible play field that resizes to the viewport without clipping.
- Keyboard controls and touch/mobile controls when practical.
- A start or ready state, active play state, pause state, game-over state, and restart path.
- Score/progress feedback and clear collision or failure feedback.
- A real animation loop using `requestAnimationFrame`.
- No placeholder art or unimplemented TODO sections.

## Quality Bar

- Use cohesive visual styling, not a plain default canvas on a white page.
- Keep all text readable on mobile and desktop.
- Avoid external CDN dependencies for simple games and demos.
- If the user names a theme loosely, choose a good one and build immediately.
- Include concise usage instructions inside the artifact only when they help the user play or operate it.

## Verification

- If a browser opener is available, call `open_browser` on the artifact path.
- If the app needs a local server, call `start_server`, call `wait_for_url` to prove the URL responds, inspect `read_server_log` on failure, then call `open_browser` with the local URL.
- If browser launch fails because the environment has no GUI, report the artifact path and the exact command to open or serve it.
