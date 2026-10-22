import assert from "node:assert/strict";
import test from "node:test";
import { kindForProcessLine } from "./log-kind.ts";

test("python INFO on stderr is ordinary output", () => {
  assert.equal(
    kindForProcessLine(
      "err",
      "INFO:ida_pro_mcp.idalib_supervisor:Spawning idalib worker on 127.0.0.1:11616",
    ),
    "out",
  );
});

test("warnings are not painted as errors", () => {
  assert.equal(kindForProcessLine("err", "WARNING:ida:cache miss"), "warn");
  assert.equal(kindForProcessLine("err", "WARN something"), "warn");
});

test("error levels and unclassified stderr stay red", () => {
  assert.equal(kindForProcessLine("err", "ERROR:ida:worker exited"), "err");
  assert.equal(kindForProcessLine("err", "CRITICAL:ida:abort"), "err");
  assert.equal(kindForProcessLine("err", "Traceback (most recent call last):"), "err");
});

test("stdout is never recolored from its level prefix", () => {
  assert.equal(kindForProcessLine("out", "ERROR:not really"), "out");
});
