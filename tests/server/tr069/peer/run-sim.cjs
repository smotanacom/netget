// One genieacs-sim device in one process (the CLI forks a cluster whose workers outlive a
// killed parent): node run-sim.cjs <acs-url> [serial]. The device is the simulator's own
// TR-098 data model; it informs, answers every RPC, and informs again after its
// PeriodicInformInterval.
//
// genieacs-sim 0.9.0 was written against libxmljs 0.18, which no longer builds on current
// Node. 1.0 builds, but refuses text and attribute values that are not strings, which the
// simulator passes (numbers), and its one-argument attr(name) is a getter there and a
// deprecated setter here (1.0 reads attributes with getAttribute). Both are adapted below;
// nothing else is changed.
const libxmljs = require("libxmljs");
const proto = Object.getPrototypeOf(libxmljs.Document().node("x"));
const text = proto.text;
proto.text = function (...a) {
  return a.length ? text.call(this, String(a[0])) : text.call(this);
};
const attr = proto.attr;
proto.attr = function (...a) {
  if (a.length === 1 && typeof a[0] === "string") return this.getAttribute(a[0]);
  if (a.length === 1 && a[0] && typeof a[0] === "object")
    return attr.call(this, Object.fromEntries(Object.entries(a[0]).map(([k, v]) => [k, String(v)])));
  return attr.apply(this, a);
};

const [acsUrl, serial = "000001"] = process.argv.slice(2);
const model = require("genieacs-sim/data_model_202BC1-BM632w-8KA8WA1151100043.json");
require("genieacs-sim/simulator").start(model, serial, acsUrl);
console.log(JSON.stringify({ started: serial, acs: acsUrl }));
