// Repo naming and the rules for who may hold a token for which repo.
//
// Every project shares one Artifacts namespace (a binding names exactly one),
// so a repo's name carries its project and role:
//
//   <project>.src                  trunk
//   <project>.i.<instance>         one fork per branch instance
//   <project>.t.<tracker>          a shared tracker's lane
//   <project>.t.<tracker>.<login>  one user's lane of a per-user tracker
//
// `.` separates the parts, so no part may contain one. Pure functions only:
// everything here is decided from its arguments, so it can be tested
// exhaustively without a runtime.

export type RepoRef =
	| { kind: "src"; project: string }
	| { kind: "instance"; project: string; instance: string }
	| { kind: "tracker"; project: string; tracker: string; login?: string };

const PART = /^[A-Za-z0-9][A-Za-z0-9_-]{0,62}$/;

/** Why `value` cannot be one part of a repo name, or null when it can. */
export function partProblem(what: string, value: string): string | null {
	if (PART.test(value)) return null;
	return `${what} \`${value}\` cannot name a repo on the remote: use letters, digits, \`_\` or \`-\` (no \`.\`), starting with a letter or digit, at most 63 characters`;
}

export function repoName(ref: RepoRef): string {
	switch (ref.kind) {
		case "src":
			return `${ref.project}.src`;
		case "instance":
			return `${ref.project}.i.${ref.instance}`;
		case "tracker":
			return ref.login === undefined
				? `${ref.project}.t.${ref.tracker}`
				: `${ref.project}.t.${ref.tracker}.${ref.login}`;
	}
}

export function parseRepoName(name: string): RepoRef | null {
	const parts = name.split(".");
	if (parts.some((part) => !PART.test(part))) return null;
	const [project, role, ...rest] = parts;
	if (role === "src" && rest.length === 0) return { kind: "src", project };
	if (role === "i" && rest.length === 1) return { kind: "instance", project, instance: rest[0] };
	if (role === "t" && rest.length === 1) return { kind: "tracker", project, tracker: rest[0] };
	if (role === "t" && rest.length === 2)
		return { kind: "tracker", project, tracker: rest[0], login: rest[1].toLowerCase() };
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

export type Scope = "read" | "write";

export interface Requester {
	/** GitHub login, lowercased. Agents act as their owner, so this is the owner. */
	login: string;
	member: boolean;
}

export interface Facts {
	/** Owner of the instance, for instance repos; undefined when there is no such instance. */
	instanceOwner?: string;
	/** The tracker's audience on trunk, for tracker repos; undefined when trunk does not define it. */
	audience?: Audience;
}

export type Decision = { ok: true } | { ok: false; reason: string };

const allow: Decision = { ok: true };
const deny = (reason: string): Decision => ({ ok: false, reason });

/**
 * May `who` hold a `scope` token for `ref`? The table in newgit-cloudflare.md
 * (*Identity And Tokens*), as code.
 */
export function decide(ref: RepoRef, scope: Scope, who: Requester, facts: Facts): Decision {
	const name = repoName(ref);
	switch (ref.kind) {
		case "src":
			if (!who.member) return deny(`${who.login} is not a member of ${ref.project}`);
			if (scope === "write")
				return deny(`nobody pushes to trunk (${name}); trunk moves only through \`newgit merge\``);
			return allow;

		case "instance":
			if (!who.member) return deny(`${who.login} is not a member of ${ref.project}`);
			if (facts.instanceOwner === undefined) return deny(`no instance \`${ref.instance}\` in ${ref.project}`);
			if (scope === "write" && facts.instanceOwner !== who.login)
				return deny(`instance \`${ref.instance}\` belongs to ${facts.instanceOwner}; only its owner pushes to ${name}`);
			return allow;

		case "tracker": {
			const audience = facts.audience;
			if (audience === undefined)
				return deny(`tracker \`${ref.tracker}\` is not defined on ${ref.project}'s trunk`);
			if (audience.kind === "user") {
				if (ref.login === undefined)
					return deny(`tracker \`${ref.tracker}\` is per-user; its content lives in ${name}.<login>`);
				if (!who.member) return deny(`${who.login} is not a member of ${ref.project}`);
				if (ref.login !== who.login)
					return deny(`${name} is ${ref.login}'s own \`${ref.tracker}\`; each user reads only their own`);
				return allow;
			}
			if (ref.login !== undefined)
				return deny(`tracker \`${ref.tracker}\` is not per-user, so it has no per-user repos`);
			switch (audience.kind) {
				case "public":
					if (scope === "read") return allow;
					return who.member ? allow : deny(`${who.login} is not a member of ${ref.project}`);
				case "project-devs":
					return who.member ? allow : deny(`${who.login} is not a member of ${ref.project}`);
				case "named":
					if (!who.member) return deny(`${who.login} is not a member of ${ref.project}`);
					return audience.login === who.login
						? allow
						: deny(`tracker \`${ref.tracker}\` is for user:${audience.login} only`);
			}
		}
	}
}
