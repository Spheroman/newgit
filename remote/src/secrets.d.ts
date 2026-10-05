// Secrets are not in wrangler.jsonc, so `wrangler types` cannot see them.
// Declared on both Env interfaces it generates: the Worker uses the global
// one, `cloudflare:workers`' `env` the namespaced one.
interface SecretEnv {
	/** Signs newgit tokens. Rotating it logs everyone out. */
	TOKEN_KEY: string;
}
interface Env extends SecretEnv {}
declare namespace Cloudflare {
	interface Env extends SecretEnv {}
}
