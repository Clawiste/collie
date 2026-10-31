const test = require("node:test");
const assert = require("node:assert");

test("passes", () => assert.equal(1 + 2, 3));
test("fails", () => assert.equal(1 + 2, 4));
