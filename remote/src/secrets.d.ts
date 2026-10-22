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
}
interface Env extends SecretEnv {}
declare namespace Cloudflare {
	interface Env extends SecretEnv {}
}
