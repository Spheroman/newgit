// Repo naming and the rules for who may hold a token for which repo.
//
// Every project shares one Artifacts namespace (a binding names exactly one),
// so a repo's name carries its owner, project, and role:
//
//   <owner>.<project>.t.<tracker>             a tracker's history: a code
//                                             tracker's, or a shared secret one
//   <owner>.<project>.t.<tracker>.<login>     one user's per-user secret tracker
//   <owner>.<project>.i.<instance>.<tracker>  an instance's fork of one code
//                                             tracker, made the first time the
//                                             instance changes it
//
// A project is `<owner>/<project>`, like a GitHub repository: the owner is
// the GitHub login that created it. `.` separates the parts, so no part may
// contain one. Pure functions only: everything here is decided from its
// arguments, so it can be tested exhaustively without a runtime.

/** A project, as `<owner>/<name>`. */
export interface ProjectId {
	owner: string;
	name: string;
}

export const projectKey = (project: ProjectId) => `${project.owner}/${project.name}`;

export function parseProjectKey(key: string): ProjectId | null {
	const [owner, name, ...rest] = key.split("/");
	if (rest.length > 0 || owner === undefined || name === undefined) return null;
	if (!PART.test(owner) || !PART.test(name)) return null;
	return { owner: owner.toLowerCase(), name };
}

export type RepoRef =
	| { kind: "tracker"; project: ProjectId; tracker: string; login?: string }
	| { kind: "fork"; project: ProjectId; instance: string; tracker: string };

const PART = /^[A-Za-z0-9][A-Za-z0-9_-]{0,62}$/;

/** Why `value` cannot be one part of a repo name, or null when it can. */
export function partProblem(what: string, value: string): string | null {
	if (PART.test(value)) return null;
	return `${what} \`${value}\` cannot name a repo on the remote: use letters, digits, \`_\` or \`-\` (no \`.\`), starting with a letter or digit, at most 63 characters`;
}

export function repoName(ref: RepoRef): string {
	const prefix = `${ref.project.owner}.${ref.project.name}`;
	switch (ref.kind) {
		case "tracker":
			return ref.login === undefined ? `${prefix}.t.${ref.tracker}` : `${prefix}.t.${ref.tracker}.${ref.login}`;
		case "fork":
			return `${prefix}.i.${ref.instance}.${ref.tracker}`;
	}
}

export function parseRepoName(name: string): RepoRef | null {
	const parts = name.split(".");
	if (parts.some((part) => !PART.test(part))) return null;
	const [owner, projectName, role, ...rest] = parts;
	if (owner === undefined || projectName === undefined) return null;
	const project = { owner: owner.toLowerCase(), name: projectName };
	if (role === "t" && rest.length === 1) return { kind: "tracker", project, tracker: rest[0] };
	if (role === "t" && rest.length === 2)
		return { kind: "tracker", project, tracker: rest[0], login: rest[1].toLowerCase() };
	if (role === "i" && rest.length === 2) return { kind: "fork", project, instance: rest[0], tracker: rest[1] };
	return null;
}

/** A tracker's `audience`, as committed in `.newgit/trackers/<name>.toml`. */
export type Audience =
	| { kind: "public" }
	| { kind: "project-devs" }
	/** Each user has their own content: one repo per user. */
	| { kind: "user" }
	/** Exactly one named user. */
	| { kind: "named"; login: string };

export function parseAudience(value: string): Audience | null {
	if (value === "public" || value === "project-devs" || value === "user") return { kind: value };
	if (value.startsWith("user:") && value.length > "user:".length)
		return { kind: "named", login: value.slice("user:".length).toLowerCase() };
	return null;
}

/**
 * A tracker's `maintainers`: who may merge changes to it. Defaults to its
 * audience; `owner` is the project's owner. Changing a tracker's *definition*
 * is changing the tracker that owns its `.newgit/trackers/<name>.toml`, so
 * layers of governance are just trackers owning other trackers' definitions.
 */
export type Maintainers =
	| { kind: "owner" }
	| { kind: "project-devs" }
	| { kind: "named"; login: string };

export function parseMaintainers(value: string): Maintainers | null {
	if (value === "owner") return { kind: "owner" };
	// Anyone who may read it, for the audiences where reading means a member.
	if (value === "project-devs" || value === "public" || value === "user") return { kind: "project-devs" };
	if (value.startsWith("user:") && value.length > "user:".length)
		return { kind: "named", login: value.slice("user:".length).toLowerCase() };
	return null;
}

