// Secrets are not in wrangler.jsonc, so `wrangler types` cannot see them.
// Declared on both Env interfaces it generates: the Worker uses the global
// one, `cloudflare:workers`' `env` the namespaced one.
interface SecretEnv {
	/** Signs newgit tokens. Rotating it logs everyone out. */
	TOKEN_KEY: string;
	/**
	 * Who may use this remote: GitHub logins separated by commas or
	 * whitespace, or `*` for anyone. Unset admits no one. Checked on every
	 * request, so removing a login cuts off the tokens it already holds.
	 */
	ALLOWED_LOGINS?: string;
	/**
	 * Who may not, even when `ALLOWED_LOGINS` is `*`. The same format, and
	 * also checked on every request.
	 */
	BLOCKED_LOGINS?: string;
	/**
	 * The remote's operators: they may delete any project, and make
	 * projects past the cap. The same format.
	 */
	ADMIN_LOGINS?: string;
	/** How many projects one login may own. Unset is 5. */
	PROJECTS_PER_LOGIN?: string;
}
interface Env extends SecretEnv {}
declare namespace Cloudflare {
	interface Env extends SecretEnv {}
}
