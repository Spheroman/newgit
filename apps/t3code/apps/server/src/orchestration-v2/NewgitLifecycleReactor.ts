/**
 * newgit fork patch: settling a thread stops its newgit instance's resources, and unsettling
 * starts them again. The workspace stays until the thread is deleted (`newgit remove`). A new
 * turn on a settled thread starts them itself, awaited, in ProviderTurnStartService.
 */
import type { OrchestrationV2DomainEvent } from "@t3tools/contracts";
import * as Context from "effect/Context";
import * as Effect from "effect/Effect";
import * as FileSystem from "effect/FileSystem";
import * as Layer from "effect/Layer";
import * as Path from "effect/Path";
import * as ChildProcessSpawner from "effect/process/ChildProcessSpawner";
import type * as Scope from "effect/Scope";
import * as Stream from "effect/Stream";

import { forkParked } from "../serverActivation.ts";
import * as NewgitWorkspaces from "../vcs/newgitWorkspaces.ts";
import * as Orchestrator from "./Orchestrator.ts";
import * as ProjectionStore from "./ProjectionStore.ts";

export class NewgitLifecycleReactor extends Context.Service<
  NewgitLifecycleReactor,
  {
    readonly start: () => Effect.Effect<void, never, Scope.Scope>;
    readonly handle: (event: OrchestrationV2DomainEvent) => Effect.Effect<void>;
  }
>()("t3/orchestration-v2/NewgitLifecycleReactor") {}

const make = Effect.gen(function* () {
  const engine = yield* Orchestrator.OrchestratorV2;
  const projections = yield* ProjectionStore.ProjectionStoreV2;
  const withNewgit = NewgitWorkspaces.provideNewgitServices({
    fileSystem: yield* FileSystem.FileSystem,
    path: yield* Path.Path,
    spawner: yield* ChildProcessSpawner.ChildProcessSpawner,
  });

  // A thread can be unsettled (or get a message) before a queued stop runs; skip it then.
  const stillSettled = (event: OrchestrationV2DomainEvent) =>
    projections.getThread(event.threadId).pipe(
      Effect.map((thread) => thread.settledOverride === "settled"),
      Effect.orElseSucceed(() => false),
    );

  const handle: NewgitLifecycleReactor["Service"]["handle"] = (event) => {
    if (event.type !== "thread.settled" && event.type !== "thread.unsettled") return Effect.void;
    const worktreePath = event.payload.worktreePath;
    if (worktreePath === null) return Effect.void;
    return withNewgit(
      event.type === "thread.settled"
        ? NewgitWorkspaces.stopNewgitInstance(worktreePath, stillSettled(event))
        : NewgitWorkspaces.startNewgitInstance(worktreePath),
    );
  };

  // Forked per event so a slow `stop` holds up neither settling nor other threads.
  const start: NewgitLifecycleReactor["Service"]["start"] = () =>
    forkParked(
      Stream.runForEach(engine.streamDomainEvents, (event) =>
        handle(event).pipe(Effect.forkDetach, Effect.asVoid),
      ).pipe(
        Effect.catchCause((cause) =>
          Effect.logWarning("newgit lifecycle event stream failed", { cause }),
        ),
      ),
    );

  return { start, handle } satisfies NewgitLifecycleReactor["Service"];
});

export const layer = Layer.effect(NewgitLifecycleReactor, make);
