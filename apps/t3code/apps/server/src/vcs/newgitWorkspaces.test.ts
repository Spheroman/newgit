// @effect-diagnostics nodeBuiltinImport:off - probes for the newgit binary synchronously to decide whether to skip.
import * as NodeChildProcess from "node:child_process";
import * as NodeServices from "@effect/platform-node/NodeServices";
import { assert, describe, it } from "@effect/vitest";
import * as Effect from "effect/Effect";
import * as FileSystem from "effect/FileSystem";
import * as Layer from "effect/Layer";
import * as Path from "effect/Path";

import * as ServerConfig from "../config.ts";
import * as GitVcsDriver from "./GitVcsDriver.ts";
import { readNewgitWorkspace } from "./newgitWorkspaces.ts";

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

describe("newgit workspaces", () => {
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
