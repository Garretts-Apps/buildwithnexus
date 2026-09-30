'use strict';
// Preloaded with `node -r`: reports FAKE_GLIBC as the runtime glibc, so a test
// on a new distro can play an old one. FAKE_GLIBC=musl plays Alpine: no glibc,
// and a musl loader in /lib.
const fs = require('fs');

const musl = process.env.FAKE_GLIBC === 'musl';
const real = process.report.getReport.bind(process.report);
process.report.getReport = (...args) => {
  const report = real(...args);
  if (musl) delete report.header.glibcVersionRuntime;
  else report.header.glibcVersionRuntime = process.env.FAKE_GLIBC;
  return report;
};
if (musl) {
  const readdir = fs.readdirSync;
  fs.readdirSync = (p, ...rest) =>
    (p === '/lib' ? ['ld-musl-x86_64.so.1'] : readdir(p, ...rest));
}
