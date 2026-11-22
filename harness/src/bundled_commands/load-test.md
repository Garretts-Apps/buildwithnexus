---
description: Sketch a bounded load test for an endpoint or workflow
---

# Load test

Target: $ARGUMENTS

1. Pick the runner the repo uses (k6, locust, etc.) or propose k6 if none.
2. Define success thresholds: latency and error rate.
3. Keep the script small and runnable locally. No production URLs unless given.
4. Run once if safe and quote summarized output. Do not DDoS shared environments.