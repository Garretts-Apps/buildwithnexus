# Hero: Wait

Use this skill whenever work depends on something asynchronous — a server starting, a build finishing, CI completing, a file appearing, an index propagating.

## The rule

Never sleep-and-hope. `sleep 5 && try` encodes a guess about timing; guesses are wrong on slow machines, fast machines, and CI. Wait on the **observable condition** instead.

## Pattern

1. **Name the condition** that means "ready": port accepts connections, file exists and is non-empty, `curl` returns 200, job status is `completed`, log contains the marker line.
2. **Poll it in a loop** with a short interval and a **deadline**:
   ```bash
   deadline=$(( $(date +%s) + 120 ))
   until <condition>; do
     [ "$(date +%s)" -ge "$deadline" ] && { echo "timed out waiting for <thing>"; exit 1; }
     sleep 2
   done
   ```
3. **On timeout, report what was being waited for and the last observed state** — "timed out: port 3000 never opened; server log ends with X" is diagnosable, "it didn't work" is not.

## Rules

- Deadline is not optional. An unbounded wait converts a hung dependency into a hung agent.
- Poll the cheapest true signal: prefer an exit code or HTTP status over parsing rendered output.
- Match the interval to the thing: 1–2s for a local server, 15–30s for CI, longer for registry propagation. Polling a 10-minute build every second is noise.
- Waiting on several things? Wait for each named condition — a compound "sleep long enough for everything" is the same guess wearing a coat.
- If an event-driven hook exists (a webhook, a `--wait` flag, `tail -f` on a marker), prefer it over polling entirely.