/** May `who` merge changes to a tracker its definition says `maintainers` maintain? */
export function mayMaintain(maintainers: Maintainers, who: Requester, owner: string, tracker: string): Decision {
	if (!who.member) return deny(`${who.login} is not a member`);
	switch (maintainers.kind) {
		case "project-devs":
			return allow;
		case "owner":
			return who.login === owner ? allow : deny(`only ${owner}, the project's owner, merges \`${tracker}\``);
		case "named":
			return who.login === maintainers.login
				? allow
				: deny(`only ${maintainers.login} maintains \`${tracker}\``);
	}
}

export type Scope = "read" | "write";

export interface Requester {
	/** GitHub login, lowercased. Agents act as their owner, so this is the owner. */
	login: string;
	member: boolean;
}

export interface Facts {
	/** Owner of the instance, for forks; undefined when there is no such instance. */
	instanceOwner?: string;
	/** Whether that instance is private: only its owner reads its forks. */
	instancePrivate?: boolean;
	/**
	 * The tracker's definition on the project's default branch — its
	 * audience, and whether it is a code tracker (committed) or a secret one.
	 * Undefined when that branch does not define it.
	 */
	tracker?: { audience: Audience; code: boolean };
}

export type Decision = { ok: true } | { ok: false; reason: string };

const allow: Decision = { ok: true };
const deny = (reason: string): Decision => ({ ok: false, reason });

/** May `who` read content whose audience is `audience`? */
function mayRead(audience: Audience, who: Requester, project: string): Decision {
	switch (audience.kind) {
		case "public":
			return allow;
		case "project-devs":
		case "user":
			return who.member ? allow : deny(`${who.login} is not a member of ${project}`);
		case "named":
			if (!who.member) return deny(`${who.login} is not a member of ${project}`);
			return audience.login === who.login ? allow : deny(`this is for user:${audience.login} only`);
	}
}

/**
 * May `who` hold a `scope` token for `ref`? The table in newgit-trackers.md
 * (*On the remote*), as code.
 */
export function decide(ref: RepoRef, scope: Scope, who: Requester, facts: Facts): Decision {
	const name = repoName(ref);
	const project = projectKey(ref.project);
	const tracker = facts.tracker;
	if (tracker === undefined)
		return deny(`tracker \`${ref.tracker}\` is not defined on ${project}'s default branch`);

	switch (ref.kind) {
		case "fork": {
			if (!tracker.code) return deny(`\`${ref.tracker}\` is a secret tracker; instances do not fork it`);
			// A private instance is not there for anyone but its owner: the
			// same answer as no instance at all.
			if (facts.instanceOwner === undefined || (facts.instancePrivate && facts.instanceOwner !== who.login))
				return deny(`no instance \`${ref.instance}\` in ${project}`);
			const read = mayRead(tracker.audience, who, project);
			if (!read.ok) return read;
			if (scope === "write" && facts.instanceOwner !== who.login)
				return deny(
					`instance \`${ref.instance}\` belongs to ${facts.instanceOwner}; only its owner pushes to ${name}`,
				);
			return allow;
		}

		case "tracker": {
			const audience = tracker.audience;
			if (audience.kind === "user") {
				if (ref.login === undefined)
					return deny(`tracker \`${ref.tracker}\` is per-user; its content lives in ${name}.<login>`);
				if (!who.member) return deny(`${who.login} is not a member of ${project}`);
				if (ref.login !== who.login)
					return deny(`${name} is ${ref.login}'s own \`${ref.tracker}\`; each user reads only their own`);
				return allow;
			}
			if (ref.login !== undefined)
				return deny(`tracker \`${ref.tracker}\` is not per-user, so it has no per-user repos`);
			const read = mayRead(audience, who, project);
			if (!read.ok) return read;
			if (scope === "read") return allow;
			// A code tracker's history moves only through `newgit merge`; an
			// instance's work goes to its fork. A shared secret tracker is
			// written by its audience, as its content is captured.
			if (tracker.code)
				return deny(
					`nobody pushes to ${name}: a code tracker's history moves only through \`newgit merge\`; an instance pushes to its fork`,
				);
			if (audience.kind === "public" && !who.member) return deny(`${who.login} is not a member of ${project}`);
			return allow;
		}
	}
}
