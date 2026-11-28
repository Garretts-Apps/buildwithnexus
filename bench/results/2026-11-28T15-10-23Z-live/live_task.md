# Fixed task against baidu/ernie-5.5-flash-preview:free

Run 2026-11-28T15-10-23Z, one run per harness through `bench/live_proxy.py` to https://openrouter.ai/api/v1. Tokens are what the upstream reported for each request. Wall time includes the upstream's latency and queueing, and the retries a harness made after a 429. The proxy buffers each reply, so it isn't a latency measurement. Requests and bytes count the requests the upstream answered; refused ones are in their own column.

| Harness | outcome | requests | refused (429 etc.) | request bytes | prompt tokens | completion tokens | wall s |
|---|---|---:|---:|---:|---:|---:|---:|
| bwn native | ok | 1 | 0 | 17113 | 4413 | 6 | 1.568 |
| bwn (npm) | ok | 1 | 0 | 17110 | 4414 | 6 | 1.939 |
| Claude Code | skipped: Claude Code speaks the Anthropic Messages API (ANTHROPIC_BASE_URL). It cannot be pointed at an OpenAI-compatible chat or responses endpoint, so it is not given a fake time. | | | | | | |
| Codex CLI | ok | 1 | 0 | 39850 | 8644 | 6 | 3.448 |
| Gemini CLI | skipped: Gemini CLI's GOOGLE_GEMINI_BASE_URL is the Gemini API base (generateContent), not an OpenAI-compatible chat or responses URL. It is not given a fake time. | | | | | | |
| OpenCode | ok | 2 | 4 | 32304 | 7181 | 11 | 11.643 |
| Aider | ok | 1 | 15 | 53584 (incl. refused) | 717 | 34 | 24.656 |
| Copilot CLI | ok | 1 | 0 | 60000 | 13483 | 6 | 2.178 |
| Crush | ok | 4 | 3 | 74607 (incl. refused) | 15609 | 1154 | 44.5 |
