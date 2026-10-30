import { describe, expect, it } from "vitest";

import { decideStart } from "../src/endpoint";

// A snapshot holds the whole container, `newgit` and its composed views
// included, so one from an older image would undo every deploy since.
const current = {
	image: "endpoint:new",
	running: false,
	inFlight: 1,
	snapshot: "snap",
	snapshotImage: "endpoint:new",
};

describe("starting the Git endpoint's container", () => {
	it("comes back from a snapshot taken on the image deployed now", () => {
		expect(decideStart(current)).toBe("from-snapshot");
	});

	it("starts from the image when the snapshot was taken on another one", () => {
		expect(decideStart({ ...current, snapshotImage: "endpoint:old" })).toBe("from-image");
	});

	it("starts from the image when the snapshot never recorded one", () => {
		expect(decideStart({ ...current, snapshotImage: undefined })).toBe("from-image");
	});

	it("starts from the image when there is no snapshot", () => {
		expect(decideStart({ ...current, snapshot: undefined })).toBe("from-image");
	});

	it("keeps a running container on the image deployed now", () => {
		expect(decideStart({ ...current, running: true, runningImage: "endpoint:new" })).toBe("running");
	});

	it("replaces a running container from another image", () => {
		expect(decideStart({ ...current, running: true, runningImage: "endpoint:old" })).toBe("replace");
		expect(decideStart({ ...current, running: true, runningImage: undefined })).toBe("replace");
	});

	it("leaves an old container alone while another response streams from it", () => {
		expect(decideStart({ ...current, running: true, runningImage: "endpoint:old", inFlight: 2 })).toBe("running");
	});
});
