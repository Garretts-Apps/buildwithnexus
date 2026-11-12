'use strict';
// Preloaded through -r: cuts only the launcher's own 30 s timers to
// SHORT_TIMERS ms, so a stalled proxy fails within a test's time limit.
const ms = Number(process.env.SHORT_TIMERS);
const realSetTimeout = global.setTimeout;
global.setTimeout = (fn, delay, ...rest) => realSetTimeout(fn, delay === 30000 ? ms : delay, ...rest);
