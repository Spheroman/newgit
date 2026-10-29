// @effect-diagnostics nodeBuiltinImport:off - probes for the newgit binary synchronously to decide whether to skip.
import * as NodeChildProcess from "node:child_process";
import * as NodeServices from "@effect/platform-node/NodeServices";
import { assert, describe, it } from "@effect/vitest";
import * as Deferred from "effect/Deferred";
import * as Effect from "effect/Effect";
import * as Fiber from "effect/Fiber";
import * as FileSystem from "effect/FileSystem";
import * as Layer from "effect/Layer";
import * as Logger from "effect/Logger";
import * as Option from "effect/Option";
import * as Path from "effect/Path";

import * as ServerConfig from "../config.ts";
import * as GitVcsDriver from "./GitVcsDriver.ts";
import {
  readNewgitWorkspace,
  spawnNewgitWorkspace,
  startNewgitInstance,
  stopNewgitInstance,
} from "./newgitWorkspaces.ts";

const newgitAvailable =
  NodeChildProcess.spawnSync(process.env.T3CODE_NEWGIT_BIN || "newgit", ["--version"]).status === 0;

const layerTest = GitVcsDriver.layer.pipe(
  Layer.provide(ServerConfig.layerTest(process.cwd(), { prefix: "t3-newgit-test-" })),
  Layer.provideMerge(NodeServices.layer),
);

const run = (cwd: string, command: string, args: ReadonlyArray<string>) => {
  const result = NodeChildProcess.spawnSync(command, args, { cwd, encoding: "utf8" });
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")}: ${result.stderr}`);
};

// Point T3CODE_NEWGIT_BIN at a script standing in for newgit for the scope's lifetime.
const fakeNewgit = (script: string) =>
  Effect.gen(function* () {
    const fileSystem = yield* FileSystem.FileSystem;
    const path = yield* Path.Path;
    const bin = path.join(yield* fileSystem.makeTempDirectoryScoped(), "newgit");
    yield* fileSystem.writeFileString(bin, `#!/bin/sh\n${script}\n`);
    yield* fileSystem.chmod(bin, 0o755);
    yield* Effect.acquireRelease(
      Effect.sync(() => {
        const previous = process.env.T3CODE_NEWGIT_BIN;
        process.env.T3CODE_NEWGIT_BIN = bin;
        return previous;
      }),
      (previous) =>
        Effect.sync(() => {
          if (previous === undefined) delete process.env.T3CODE_NEWGIT_BIN;
          else process.env.T3CODE_NEWGIT_BIN = previous;
        }),
    );
  });

// A newgit workspace marker for `name`, with a log the fake newgit appends its calls to.
const fakeWorkspace = (name: string) =>
  Effect.gen(function* () {
    const fileSystem = yield* FileSystem.FileSystem;
    const path = yield* Path.Path;
    const dir = yield* fileSystem.makeTempDirectoryScoped();
    yield* fileSystem.makeDirectory(path.join(dir, ".newgit", "local"), { recursive: true });
    yield* fileSystem.writeFileString(
      path.join(dir, ".newgit", "local", "instance.toml"),
      `branch = "${name}"\nstore_root = "${dir}"\n`,
    );
    const log = path.join(dir, "calls.log");
    const calls = fileSystem.readFileString(log).pipe(
      Effect.map((text) => text.trim().split("\n")),
      Effect.orElseSucceed(() => [] as string[]),
    );
    return { dir, log, calls };
  });

