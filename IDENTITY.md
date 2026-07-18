# Product identity

Who this product is, so every page, error message, and release note sounds
like the same tool. New copy ships when it passes these rules; existing copy
that breaks them is a bug.

**The character:** a fast, honest craftsman. Confident because it measures,
plain-spoken because it respects you, and funny only in passing — the way a
good colleague is.

`bwn` is the character — short, lowercase, terminal-native; it's what humans
type and what prose should call the tool. `buildwithnexus` is the formal
wordmark: package names, titles, legal.

## Three pillars

1. **Speed is respect.** Instant startup, no black frames, 60fps streaming —
   not benchmark flexing; making a human wait is rude. This is why
   "hilariously fast" works: confident without being solemn.
2. **Honesty is a feature.** The tool never claims what it didn't verify — in
   telemetry (no fabricated costs), in security docs ("guardrails, not a
   sandbox"), in release claims (a measured 215×, never "blazing"), and in
   the UX itself ("hot-swapped **(validated)**"). Calibrated truthfulness is
   the differentiator that's expensive to fake.
3. **No babysitting.** One ~3 MB binary, five direct dependencies, no
   language runtime, works with the model you already have. The product
   carries its own weight and updates only with consent.

## Voice rules (ship-blockers)

1. **Claims carry numbers or they don't ship.** "~215× faster — 966µs →
   4.5µs per streamed chunk", never "blazingly fast". If it isn't measured,
   don't say it.
2. **Errors teach the next step.** Every failure names what happened and the
   exact command that recovers: "Did you mean `read_file`?", "run
   `ollama pull llama3`, then `/model` again". A dead end in an error message
   is a bug.
3. **Wit is allowed, hype is not.** Dry, and at most one wink per surface
   ("hilariously fast", "deliberately boring launcher"). Exclamation marks
   are almost always hype.
4. **The user is a peer, not a patient.** No scolding, no "oops!", no
   cheer-leading. State facts, offer the next move, get out of the way.
5. **Never claim what wasn't verified — and say what wasn't.** "(validated)",
   "untested on Windows", "corruption check only, not an integrity control".
   Unmarked uncertainty is a lie with latency.
6. **lowercase `bwn` in prose and examples**; `buildwithnexus` for the
   wordmark, package registries, and first mention on a page.

## Where the voice lives

In descending order of importance: error messages → first-run/onboarding →
`/help` and command output → README/site copy → release notes. If budget for
polish is limited, spend it in that order — a user forgives a plain landing
page faster than an unhelpful error.

## Register boundary

The `letsbeheroes` skill pack speaks in its own earnest register ("the
oath"); that flavor stays inside the skill pack. Core surfaces keep the dry
craftsman voice.
