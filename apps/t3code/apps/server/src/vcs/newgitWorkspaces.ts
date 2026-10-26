/**
 * newgit fork patch: in a project with `.newgit/config.toml`, worktrees become newgit workspaces.
 * Upstream files only carry small hooks into this module, so the fork rebases cleanly.
 * `T3CODE_NEWGIT=0` opts out; `T3CODE_NEWGIT_BIN` names a binary off the server's PATH.
 */
import { GitCommandError } from "@t3tools/contracts";
import * as Duration from "effect/Duration";
import * as Effect from "effect/Effect";
import * as FileSystem from "effect/FileSystem";
import * as Option from "effect/Option";
import * as Path from "effect/Path";
import * as ChildProcessSpawner from "effect/process/ChildProcessSpawner";
import * as Schema from "effect/Schema";

import * as ProcessRunner from "../processRunner.ts";

// Spawning runs resource `prepare` hooks (dependency installs), so allow far
// longer than a plain `git worktree add`.
const SPAWN_TIMEOUT = Duration.minutes(15);
const REMOVE_TIMEOUT = Duration.minutes(5);

const decodeSpawnOutput = Schema.decodeUnknownEffect(
  Schema.fromJsonString(Schema.Struct({ workspace: Schema.NonEmptyString })),
);
const isSpawnError = Schema.is(ProcessRunner.ProcessSpawnError);

export interface NewgitWorkspace {
  /** The instance name, which is also its source branch. */
  readonly name: string;
  /** The project root that owns the instance's store. */
  readonly storeRoot: string;
}

const newgitBinary = () => process.env.T3CODE_NEWGIT_BIN?.trim() || "newgit";

const enabled = () => process.env.T3CODE_NEWGIT !== "0";

/** Whether `cwd` is the root of a newgit project, so worktrees should be newgit workspaces. */
export const projectUsesNewgit = Effect.fn("newgitWorkspaces.projectUsesNewgit")(function* (
  cwd: string,
) {
  if (!enabled()) return false;
  const fileSystem = yield* FileSystem.FileSystem;
  const path = yield* Path.Path;
  return yield* fileSystem
    .exists(path.join(cwd, ".newgit", "config.toml"))
    .pipe(Effect.orElseSucceed(() => false));
});

// The marker is two `key = "value"` lines written by newgit; a TOML parser is overkill.
const parseMarker = (contents: string): NewgitWorkspace | null => {
  const fields = new Map<string, string>();
  for (const line of contents.split("\n")) {
    const match = /^\s*(\w+)\s*=\s*"((?:[^"\\]|\\.)*)"\s*$/.exec(line);
    if (match) fields.set(match[1]!, match[2]!.replace(/\\(.)/g, "$1"));
  }
  const name = fields.get("branch");
  const storeRoot = fields.get("store_root");
  return name && storeRoot ? { name, storeRoot } : null;
};

/** The newgit instance whose workspace is `workspacePath`, if it is one. */
export const readNewgitWorkspace = Effect.fn("newgitWorkspaces.readNewgitWorkspace")(function* (
  workspacePath: string,
) {
  if (!enabled()) return null;
  const fileSystem = yield* FileSystem.FileSystem;
  const path = yield* Path.Path;
  return yield* fileSystem
    .readFileString(path.join(workspacePath, ".newgit", "local", "instance.toml"))
    .pipe(
      Effect.map(parseMarker),
      Effect.orElseSucceed(() => null),
    );
});

const runNewgit = Effect.fn("newgitWorkspaces.runNewgit")(function* (input: {
  readonly operation: string;
  readonly cwd: string;
  readonly args: ReadonlyArray<string>;
  readonly timeout: Duration.Duration;
}) {
  const runner = yield* ProcessRunner.make();
  const command = `newgit ${input.args[0] ?? ""}`.trim();
  const fail = (detail: string, extra: { exitCode?: number; cause?: unknown } = {}) =>
    new GitCommandError({ operation: input.operation, command, cwd: input.cwd, detail, ...extra });
  const result = yield* runner
    .run({
      command: newgitBinary(),
      args: input.args,
      cwd: input.cwd,
      timeout: input.timeout,
    })
    .pipe(
      Effect.mapError((cause) => fail(`${command} could not run: ${cause.message}`, { cause })),
    );
  if (result.timedOut) return yield* fail(`${command} timed out`);
  if (result.code !== 0) {
    return yield* fail(
      result.stderr.trim() || `${command} failed`,
      result.code === null ? {} : { exitCode: Number(result.code) },
    );
  }
  return result.stdout;
});

/**
 * `newgit spawn <name> --from <ref>` in the project at `cwd`. Returns the new
 * workspace path, or None when newgit is not installed, so the caller can fall
 * back to a git worktree rather than fail the thread.
 */
export const spawnNewgitWorkspace = Effect.fn("newgitWorkspaces.spawnNewgitWorkspace")(
  function* (input: { readonly cwd: string; readonly name: string; readonly from: string }) {
    const stdout = yield* runNewgit({
      operation: "newgitWorkspaces.spawn",
      cwd: input.cwd,
      args: ["spawn", input.name, "--from", input.from, "--json"],
      timeout: SPAWN_TIMEOUT,
    }).pipe(
      Effect.asSome,
      Effect.catchIf(
        (error) => isSpawnError(error.cause),
        (error) =>
          Effect.logWarning("newgit is not runnable; using a git worktree instead", {
            cwd: input.cwd,
            detail: error.detail,
          }).pipe(Effect.as(Option.none<string>())),
      ),
    );
    if (Option.isNone(stdout)) return Option.none<string>();
    const { workspace } = yield* decodeSpawnOutput(stdout.value).pipe(
      Effect.mapError(
        (cause) =>
          new GitCommandError({
            operation: "newgitWorkspaces.spawn",
            command: "newgit spawn",
            cwd: input.cwd,
            detail: "newgit spawn did not report a workspace path",
            cause,
          }),
      ),
    );
    return Option.some(workspace);
  },
);

/** `newgit remove <name>`, run from the instance's project root. */
export const removeNewgitWorkspace = (workspace: NewgitWorkspace) =>
  runNewgit({
    operation: "newgitWorkspaces.remove",
    cwd: workspace.storeRoot,
    args: ["remove", workspace.name],
    timeout: REMOVE_TIMEOUT,
  }).pipe(Effect.asVoid);

/** Provide what this module needs from services a caller already holds. */
export const provideNewgitServices =
  (services: {
    readonly fileSystem: FileSystem.FileSystem;
    readonly path: Path.Path;
    readonly spawner: ChildProcessSpawner.ChildProcessSpawner["Service"];
  }) =>
  <A, E, R>(effect: Effect.Effect<A, E, R>) =>
    effect.pipe(
      Effect.provideService(FileSystem.FileSystem, services.fileSystem),
      Effect.provideService(Path.Path, services.path),
      Effect.provideService(ChildProcessSpawner.ChildProcessSpawner, services.spawner),
    );
