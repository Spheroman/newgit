import { describe, expect, it } from "vitest";

import { IDLE_MS, decideSleep } from "../src/endpoint";

// The container itself cannot run under Miniflare, so this checks the idle
// alarm's decision; the GitEndpoint class only carries it out.
const now = 1_000_000_000;
const idle = {
	now,
	running: true,
	inFlight: 0,
	lastRequestAt: now - IDLE_MS,
	composedAt: now - IDLE_MS - 2,
	snapshotAt: now - IDLE_MS - 1,
};

describe("the Git endpoint's idle alarm", () => {
	it("stops a container idle for IDLE_MS whose snapshot is current", () => {
		expect(decideSleep(idle)).toBe("stop");
	});

	it("snapshots first when something was composed since the last snapshot", () => {
		expect(decideSleep({ ...idle, composedAt: idle.snapshotAt + 1 })).toBe("snapshot-then-stop");
	});

	it("snapshots first when there has never been a snapshot", () => {
		expect(decideSleep({ ...idle, snapshotAt: 0 })).toBe("snapshot-then-stop");
	});

	it("leaves a container alone while a response is still streaming", () => {
		expect(decideSleep({ ...idle, inFlight: 1 })).toBe("busy");
	});

	it("does nothing when a request came in since the alarm was set", () => {
		expect(decideSleep({ ...idle, lastRequestAt: now - IDLE_MS + 1 })).toBe("too-soon");
	});

	it("does nothing when the container is already stopped", () => {
		expect(decideSleep({ ...idle, running: false })).toBe("not-running");
	});
});
