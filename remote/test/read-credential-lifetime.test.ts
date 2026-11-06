import { describe, expect, it } from "vitest";

import { CREDENTIAL_TTL_SECONDS, READ_CREDENTIAL_TTL_SECONDS } from "../src/index";

// A clone's stepped fetch of a large history outlasted a fifteen-minute read
// credential and failed part way (#144).
describe("credential lifetimes", () => {
	it("gives a read long enough for a stepped fetch, and a write no more than it had", () => {
		expect(READ_CREDENTIAL_TTL_SECONDS).toBeGreaterThanOrEqual(60 * 60);
		expect(CREDENTIAL_TTL_SECONDS).toBe(15 * 60);
	});
});