describe("newgit workspaces", () => {
  it.effect("stops and starts an instance from its project root", () =>
    Effect.gen(function* () {
      const workspace = yield* fakeWorkspace("feature");
      yield* fakeNewgit(
        `echo "$(pwd -P) $*" >> '${workspace.log}'; echo '{"instance":"feature","resources":[]}'`,
      );
      yield* stopNewgitInstance(workspace.dir);
      yield* startNewgitInstance(workspace.dir);
      const root = NodeChildProcess.execSync("pwd -P", { cwd: workspace.dir }).toString().trim();
      assert.deepEqual(yield* workspace.calls, [
        `${root} stop feature --json`,
        `${root} start feature --json`,
      ]);
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );

  it.effect("leaves plain directories alone and skips a stop the thread no longer wants", () =>
    Effect.gen(function* () {
      const fileSystem = yield* FileSystem.FileSystem;
      const workspace = yield* fakeWorkspace("feature");
      yield* fakeNewgit(`echo "$*" >> '${workspace.log}'; echo '{"resources":[]}'`);
      yield* stopNewgitInstance(yield* fileSystem.makeTempDirectoryScoped());
      yield* startNewgitInstance(yield* fileSystem.makeTempDirectoryScoped());
      yield* stopNewgitInstance(workspace.dir, Effect.succeed(false));
      assert.deepEqual(yield* workspace.calls, []);
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );

  it.live("starts only after a stop still running finishes", () =>
    Effect.gen(function* () {
      const workspace = yield* fakeWorkspace("feature");
      yield* fakeNewgit(
        `echo "$1 begin" >> '${workspace.log}'; [ "$1" = stop ] && sleep 0.3; echo "$1 end" >> '${workspace.log}'; echo '{"resources":[]}'`,
      );
      // The guard runs under the instance lock, so once it fires the stop holds it.
      const stopHoldsLock = yield* Deferred.make<void>();
      const stopping = yield* Effect.forkChild(
        stopNewgitInstance(
          workspace.dir,
          Deferred.succeed(stopHoldsLock, undefined).pipe(Effect.as(true)),
        ),
      );
      yield* Deferred.await(stopHoldsLock);
      yield* startNewgitInstance(workspace.dir);
      yield* Fiber.join(stopping);
      assert.deepEqual(yield* workspace.calls, [
        "stop begin",
        "stop end",
        "start begin",
        "start end",
      ]);
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );

  it.effect("logs resources that failed to start instead of failing", () =>
    Effect.gen(function* () {
      const workspace = yield* fakeWorkspace("feature");
      const report = JSON.stringify({
        instance: "feature",
        resources: [
          { name: "web", action: "dev", status: "failed", error: "port 3000 in use" },
          { name: "db", action: "serve", status: "started" },
        ],
      });
      yield* fakeNewgit(`echo '${report}'; exit 1`);
      const logs: string[] = [];
      yield* startNewgitInstance(workspace.dir).pipe(
        Effect.provide(
          Logger.layer([Logger.make(({ message }) => logs.push(JSON.stringify(message)))]),
        ),
      );
      const warning = logs.find((line) => line.includes("left some resources")) ?? "";
      assert.include(warning, "web: port 3000 in use");
      assert.notInclude(warning, "db");
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );

  it.effect("opens the workspace when spawn exits 1 because a resource did not come up", () =>
    Effect.gen(function* () {
      const report = JSON.stringify({
        workspace: "/tmp/ws/feature",
        resources: [
          { name: "db", status: "failed", blocked_by: [], prepare_log: "/tmp/db.log" },
          { name: "web", status: "blocked", blocked_by: ["db"], prepare_log: null },
        ],
      });
      yield* fakeNewgit(`echo '${report}'; echo 'db: prepare failed' >&2; exit 1`);
      const logs: string[] = [];
      const workspace = yield* spawnNewgitWorkspace({
        cwd: process.cwd(),
        name: "feature",
        from: "main",
      }).pipe(
        Effect.provide(
          Logger.layer([Logger.make(({ message }) => logs.push(JSON.stringify(message)))]),
        ),
      );
      assert.deepEqual(workspace, Option.some("/tmp/ws/feature"));
      const warning = logs.find((line) => line.includes("not ready")) ?? "";
      assert.include(warning, "db (failed): see /tmp/db.log");
      assert.include(warning, "web (blocked): blocked by db");
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );

  it.effect("still fails when spawn exits 1 without a report", () =>
    Effect.gen(function* () {
      yield* fakeNewgit("echo 'no such ref: nope' >&2; exit 1");
      const error = yield* spawnNewgitWorkspace({
        cwd: process.cwd(),
        name: "feature",
        from: "nope",
      }).pipe(Effect.flip);
      assert.equal(error.detail, "no such ref: nope");
      assert.equal(error.exitCode, 1);
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );

  it.effect("still fails on exit codes other than 1", () =>
    Effect.gen(function* () {
      yield* fakeNewgit(`echo '{"workspace":"/tmp/ws/feature"}'; exit 2`);
      const error = yield* spawnNewgitWorkspace({
        cwd: process.cwd(),
        name: "feature",
        from: "main",
      }).pipe(Effect.flip);
      assert.equal(error.exitCode, 2);
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );

  it.effect("falls back to a git worktree when newgit cannot run", () =>
    Effect.gen(function* () {
      const previous = process.env.T3CODE_NEWGIT_BIN;
      process.env.T3CODE_NEWGIT_BIN = "/nonexistent/newgit";
      const workspace = yield* spawnNewgitWorkspace({
        cwd: process.cwd(),
        name: "feature",
        from: "main",
      }).pipe(
        Effect.ensuring(
          Effect.sync(() => {
            if (previous === undefined) delete process.env.T3CODE_NEWGIT_BIN;
            else process.env.T3CODE_NEWGIT_BIN = previous;
          }),
        ),
      );
      assert.deepEqual(workspace, Option.none());
    }).pipe(Effect.provide(NodeServices.layer)),
  );

  it.effect("reads the instance marker and ignores plain directories", () =>
    Effect.gen(function* () {
      const fileSystem = yield* FileSystem.FileSystem;
      const path = yield* Path.Path;
      const dir = yield* fileSystem.makeTempDirectoryScoped();
      assert.equal(yield* readNewgitWorkspace(dir), null);
      yield* fileSystem.makeDirectory(path.join(dir, ".newgit", "local"), { recursive: true });
      yield* fileSystem.writeFileString(
        path.join(dir, ".newgit", "local", "instance.toml"),
        'branch = "t3code/abcd1234"\nstore_root = "/tmp/a \\"quoted\\" root"\n',
      );
      assert.deepEqual(yield* readNewgitWorkspace(dir), {
        name: "t3code/abcd1234",
        storeRoot: '/tmp/a "quoted" root',
      });
    }).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
  );

  it.effect.skipIf(!newgitAvailable)(
    "creates, keeps the branch of, and removes a newgit workspace in place of a worktree",
    () =>
      Effect.gen(function* () {
        const fileSystem = yield* FileSystem.FileSystem;
        const path = yield* Path.Path;
        const driver = yield* GitVcsDriver.GitVcsDriver;
        // newgit records canonical paths (macOS temp dirs live behind /var -> /private/var).
        const root = yield* fileSystem.realPath(yield* fileSystem.makeTempDirectoryScoped());
        const project = path.join(root, "project");
        yield* fileSystem.makeDirectory(project);
        run(project, "git", ["init", "-q", "-b", "main"]);
        run(project, "git", [
          "-c",
          "user.name=t3",
          "-c",
          "user.email=t3@example.com",
          "commit",
          "-q",
          "--allow-empty",
          "-m",
          "init",
        ]);
        run(project, "newgit", ["init"]);
        // Keep workspaces inside the test's temp directory.
        const configPath = path.join(project, ".newgit", "config.toml");
        yield* fileSystem.writeFileString(
          configPath,
          `${yield* fileSystem.readFileString(configPath)}\n[workspace]\nroot = ${JSON.stringify(path.join(root, "workspaces"))}\n`,
        );

        const claimed: string[] = [];
        const { worktree } = yield* driver.createWorktree(
          { cwd: project, refName: "main", newRefName: "t3code/abcd1234", path: null },
          {
            progress: {
              onWorktreeClaimed: (claimedPath) => Effect.sync(() => claimed.push(claimedPath)),
            },
          },
        );
        assert.isTrue(worktree.path.startsWith(path.join(root, "workspaces")));
        assert.deepEqual(claimed, [worktree.path]);
        assert.deepEqual(yield* readNewgitWorkspace(worktree.path), {
          name: "t3code/abcd1234",
          storeRoot: project,
        });

        const renamed = yield* driver.renameBranch({
          cwd: worktree.path,
          oldBranch: "t3code/abcd1234",
          newBranch: "t3code/nicer-name",
        });
        assert.equal(renamed.branch, "t3code/abcd1234");

        yield* driver.removeWorktree({ cwd: project, path: worktree.path, force: true });
        assert.isFalse(yield* fileSystem.exists(worktree.path));
      }).pipe(Effect.scoped, Effect.provide(layerTest)),
  );
});
