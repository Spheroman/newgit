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
import * as Semaphore from "effect/Semaphore";

import * as ProcessRunner from "../processRunner.ts";

// Spawning runs resource `prepare` hooks (dependency installs), so allow far
// longer than a plain `git worktree add`.
const SPAWN_TIMEOUT = Duration.minutes(15);
const REMOVE_TIMEOUT = Duration.minutes(5);
const LIFECYCLE_TIMEOUT = Duration.minutes(5);

// The fields of `spawn --json` this module reads; newgit reports more.
const SpawnResource = Schema.Struct({
  name: Schema.String,
  status: Schema.String,
  blocked_by: Schema.optional(Schema.NullOr(Schema.Array(Schema.String))),
  prepare_log: Schema.optional(Schema.NullOr(Schema.String)),
  render_error: Schema.optional(Schema.NullOr(Schema.String)),
  export_error: Schema.optional(Schema.NullOr(Schema.String)),
});
const decodeSpawnOutput = Schema.decodeUnknownEffect(
  Schema.fromJsonString(
    Schema.Struct({
      workspace: Schema.NonEmptyString,
      resources: Schema.optional(Schema.Array(SpawnResource)),
    }),
  ),
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

interface NewgitOutput {
  readonly stdout: string;
  readonly stderr: string;
  readonly exitCode: number;
}

const runNewgit = Effect.fn("newgitWorkspaces.runNewgit")(function* (input: {
  readonly operation: string;
  readonly cwd: string;
  readonly args: ReadonlyArray<string>;
  readonly timeout: Duration.Duration;
  /** Exit codes after which stdout is still the command's report, besides 0. */
  readonly reportExitCodes?: ReadonlyArray<number>;
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
  const exitCode = result.code === null ? null : Number(result.code);
  if (exitCode === null || (exitCode !== 0 && !input.reportExitCodes?.includes(exitCode))) {
    return yield* fail(
      result.stderr.trim() || `${command} failed`,
      exitCode === null ? {} : { exitCode },
    );
  }
  const output: NewgitOutput = { stdout: result.stdout, stderr: result.stderr, exitCode };
  return output;
});

const describeResource = (resource: typeof SpawnResource.Type) => {
  const reason =
    resource.render_error ??
    resource.export_error ??
    (resource.blocked_by?.length ? `blocked by ${resource.blocked_by.join(", ")}` : null) ??
    (resource.prepare_log ? `see ${resource.prepare_log}` : null);
  return reason
    ? `${resource.name} (${resource.status}): ${reason}`
    : `${resource.name} (${resource.status})`;
};

/**
 * `newgit spawn <name> --from <ref>` in the project at `cwd`. Returns the workspace
 * path, or None when newgit can't run so the caller falls back to a git worktree.
 * Exit 1 with a report means a resource didn't come up: open the workspace anyway.
 */
export const spawnNewgitWorkspace = Effect.fn("newgitWorkspaces.spawnNewgitWorkspace")(
  function* (input: { readonly cwd: string; readonly name: string; readonly from: string }) {
    const output = yield* runNewgit({
      operation: "newgitWorkspaces.spawn",
      cwd: input.cwd,
      args: ["spawn", input.name, "--from", input.from, "--json"],
      timeout: SPAWN_TIMEOUT,
      reportExitCodes: [1],
    }).pipe(
      Effect.asSome,
      Effect.catchIf(
        (error) => isSpawnError(error.cause),
        (error) =>
          Effect.logWarning("newgit is not runnable; using a git worktree instead", {
            cwd: input.cwd,
            detail: error.detail,
          }).pipe(Effect.as(Option.none<NewgitOutput>())),
      ),
    );
    if (Option.isNone(output)) return Option.none<string>();
    const { stdout, stderr, exitCode } = output.value;
    const { workspace, resources = [] } = yield* decodeSpawnOutput(stdout).pipe(
      Effect.mapError(
        (cause) =>
          new GitCommandError({
            operation: "newgitWorkspaces.spawn",
            command: "newgit spawn",
            cwd: input.cwd,
            detail:
              exitCode === 0
                ? "newgit spawn did not report a workspace path"
                : stderr.trim() || "newgit spawn failed",
            ...(exitCode === 0 ? {} : { exitCode }),
            cause,
          }),
      ),
    );
    const notReady = resources.filter((resource) => resource.status !== "ready");
    if (exitCode !== 0 || notReady.length > 0) {
      yield* Effect.logWarning("newgit spawned the workspace, but some resources are not ready", {
        workspace,
        resources: notReady.map(describeResource),
      });
    }
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

const decodeLifecycleOutput = Schema.decodeUnknownEffect(
  Schema.fromJsonString(
    Schema.Struct({
      resources: Schema.Array(
        Schema.Struct({
          name: Schema.String,
          status: Schema.String,
          error: Schema.optional(Schema.NullOr(Schema.String)),
        }),
      ),
    }),
  ),
);

// One permit per instance, so a turn's `start` waits out a settle's `stop` still running.
const instanceLocks = new Map<string, Semaphore.Semaphore>();
const instanceLock = (workspace: NewgitWorkspace) => {
  const key = `${workspace.storeRoot}\0${workspace.name}`;
  let lock = instanceLocks.get(key);
  if (lock === undefined) {
    lock = Semaphore.makeUnsafe(1);
    instanceLocks.set(key, lock);
  }
  return lock;
};

/**
 * `newgit stop|start <name>` for the instance whose workspace is `worktreePath`;
 * a no-op elsewhere. Failures are logged, never raised: settling and turns go on.
 * `stillWanted` is checked under the instance lock, so a stale request is dropped.
 */
const runLifecycle = Effect.fn("newgitWorkspaces.runLifecycle")(
  function* (verb: "stop" | "start", worktreePath: string, stillWanted: Effect.Effect<boolean>) {
    const workspace = yield* readNewgitWorkspace(worktreePath);
    if (workspace === null) return;
    yield* instanceLock(workspace).withPermits(1)(
      Effect.gen(function* () {
        if (!(yield* stillWanted)) return;
        const { stdout, exitCode } = yield* runNewgit({
          operation: `newgitWorkspaces.${verb}`,
          cwd: workspace.storeRoot,
          args: [verb, workspace.name, "--json"],
          timeout: LIFECYCLE_TIMEOUT,
          reportExitCodes: [1],
        });
        const { resources } = yield* decodeLifecycleOutput(stdout);
        const failed = resources.filter((resource) => resource.status === "failed");
        if (exitCode !== 0 || failed.length > 0) {
          yield* Effect.logWarning(`newgit ${verb} left some resources behind`, {
            instance: workspace.name,
            resources: failed.map((resource) => `${resource.name}: ${resource.error ?? "failed"}`),
          });
        }
      }),
    );
  },
  (effect, verb, worktreePath) =>
    effect.pipe(
      Effect.catchCause((cause) =>
        Effect.logWarning(`newgit ${verb} failed`, { worktreePath, cause }),
      ),
    ),
);

/** Release a settled thread's resources, keeping its workspace. */
export const stopNewgitInstance = (
  worktreePath: string,
  stillSettled: Effect.Effect<boolean> = Effect.succeed(true),
) => runLifecycle("stop", worktreePath, stillSettled);

/** Bring back what the last `stopNewgitInstance` released; cheap when nothing was. */
export const startNewgitInstance = (worktreePath: string) =>
  runLifecycle("start", worktreePath, Effect.succeed(true));

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
